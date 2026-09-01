//! Enrichers: the step that turns "a window was focused" into "you were on
//! the login branch of spotted-android".
//!
//! Everything here reads only what the application already wrote to disk about
//! itself — a window title, a recent-projects list, a `.git/HEAD` — and never
//! the contents of the user's files.

use chronicle_core::model::{ArtifactKind, ArtifactObs};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::win::Sample;

/// Files and folders whose presence means "this directory is a project root".
static PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "pubspec.yaml",
    "pyproject.toml",
    "requirements.txt",
    "Gemfile",
    "composer.json",
    "CMakeLists.txt",
    "Makefile",
    ".idea",
    ".vscode",
];

/// Directories that must never be reported as a project root, however many
/// markers they happen to contain.
///
/// A home directory routinely picks up a `.vscode` or a stray `Makefile`, and
/// treating it as a project would file every folder on the machine under one
/// enormous fake project — and name every session after it.
fn is_too_broad(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return true; // a drive root
    }
    let home = std::env::var("USERPROFILE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from));

    match home {
        // The home directory itself, and anything at or above it.
        Some(h) => dir == h || h.starts_with(dir),
        None => false,
    }
}

/// Walk up from a file looking for a project root. Bounded, so a path on a
/// network share cannot turn into an unbounded directory walk.
pub fn project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    for _ in 0..24 {
        if is_too_broad(dir) {
            return None;
        }
        for marker in PROJECT_MARKERS {
            if dir.join(marker).exists() {
                return Some(dir.to_path_buf());
            }
        }
        dir = dir.parent()?;
    }
    None
}

/// The checked-out branch, read from `.git/HEAD`. Restore needs it, and it is
/// the second-strongest signal the sessioniser has after the root itself.
pub fn git_branch(root: &Path) -> Option<String> {
    let git = root.join(".git");
    let head = if git.is_dir() {
        git.join("HEAD")
    } else if git.is_file() {
        // A worktree or submodule: `.git` is a file pointing elsewhere.
        let content = std::fs::read_to_string(&git).ok()?;
        let target = content.strip_prefix("gitdir:")?.trim();
        PathBuf::from(target).join("HEAD")
    } else {
        return None;
    };
    let content = std::fs::read_to_string(head).ok()?;
    let content = content.trim();
    match content.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        // Detached head: a short sha is still more useful than nothing.
        None => content.get(..8).map(str::to_string),
    }
}

/// Canonical `file:///c:/work/proj` form. Two observations of the same path
/// must produce the same string, forever, or they become two artifacts.
pub fn path_uri(p: &Path) -> String {
    format!("file:///{}", normalised_path(p))
}

/// A path in the one spelling Chronicle stores, without any scheme.
///
/// Split out from [`path_uri`] because a terminal's directory needs the same
/// normalisation under a different scheme, and two copies of this would
/// eventually disagree about a drive letter.
pub fn normalised_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    // Normalise the drive letter so `C:` and `c:` never become two artifacts.
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let mut out = String::with_capacity(s.len());
        out.push((b[0] as char).to_ascii_lowercase());
        out.push_str(&s[1..]);
        out
    } else {
        s.to_string()
    }
}


pub fn base_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string_lossy().to_string())
}

/// Build a project artifact from a resolved directory, carrying the branch.
pub fn project_artifact(root: &Path) -> ArtifactObs {
    let mut a = ArtifactObs::new(ArtifactKind::Project, path_uri(root), base_name(root))
        .with_root(root.to_string_lossy().replace('\\', "/"));
    if let Some(branch) = git_branch(root) {
        a = a.with_state("branch", branch);
    }
    a
}

/// Build a file artifact, attaching the project root it belongs to.
pub fn file_artifact(path: &Path, kind: ArtifactKind) -> ArtifactObs {
    let mut a = ArtifactObs::new(kind, path_uri(path), base_name(path));
    if let Some(root) = project_root(path) {
        a = a.with_root(root.to_string_lossy().replace('\\', "/"));
    }
    a
}

// ── enricher plumbing ───────────────────────────────────────────────────

pub trait Enricher: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, s: &Sample) -> bool;
    /// Most specific artifact first — the store treats index 0 as focused.
    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs>;
}

/// The default set, in priority order.
pub fn default_enrichers() -> Vec<Box<dyn Enricher>> {
    vec![
        Box::new(crate::chrome::Chromium::new()),
        #[cfg(windows)]
        Box::new(crate::explorer::Explorer),
        #[cfg(windows)]
        Box::new(crate::terminal::Terminal),
        Box::new(VsCode),
        Box::new(JetBrains),
        Box::new(Documents),
        Box::new(PathsInTitle),
    ]
}

// ── VS Code ─────────────────────────────────────────────────────────────

/// Title is `{file} - {folder} - Visual Studio Code`. The folder name is only
/// a name, so it is matched against the paths VS Code itself recorded in its
/// window state, which is how a real path is recovered without an extension.
pub struct VsCode;

static VSCODE_IDS: &[&str] = &["code.exe", "code - insiders.exe", "cursor.exe", "windsurf.exe"];

impl Enricher for VsCode {
    fn name(&self) -> &'static str {
        "vscode"
    }

    fn matches(&self, s: &Sample) -> bool {
        VSCODE_IDS.contains(&s.app_id.as_str())
    }

    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs> {
        let Some((file, folder)) = parse_vscode_title(&s.title) else {
            return Vec::new();
        };
        let mut out = Vec::new();

        let resolved = folder
            .as_deref()
            .and_then(|name| resolve_from_recents(name, &vscode_recent_folders()));

        if let Some(root) = &resolved {
            if let Some(f) = &file {
                let full = root.join(f);
                if full.exists() {
                    out.push(file_artifact(&full, ArtifactKind::File));
                }
            }
            out.push(project_artifact(root));
        } else if let Some(name) = folder {
            // The path could not be recovered; the name is still worth having,
            // and restore will report it as "needs you" rather than pretend.
            out.push(ArtifactObs::new(
                ArtifactKind::Project,
                format!("project://vscode/{name}"),
                name,
            ));
        }
        out
    }
}

/// `● LoginViewModel.kt - spotted-android - Visual Studio Code`
fn parse_vscode_title(title: &str) -> Option<(Option<String>, Option<String>)> {
    let t = title.trim().trim_start_matches(['●', '•', '*']).trim();
    let mut parts: Vec<&str> = t.split(" - ").map(str::trim).collect();
    let last = parts.pop()?;
    if !last.contains("Visual Studio Code") && !last.contains("Cursor") && !last.contains("Windsurf")
    {
        return None;
    }
    // Trailing "[Administrator]" and workspace suffixes are noise.
    parts.retain(|p| !p.is_empty() && !p.starts_with('['));
    match parts.len() {
        0 => None,
        1 => {
            let only = parts[0].to_string();
            if Path::new(&only).extension().is_some() {
                Some((Some(only), None))
            } else {
                Some((None, Some(only)))
            }
        }
        _ => Some((
            Some(parts[0].to_string()),
            Some(parts[parts.len() - 1].to_string()),
        )),
    }
}

/// Every `file:///` URI VS Code has written into its own window state.
fn vscode_recent_folders() -> Vec<PathBuf> {
    let Ok(appdata) = std::env::var("APPDATA") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for product in ["Code", "Code - Insiders", "Cursor", "Windsurf"] {
        let p = PathBuf::from(&appdata)
            .join(product)
            .join("User")
            .join("globalStorage")
            .join("storage.json");
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        out.extend(file_uris_in(&text));
    }
    out
}

/// Pull every `file:///…` out of a blob of JSON without depending on its
/// schema, which VS Code changes between releases.
fn file_uris_in(text: &str) -> Vec<PathBuf> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"file:///([A-Za-z]%3A|[A-Za-z]:)[^"\\]*"#).unwrap());
    RE.find_iter(text)
        .filter_map(|m| decode_file_uri(m.as_str()))
        .collect()
}

fn decode_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file:///")?;
    let decoded = rest
        .replace("%3A", ":")
        .replace("%3a", ":")
        .replace("%20", " ")
        .replace('/', "\\");
    Some(PathBuf::from(decoded))
}

/// Match a folder *name* from a window title against known full paths.
/// Prefers a path that still exists; ties go to the shortest, which is almost
/// always the real project rather than a nested copy.
fn resolve_from_recents(name: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    let mut hits: Vec<&PathBuf> = candidates
        .iter()
        .filter(|p| p.file_name().is_some_and(|f| f.eq_ignore_ascii_case(name)))
        .collect();
    hits.sort_by_key(|p| (!p.exists(), p.as_os_str().len()));
    hits.first().map(|p| (*p).clone())
}

// ── JetBrains: Android Studio, IntelliJ, and the rest ───────────────────

pub struct JetBrains;

static JETBRAINS_IDS: &[&str] = &[
    "studio64.exe",
    "idea64.exe",
    "pycharm64.exe",
    "webstorm64.exe",
    "clion64.exe",
    "rider64.exe",
    "goland64.exe",
    "phpstorm64.exe",
    "rubymine64.exe",
    "datagrip64.exe",
];

impl Enricher for JetBrains {
    fn name(&self) -> &'static str {
        "jetbrains"
    }

    fn matches(&self, s: &Sample) -> bool {
        JETBRAINS_IDS.contains(&s.app_id.as_str())
    }

    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs> {
        // `spotted-android – LoginViewModel.kt` (note the en dash)
        let Some(project) = s
            .title
            .split(['–', '-'])
            .next()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
        else {
            return Vec::new();
        };

        match resolve_from_recents(&project, &jetbrains_recent_projects()) {
            Some(root) => vec![project_artifact(&root)],
            None => vec![ArtifactObs::new(
                ArtifactKind::Project,
                format!("project://jetbrains/{project}"),
                project,
            )],
        }
    }
}

/// JetBrains IDEs keep `recentProjects.xml` under their per-product config.
fn jetbrains_recent_projects() -> Vec<PathBuf> {
    let Ok(appdata) = std::env::var("APPDATA") else {
        return Vec::new();
    };
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let mut out = Vec::new();

    for vendor in ["JetBrains", "Google"] {
        let base = PathBuf::from(&appdata).join(vendor);
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for e in entries.flatten() {
            let xml = e.path().join("options").join("recentProjects.xml");
            let Ok(text) = std::fs::read_to_string(&xml) else {
                continue;
            };
            static RE: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r#"key="([^"]+)""#).unwrap());
            for c in RE.captures_iter(&text) {
                let raw = c[1].replace("$USER_HOME$", &home);
                let p = PathBuf::from(raw.replace('/', "\\"));
                if p.is_absolute() {
                    out.push(p);
                }
            }
        }
    }
    out
}

// ── PDFs and office documents ───────────────────────────────────────────

pub struct Documents;

static DOC_IDS: &[&str] = &[
    "acrobat.exe",
    "acrord32.exe",
    "sumatrapdf.exe",
    "foxitpdfreader.exe",
    "winword.exe",
    "excel.exe",
    "powerpnt.exe",
];

impl Enricher for Documents {
    fn name(&self) -> &'static str {
        "documents"
    }

    fn matches(&self, s: &Sample) -> bool {
        DOC_IDS.contains(&s.app_id.as_str())
    }

    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs> {
        // Titles are usually `report.pdf - Adobe Acrobat` or just `report.pdf`.
        let Some(name) = s
            .title
            .split(" - ")
            .find(|p| {
                let lower = p.trim().to_ascii_lowercase();
                [".pdf", ".docx", ".doc", ".xlsx", ".pptx", ".csv"]
                    .iter()
                    .any(|ext| lower.ends_with(ext))
            })
            .map(str::trim)
        else {
            return Vec::new();
        };

        vec![ArtifactObs::new(
            ArtifactKind::Document,
            format!("document://{}", name.to_ascii_lowercase()),
            name,
        )]
    }
}

// ── Anything with a real path in its title ──────────────────────────────

pub struct PathsInTitle;

impl Enricher for PathsInTitle {
    fn name(&self) -> &'static str {
        "paths-in-title"
    }

    fn matches(&self, _s: &Sample) -> bool {
        true
    }

    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs> {
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"[A-Za-z]:[\\/][^\\/:*?"<>|\r\n]+(?:[\\/][^\\/:*?"<>|\r\n]+)*"#).unwrap()
        });
        RE.find_iter(&s.title)
            .filter_map(|m| {
                let p = PathBuf::from(m.as_str());
                if !p.exists() {
                    return None;
                }
                Some(if p.is_dir() {
                    match project_root(&p) {
                        Some(root) if root == p => project_artifact(&p),
                        _ => file_artifact(&p, ArtifactKind::Directory),
                    }
                } else {
                    file_artifact(&p, ArtifactKind::File)
                })
            })
            .take(3)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_vscode_title_with_file_and_folder() {
        let r = parse_vscode_title("● LoginViewModel.kt - spotted-android - Visual Studio Code");
        assert_eq!(
            r,
            Some((Some("LoginViewModel.kt".into()), Some("spotted-android".into())))
        );
    }

    #[test]
    fn parses_a_vscode_title_with_only_a_folder() {
        let r = parse_vscode_title("spotted-android - Visual Studio Code");
        assert_eq!(r, Some((None, Some("spotted-android".into()))));
    }

    #[test]
    fn ignores_a_window_that_is_not_vscode() {
        assert_eq!(parse_vscode_title("Inbox - Outlook"), None);
    }

    /// A `.vscode` folder in a home directory used to make every folder on the
    /// machine look like it belonged to one giant project.
    #[test]
    fn the_home_directory_is_never_a_project_root() {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .expect("a home directory");
        let home = PathBuf::from(home);
        assert_eq!(project_root(&home), None);
        assert_eq!(project_root(&home.join("Downloads")), None);
    }

    #[test]
    fn a_drive_root_is_never_a_project_root() {
        assert_eq!(project_root(Path::new("C:/")), None);
    }

    #[test]
    fn path_uris_are_stable_across_drive_letter_case() {
        assert_eq!(
            path_uri(Path::new(r"C:\work\Proj")),
            path_uri(Path::new(r"c:/work/Proj"))
        );
        assert_eq!(path_uri(Path::new(r"C:\work\Proj")), "file:///c:/work/Proj");
    }

    #[test]
    fn extracts_file_uris_from_vscode_storage_json() {
        let json = r#"{"windowsState":{"openedWindows":[{"folder":"file:///c%3A/work/spotted-android"}]}}"#;
        let found = file_uris_in(json);
        assert_eq!(found, vec![PathBuf::from(r"c:\work\spotted-android")]);
    }

    /// Paths built by joining, never by writing separators into a literal.
    ///
    /// `resolve_from_recents` splits a path into components to read its last
    /// one, and only Windows treats a backslash as a separator — so a literal
    /// `r"c:\work\proj"` is three components on Windows and one everywhere
    /// else, which is how this test used to pass here and fail on Linux.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("chronicle-{name}"))
    }

    #[test]
    fn a_path_that_still_exists_wins_even_when_it_is_longer() {
        let base = scratch("recents-existing");
        let real = base.join("work").join("nested").join("proj");
        std::fs::create_dir_all(&real).expect("creating the scratch directory");

        // Shorter, but not there any more.
        let gone = base.join("old").join("proj");
        let hit = resolve_from_recents("proj", &[gone, real.clone()]);

        std::fs::remove_dir_all(&base).ok();
        assert_eq!(hit, Some(real));
    }

    #[test]
    fn between_two_paths_that_are_both_gone_the_shortest_wins() {
        // A nested copy under `backup` is almost never the project itself.
        let base = scratch("recents-missing");
        let deep = base.join("backup").join("old").join("proj");
        let shallow = base.join("work").join("proj");

        assert_eq!(
            resolve_from_recents("proj", &[deep, shallow.clone()]),
            Some(shallow)
        );
    }

    #[test]
    fn a_name_that_matches_nothing_resolves_to_nothing() {
        let base = scratch("recents-none");
        assert_eq!(resolve_from_recents("proj", &[base.join("work").join("other")]), None);
    }
}

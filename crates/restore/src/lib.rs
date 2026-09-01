//! The restore engine: the only part of Chronicle that acts on the system
//! rather than watching it.
//!
//! Planning is separated from execution on purpose. A plan is inspectable,
//! testable without launching anything, and shown to the user before a single
//! process starts — which is what makes the fidelity badge on every row
//! honest rather than decorative.

use chronicle_core::model::{ArtifactId, ArtifactKind, Category, SessionArtifact, SessionDetail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How faithfully one item can come back. The operating system decides this,
/// not Chronicle; the job here is to reach the best rung each app allows and
/// then say plainly which one it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Reopens in the state it was left.
    Exact,
    /// Opens straight to the artifact, without the finer state.
    DeepLink,
    /// The file opens; position is not recoverable.
    Open,
    /// Chronicle cannot act alone.
    NeedsYou,
    /// The thing is gone.
    Missing,
}

impl Fidelity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "Exact",
            Self::DeepLink => "Deep link",
            Self::Open => "Open",
            Self::NeedsYou => "Needs you",
            Self::Missing => "Missing",
        }
    }

    /// Whether Chronicle can do this one by itself.
    pub fn actionable(self) -> bool {
        matches!(self, Self::Exact | Self::DeepLink | Self::Open)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Run a specific executable. Chronicle uses the binary it actually
    /// observed, so there is no searching PATH and hoping.
    Launch { program: PathBuf, args: Vec<String> },
    /// Hand the path to the shell and let Windows pick the handler.
    Shell { path: PathBuf },
    /// Something only the user can do. Shown with a copy button.
    Manual { instruction: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    /// Stable within a plan, so the UI can check and uncheck rows.
    pub key: String,
    pub label: String,
    pub detail: String,
    pub app_name: String,
    pub category: Category,
    pub fidelity: Fidelity,
    pub action: Action,
    /// Why this item is not `Exact`, when it is not.
    pub note: Option<String>,
    pub selected: bool,
    pub artifact_ids: Vec<ArtifactId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub session_id: i64,
    pub session_title: String,
    pub items: Vec<PlanItem>,
}

impl Plan {
    /// "12 of 14 items ready · 2 need you"
    pub fn readiness(&self) -> (usize, usize) {
        let ready = self.items.iter().filter(|i| i.fidelity.actionable()).count();
        (ready, self.items.len())
    }
}

/// Build a restore plan. Touches the filesystem to check existence, but starts
/// nothing.
pub fn plan(detail: &SessionDetail) -> Plan {
    let mut items: Vec<PlanItem> = Vec::new();

    // Browser tabs collapse into one window per browser, in the order they were
    // seen. Without an extension this loses pinning and tab groups — the tab
    // set and its order are what survive.
    let mut urls_by_app: BTreeMap<String, Vec<&SessionArtifact>> = BTreeMap::new();
    let mut dirs_by_terminal: BTreeMap<String, Vec<&SessionArtifact>> = BTreeMap::new();

    for a in &detail.artifacts {
        match a.kind {
            ArtifactKind::Url => {
                urls_by_app.entry(a.app_id.clone()).or_default().push(a);
                continue;
            }
            ArtifactKind::Terminal if TABBED_TERMINALS.contains(&a.app_id.as_str()) => {
                dirs_by_terminal.entry(a.app_id.clone()).or_default().push(a);
                continue;
            }
            ArtifactKind::App => continue, // handled after, only if nothing better
            _ => {}
        }
        items.push(item_for(a));
    }

    for (app_id, tabs) in urls_by_app {
        items.push(browser_item(&app_id, &tabs));
    }

    // Four directories were four tabs of one window, not four windows. A
    // terminal that can be told to open tabs is told to.
    for (app_id, dirs) in dirs_by_terminal {
        match dirs.len() {
            1 => items.push(item_for(dirs[0])),
            _ => items.push(terminal_item(&app_id, &dirs)),
        }
    }

    // A bare app is worth restoring only when nothing more specific covers it.
    let covered: std::collections::HashSet<&str> =
        detail.artifacts.iter().filter(|a| a.kind != ArtifactKind::App).map(|a| a.app_id.as_str()).collect();
    for a in detail.artifacts.iter().filter(|a| a.kind == ArtifactKind::App) {
        if !covered.contains(a.app_id.as_str()) {
            items.push(item_for(a));
        }
    }

    // Editors and terminals first, browsers last.
    items.sort_by_key(|i| (i.category.launch_order(), i.label.clone()));

    Plan {
        session_id: detail.session.id,
        session_title: detail.session.title.clone(),
        items,
    }
}

fn item_for(a: &SessionArtifact) -> PlanItem {
    let exe = a.app_exe.as_ref().map(PathBuf::from).filter(|p| p.exists());
    let path = local_path(&a.uri);

    let (fidelity, action, note) = match (&path, &exe) {
        // The path is gone. Say so, and offer what was last known.
        (Some(p), _) if !p.exists() => (
            Fidelity::Missing,
            Action::Manual {
                instruction: p.display().to_string(),
            },
            Some(format!(
                "moved or deleted since the session — last seen {}",
                a.last_seen.format("%-d %b, %H:%M")
            )),
        ),

        // A real path and the exact binary that had it open.
        (Some(p), Some(e)) => {
            let (f, args) = launch_recipe(&a.app_id, p, a);
            (
                f,
                Action::Launch {
                    program: e.clone(),
                    args,
                },
                None,
            )
        }

        // A real path but the app is not installed here any more.
        (Some(p), None) => (
            Fidelity::Open,
            Action::Shell { path: p.clone() },
            Some(format!("{} was not found; opening with the default handler", a.app_name)),
        ),

        // No path at all — a project name we could not resolve, or a document
        // known only by its title.
        (None, _) => (
            Fidelity::NeedsYou,
            Action::Manual {
                instruction: a.display_name.clone(),
            },
            Some("Chronicle recorded the name but never resolved a path".into()),
        ),
    };

    PlanItem {
        key: format!("a{}", a.artifact_id),
        label: a.display_name.clone(),
        detail: detail_line(a),
        app_name: a.app_name.clone(),
        category: a.category,
        selected: fidelity.actionable(),
        fidelity,
        action,
        note,
        artifact_ids: vec![a.artifact_id],
    }
}

/// Per-application launch arguments. This is the table that grows as adapters
/// are added; everything else in the planner stays the same.
fn launch_recipe(app_id: &str, path: &std::path::Path, a: &SessionArtifact) -> (Fidelity, Vec<String>) {
    let p = path.display().to_string();
    match app_id {
        // VS Code and its forks restore their own editor state for a folder.
        "code.exe" | "code - insiders.exe" | "cursor.exe" | "windsurf.exe" => {
            let mut args = vec![p];
            if let (Some(file), Some(line)) = (a.state_of("file"), a.state_of("line")) {
                args.push("-g".into());
                args.push(format!("{file}:{line}"));
            }
            (Fidelity::Exact, args)
        }

        // JetBrains launchers take the project directory.
        id if id.ends_with("64.exe") => (Fidelity::Exact, vec![p]),

        // Terminals open in the directory. The last command is never run —
        // the machine cannot tell a build from a deploy.
        "windowsterminal.exe" | "wt.exe" => (Fidelity::DeepLink, vec!["-d".into(), p]),

        "explorer.exe" => (Fidelity::Open, vec![p]),

        _ => (Fidelity::Open, vec![p]),
    }
}

/// Terminals that take a tab list on the command line. Everything else gets one
/// window per directory, because inventing a syntax for it would be guessing.
const TABBED_TERMINALS: &[&str] = &["windowsterminal.exe", "wt.exe"];

/// One terminal window with a tab per directory.
///
/// `wt -d A ; new-tab -d B` is Windows Terminal's own syntax for this, with the
/// semicolons as separate arguments. Nothing that was typed at those prompts is
/// replayed: Chronicle cannot tell a build from a deploy, and a session it
/// half-remembers is not licence to run a command the user did not ask for.
fn terminal_item(app_id: &str, dirs: &[&SessionArtifact]) -> PlanItem {
    let first = dirs[0];
    let exe = first.app_exe.as_ref().map(PathBuf::from).filter(|p| p.exists());
    let paths: Vec<PathBuf> = dirs.iter().filter_map(|d| local_path(&d.uri)).collect();
    let live: Vec<&PathBuf> = paths.iter().filter(|p| p.exists()).collect();

    let (fidelity, action, note) = match (&exe, live.is_empty()) {
        (Some(program), false) => {
            let mut args: Vec<String> = Vec::new();
            for (i, p) in live.iter().enumerate() {
                if i > 0 {
                    args.push(";".into());
                    args.push("new-tab".into());
                }
                args.push("-d".into());
                args.push(p.display().to_string());
            }
            let missing = paths.len() - live.len();
            let note = (missing > 0).then(|| {
                format!("{missing} of the directories no longer exist and are left out")
            });
            (
                Fidelity::DeepLink,
                Action::Launch {
                    program: program.clone(),
                    args,
                },
                note.or_else(|| Some("a tab per directory; nothing typed at the prompts is re-run".into())),
            )
        }
        (None, _) => (
            Fidelity::NeedsYou,
            Action::Manual {
                instruction: paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("
"),
            },
            Some(format!("{} was not found on this machine", first.app_name)),
        ),
        (_, true) => (
            Fidelity::Missing,
            Action::Manual {
                instruction: paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("
"),
            },
            Some("none of these directories exist any more".into()),
        ),
    };

    PlanItem {
        key: format!("terminal:{app_id}"),
        label: format!("{} — {} tab{}", first.app_name, dirs.len(), if dirs.len() == 1 { "" } else { "s" }),
        detail: dirs.iter().take(4).map(|d| d.display_name.clone()).collect::<Vec<_>>().join("
"),
        app_name: first.app_name.clone(),
        category: Category::Terminal,
        selected: fidelity.actionable(),
        fidelity,
        action,
        note,
        artifact_ids: dirs.iter().map(|d| d.artifact_id).collect(),
    }
}

fn browser_item(app_id: &str, tabs: &[&SessionArtifact]) -> PlanItem {
    let first = tabs[0];
    let exe = first.app_exe.as_ref().map(PathBuf::from).filter(|p| p.exists());
    let urls: Vec<String> = tabs.iter().map(|t| t.uri.clone()).collect();

    let (fidelity, action, note) = match exe {
        Some(program) => {
            let mut args = vec!["--new-window".to_string()];
            args.extend(urls.clone());
            (
                Fidelity::DeepLink,
                Action::Launch { program, args },
                Some("tab order kept; pinning and tab groups are not recoverable without an extension".into()),
            )
        }
        None => (
            Fidelity::NeedsYou,
            Action::Manual {
                instruction: urls.join("\n"),
            },
            Some(format!("{} was not found on this machine", first.app_name)),
        ),
    };

    PlanItem {
        key: format!("browser:{app_id}"),
        label: format!("{} — {} tab{}", first.app_name, tabs.len(), if tabs.len() == 1 { "" } else { "s" }),
        detail: tabs
            .iter()
            .take(4)
            .map(|t| t.display_name.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        app_name: first.app_name.clone(),
        category: Category::Browser,
        selected: fidelity.actionable(),
        fidelity,
        action,
        note,
        artifact_ids: tabs.iter().map(|t| t.artifact_id).collect(),
    }
}

fn detail_line(a: &SessionArtifact) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(root) = &a.project_root {
        parts.push(root.clone());
    }
    for (k, v) in &a.state {
        parts.push(format!("{k} {v}"));
    }
    if parts.is_empty() {
        a.uri.clone()
    } else {
        parts.join(" · ")
    }
}

/// `file:///c:/work/proj` back to a real path.
pub fn local_path(uri: &str) -> Option<PathBuf> {
    // `terminal://` is the same path under a different scheme: a directory a
    // prompt was sitting in, kept separate so that a folder seen by both
    // Explorer and a shell does not become one artifact that can only be
    // restored one of the two ways.
    let rest = uri
        .strip_prefix("file:///")
        .or_else(|| uri.strip_prefix("terminal://"))?;
    Some(PathBuf::from(rest.replace('/', "\\")))
}

// ── execution ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Build and report the plan without starting anything.
    pub dry_run: bool,
    /// Pause between categories so a slow editor is not racing a browser.
    pub stagger_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemOutcome {
    pub key: String,
    pub label: String,
    pub fidelity: Fidelity,
    pub ok: bool,
    pub message: String,
    /// Recorded so that Undo can later close exactly what Chronicle opened.
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub session_id: i64,
    pub items: Vec<ItemOutcome>,
    pub elapsed_ms: u128,
}

impl Outcome {
    pub fn restored(&self) -> usize {
        self.items.iter().filter(|i| i.ok).count()
    }
}

/// Run a plan. Only selected, actionable items start anything; the rest are
/// reported so the receipt tells the whole truth.
pub fn execute(plan: &Plan, opts: &RestoreOptions) -> Outcome {
    let start = std::time::Instant::now();
    let mut items = Vec::with_capacity(plan.items.len());
    let mut last_category: Option<Category> = None;

    for item in &plan.items {
        if !item.selected {
            items.push(ItemOutcome {
                key: item.key.clone(),
                label: item.label.clone(),
                fidelity: item.fidelity,
                ok: false,
                message: "skipped".into(),
                pid: None,
            });
            continue;
        }

        if let Action::Manual { instruction } = &item.action {
            items.push(ItemOutcome {
                key: item.key.clone(),
                label: item.label.clone(),
                fidelity: item.fidelity,
                ok: false,
                message: instruction.clone(),
                pid: None,
            });
            continue;
        }

        if last_category.is_some_and(|c| c != item.category) && opts.stagger_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(opts.stagger_ms));
        }
        last_category = Some(item.category);

        items.push(run_action(item, opts.dry_run));
    }

    Outcome {
        session_id: plan.session_id,
        items,
        elapsed_ms: start.elapsed().as_millis(),
    }
}

fn run_action(item: &PlanItem, dry_run: bool) -> ItemOutcome {
    let mut out = ItemOutcome {
        key: item.key.clone(),
        label: item.label.clone(),
        fidelity: item.fidelity,
        ok: false,
        message: String::new(),
        pid: None,
    };

    let mut cmd = match &item.action {
        Action::Launch { program, args } => {
            let mut c = std::process::Command::new(program);
            c.args(args);
            out.message = format!("{} {}", program.display(), args.join(" "));
            c
        }
        Action::Shell { path } => {
            // `cmd /c start` hands the path to the registered handler without
            // holding a console window open.
            let mut c = std::process::Command::new("cmd");
            c.args(["/c", "start", "", &path.display().to_string()]);
            out.message = format!("open {}", path.display());
            c
        }
        Action::Manual { .. } => unreachable!("manual actions are filtered above"),
    };

    if dry_run {
        out.ok = true;
        out.message = format!("[dry run] {}", out.message);
        return out;
    }

    match cmd.spawn() {
        Ok(child) => {
            out.pid = Some(child.id());
            out.ok = true;
        }
        Err(e) => {
            out.ok = false;
            out.message = format!("{}: {e}", out.message);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use chronicle_core::model::{EndReason, Session, TitleSource};

    fn artifact(
        id: ArtifactId,
        kind: ArtifactKind,
        uri: &str,
        app: &str,
        cat: Category,
        exe: Option<&str>,
    ) -> SessionArtifact {
        SessionArtifact {
            artifact_id: id,
            kind,
            category: cat,
            uri: uri.into(),
            display_name: format!("artifact{id}"),
            app_id: app.into(),
            app_name: app.into(),
            app_exe: exe.map(str::to_string),
            project_root: None,
            focus_seconds: 60,
            first_seen: Utc::now(),
            last_seen: Utc::now(),
            state: Vec::new(),
        }
    }

    /// A directory that certainly exists on any machine running these tests.
    fn a_real_dir() -> String {
        std::env::temp_dir().to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn several_terminal_directories_become_one_window_of_tabs() {
        let dir = a_real_dir();
        let exe = std::env::current_exe().unwrap();
        let d = detail(vec![
            artifact(1, ArtifactKind::Terminal, &format!("terminal://{dir}"), "windowsterminal.exe",
                     Category::Terminal, exe.to_str()),
            artifact(2, ArtifactKind::Terminal, &format!("terminal://{dir}/."), "windowsterminal.exe",
                     Category::Terminal, exe.to_str()),
        ]);
        let p = plan(&d);
        let term: Vec<_> = p.items.iter().filter(|i| i.category == Category::Terminal).collect();
        assert_eq!(term.len(), 1, "two directories must not become two windows");

        let Action::Launch { args, .. } = &term[0].action else {
            panic!("expected a launch, got {:?}", term[0].action);
        };
        // `wt -d A ; new-tab -d B`
        assert_eq!(args.iter().filter(|a| *a == "-d").count(), 2);
        assert!(args.contains(&";".to_string()));
        assert!(args.contains(&"new-tab".to_string()));
        assert_eq!(term[0].fidelity, Fidelity::DeepLink);
    }

    #[test]
    fn a_single_terminal_directory_stays_a_plain_item() {
        let dir = a_real_dir();
        let exe = std::env::current_exe().unwrap();
        let d = detail(vec![artifact(
            1, ArtifactKind::Terminal, &format!("terminal://{dir}"), "windowsterminal.exe",
            Category::Terminal, exe.to_str(),
        )]);
        let p = plan(&d);
        let term: Vec<_> = p.items.iter().filter(|i| i.category == Category::Terminal).collect();
        assert_eq!(term.len(), 1);
        let Action::Launch { args, .. } = &term[0].action else {
            panic!("expected a launch");
        };
        assert!(!args.contains(&"new-tab".to_string()), "one tab needs no tab syntax");
    }

    #[test]
    fn a_terminal_directory_and_the_same_folder_in_explorer_stay_separate() {
        // The two URIs differ by scheme precisely so that one folder can be
        // both "a prompt was here" and "a window was showing this".
        let dir = a_real_dir();
        let exe = std::env::current_exe().unwrap();
        let d = detail(vec![
            artifact(1, ArtifactKind::Terminal, &format!("terminal://{dir}"), "windowsterminal.exe",
                     Category::Terminal, exe.to_str()),
            artifact(2, ArtifactKind::Directory, &format!("file:///{dir}"), "explorer.exe",
                     Category::Folder, exe.to_str()),
        ]);
        let p = plan(&d);
        assert_eq!(p.items.len(), 2);
        assert_eq!(local_path(&format!("terminal://{dir}")), local_path(&format!("file:///{dir}")));
    }

    fn detail(artifacts: Vec<SessionArtifact>) -> SessionDetail {
        SessionDetail {
            session: Session {
                id: 1,
                title: "Test".into(),
                title_source: TitleSource::ProjectRoot,
                started_at: Utc::now(),
                ended_at: None,
                active_seconds: 0,
                end_reason: EndReason::Idle,
                pinned: false,
                device_id: "d".into(),
            },
            artifacts,
        }
    }

    #[test]
    fn a_vanished_file_is_reported_missing_not_launched() {
        let d = detail(vec![artifact(
            1,
            ArtifactKind::File,
            "file:///c:/nope/gone.mov",
            "code.exe",
            Category::Editor,
            None,
        )]);
        let p = plan(&d);
        assert_eq!(p.items[0].fidelity, Fidelity::Missing);
        assert!(!p.items[0].selected, "missing items must not be selected");
    }

    #[test]
    fn tabs_collapse_into_one_browser_window_in_order() {
        let d = detail(vec![
            artifact(1, ArtifactKind::Url, "https://a.example/1", "chrome.exe", Category::Browser, None),
            artifact(2, ArtifactKind::Url, "https://b.example/2", "chrome.exe", Category::Browser, None),
            artifact(3, ArtifactKind::Url, "https://c.example/3", "chrome.exe", Category::Browser, None),
        ]);
        let p = plan(&d);
        assert_eq!(p.items.len(), 1, "three tabs are one window, not three items");
        assert_eq!(p.items[0].artifact_ids.len(), 3);
        assert!(p.items[0].label.contains("3 tabs"));
    }

    #[test]
    fn editors_are_launched_before_browsers() {
        let d = detail(vec![
            artifact(1, ArtifactKind::Url, "https://a.example/1", "chrome.exe", Category::Browser, None),
            artifact(2, ArtifactKind::App, "app://code.exe", "code.exe", Category::Editor, None),
        ]);
        let p = plan(&d);
        assert_eq!(p.items[0].category, Category::Editor);
        assert_eq!(p.items[1].category, Category::Browser);
    }

    #[test]
    fn a_bare_app_is_dropped_when_something_specific_covers_it() {
        let d = detail(vec![
            artifact(1, ArtifactKind::App, "app://code.exe", "code.exe", Category::Editor, None),
            artifact(2, ArtifactKind::Project, "file:///c:/work", "code.exe", Category::Editor, None),
        ]);
        let p = plan(&d);
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].artifact_ids, vec![2]);
    }

    #[test]
    fn an_unresolved_project_name_asks_the_user_rather_than_guessing() {
        let d = detail(vec![artifact(
            1,
            ArtifactKind::Project,
            "project://vscode/spotted-android",
            "code.exe",
            Category::Editor,
            None,
        )]);
        let p = plan(&d);
        assert_eq!(p.items[0].fidelity, Fidelity::NeedsYou);
    }

    #[test]
    fn a_dry_run_starts_nothing_and_still_reports() {
        let d = detail(vec![artifact(
            1,
            ArtifactKind::Url,
            "https://a.example/1",
            "chrome.exe",
            Category::Browser,
            None,
        )]);
        let p = plan(&d);
        let o = execute(&p, &RestoreOptions { dry_run: true, stagger_ms: 0 });
        assert_eq!(o.items.len(), 1);
        assert!(o.items.iter().all(|i| i.pid.is_none()));
    }

    #[test]
    fn readiness_counts_only_what_chronicle_can_do_alone() {
        let d = detail(vec![
            artifact(1, ArtifactKind::File, "file:///c:/nope/gone.mov", "code.exe", Category::Editor, None),
            artifact(2, ArtifactKind::Project, "project://vscode/x", "code.exe", Category::Editor, None),
            artifact(3, ArtifactKind::Url, "https://a.example/1", "chrome.exe", Category::Browser, None),
        ]);
        let p = plan(&d);
        let (ready, total) = p.readiness();
        assert_eq!(total, 3);
        assert_eq!(ready, 0, "no exe on disk in the test fixture");
    }
}

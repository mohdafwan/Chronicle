//! Per-application capture policy, and the catalogue that turns an executable
//! name into something a human recognises.
//!
//! Three states per app, as designed: `Full`, `TitlesOff`, `Ignore`. The
//! shipped deny list is deliberately conservative — password managers, system
//! credential prompts, and messaging apps are off before the user ever opens
//! settings, because the first run has to be safe without configuration.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::model::Category;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapturePolicy {
    /// Title, artifacts and restorable state.
    #[default]
    Full,
    /// The app was open and for how long. The title is stored as the app name.
    TitlesOff,
    /// Nothing at all. Not even that it ran.
    Ignore,
}

/// One row in Settings → Sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppEntry {
    pub app_id: String,
    pub display_name: String,
    pub category: Category,
    pub policy: CapturePolicy,
    /// True when this app was placed on the deny list automatically. Shown in
    /// the UI as "added automatically" so the user knows it was not their doing.
    pub auto_denied: bool,
}

/// The catalogue: exe name → (pretty name, category, default policy).
struct Known {
    id: &'static str,
    name: &'static str,
    category: Category,
    default_policy: CapturePolicy,
}

const fn k(id: &'static str, name: &'static str, category: Category) -> Known {
    Known { id, name, category, default_policy: CapturePolicy::Full }
}

const fn denied(id: &'static str, name: &'static str, category: Category) -> Known {
    Known { id, name, category, default_policy: CapturePolicy::Ignore }
}

#[rustfmt::skip]
static CATALOGUE: &[Known] = &[
    // ── Editors and IDEs ────────────────────────────────────────────────
    k("code.exe",              "Visual Studio Code",      Category::Editor),
    k("code - insiders.exe",   "VS Code Insiders",        Category::Editor),
    k("cursor.exe",            "Cursor",                  Category::Editor),
    k("windsurf.exe",          "Windsurf",                Category::Editor),
    k("zed.exe",               "Zed",                     Category::Editor),
    k("sublime_text.exe",      "Sublime Text",            Category::Editor),
    k("studio64.exe",          "Android Studio",          Category::Editor),
    k("idea64.exe",            "IntelliJ IDEA",           Category::Editor),
    k("pycharm64.exe",         "PyCharm",                 Category::Editor),
    k("webstorm64.exe",        "WebStorm",                Category::Editor),
    k("clion64.exe",           "CLion",                   Category::Editor),
    k("rider64.exe",           "Rider",                   Category::Editor),
    k("goland64.exe",          "GoLand",                  Category::Editor),
    k("phpstorm64.exe",        "PhpStorm",                Category::Editor),
    k("rubymine64.exe",        "RubyMine",                Category::Editor),
    k("datagrip64.exe",        "DataGrip",                Category::Editor),
    k("devenv.exe",            "Visual Studio",           Category::Editor),
    k("notepad++.exe",         "Notepad++",               Category::Editor),
    k("obsidian.exe",          "Obsidian",                Category::Editor),

    // ── Browsers ────────────────────────────────────────────────────────
    k("chrome.exe",            "Google Chrome",           Category::Browser),
    k("msedge.exe",            "Microsoft Edge",          Category::Browser),
    k("firefox.exe",           "Firefox",                 Category::Browser),
    k("brave.exe",             "Brave",                   Category::Browser),
    k("vivaldi.exe",           "Vivaldi",                 Category::Browser),
    k("opera.exe",             "Opera",                   Category::Browser),
    k("arc.exe",               "Arc",                     Category::Browser),
    k("zen.exe",               "Zen Browser",             Category::Browser),

    // ── Design ──────────────────────────────────────────────────────────
    k("figma.exe",             "Figma",                   Category::Design),
    k("photoshop.exe",         "Photoshop",               Category::Design),
    k("illustrator.exe",       "Illustrator",             Category::Design),
    k("adobe xd.exe",          "Adobe XD",                Category::Design),
    k("affinityphoto.exe",     "Affinity Photo",          Category::Design),
    k("affinitydesigner.exe",  "Affinity Designer",       Category::Design),
    k("blender.exe",           "Blender",                 Category::Design),
    k("krita.exe",             "Krita",                   Category::Design),

    // ── Terminals ───────────────────────────────────────────────────────
    k("windowsterminal.exe",   "Windows Terminal",        Category::Terminal),
    k("wt.exe",                "Windows Terminal",        Category::Terminal),
    k("powershell.exe",        "PowerShell",              Category::Terminal),
    k("pwsh.exe",              "PowerShell",              Category::Terminal),
    k("cmd.exe",               "Command Prompt",          Category::Terminal),
    k("alacritty.exe",         "Alacritty",               Category::Terminal),
    k("wezterm-gui.exe",       "WezTerm",                 Category::Terminal),
    k("hyper.exe",             "Hyper",                   Category::Terminal),
    k("conemu64.exe",          "ConEmu",                  Category::Terminal),
    k("mintty.exe",            "Git Bash",                Category::Terminal),
    // The window of a classic console belongs to conhost, not to the shell.
    k("conhost.exe",           "Console",                 Category::Terminal),

    // ── Documents ───────────────────────────────────────────────────────
    k("acrobat.exe",           "Adobe Acrobat",           Category::Document),
    k("acrord32.exe",          "Adobe Reader",            Category::Document),
    k("sumatrapdf.exe",        "SumatraPDF",              Category::Document),
    k("foxitpdfreader.exe",    "Foxit Reader",            Category::Document),
    k("winword.exe",           "Word",                    Category::Document),
    k("excel.exe",             "Excel",                   Category::Document),
    k("powerpnt.exe",          "PowerPoint",              Category::Document),
    k("onenote.exe",           "OneNote",                 Category::Document),

    // ── Folders ─────────────────────────────────────────────────────────
    k("explorer.exe",          "File Explorer",           Category::Folder),

    // ── Off by default: messaging and mail ──────────────────────────────
    denied("slack.exe",        "Slack",                   Category::Comms),
    denied("discord.exe",      "Discord",                 Category::Comms),
    denied("telegram.exe",     "Telegram",                Category::Comms),
    denied("whatsapp.exe",     "WhatsApp",                Category::Comms),
    denied("signal.exe",       "Signal",                  Category::Comms),
    denied("ms-teams.exe",     "Microsoft Teams",         Category::Comms),
    denied("teams.exe",        "Microsoft Teams",         Category::Comms),
    denied("outlook.exe",      "Outlook",                 Category::Comms),
    denied("thunderbird.exe",  "Thunderbird",             Category::Comms),
    denied("zoom.exe",         "Zoom",                    Category::Comms),
    denied("skype.exe",        "Skype",                   Category::Comms),

    // ── Chronicle stays out of its own record ───────────────────────────
    denied("chronicle.exe",         "Chronicle",          Category::Other),
    denied("chronicled.exe",        "Chronicle Recorder", Category::Other),

    // ── Never, under any setting ────────────────────────────────────────
    denied("1password.exe",         "1Password",          Category::Other),
    denied("keepass.exe",           "KeePass",            Category::Other),
    denied("keepassxc.exe",         "KeePassXC",          Category::Other),
    denied("bitwarden.exe",         "Bitwarden",          Category::Other),
    denied("dashlane.exe",          "Dashlane",           Category::Other),
    denied("keeper.exe",            "Keeper",             Category::Other),
    denied("lastpass.exe",          "LastPass",           Category::Other),
    denied("enpass.exe",            "Enpass",             Category::Other),
    denied("nordpass.exe",          "NordPass",           Category::Other),
    denied("proton pass.exe",       "Proton Pass",        Category::Other),
    denied("credentialuibroker.exe","Windows Security",   Category::Other),
    denied("consent.exe",           "User Account Control", Category::Other),
    denied("logonui.exe",           "Sign-in",            Category::Other),
    denied("lockapp.exe",           "Lock Screen",        Category::Other),
    denied("winlogon.exe",          "Windows Logon",      Category::Other),
    denied("searchhost.exe",        "Windows Search",     Category::Other),
    denied("shellexperiencehost.exe","Windows Shell",     Category::Other),
    denied("startmenuexperiencehost.exe","Start Menu",    Category::Other),
    denied("textinputhost.exe",     "Text Input",         Category::Other),
    denied("applicationframehost.exe","Windows App",      Category::Other),
];

/// Executables that are hard-denied. No preference re-enables these; the
/// check runs before any user configuration is consulted.
static PERMANENT_DENY: &[&str] = &[
    "1password.exe",
    "keepass.exe",
    "keepassxc.exe",
    "bitwarden.exe",
    "dashlane.exe",
    "keeper.exe",
    "lastpass.exe",
    "enpass.exe",
    "nordpass.exe",
    "proton pass.exe",
    "credentialuibroker.exe",
    "consent.exe",
    "logonui.exe",
    "lockapp.exe",
    "winlogon.exe",
    "lsass.exe",
];

/// Substrings in an executable or product name that trigger an automatic
/// deny on first sight. Finance and health, per the shipped deny list.
static AUTO_DENY_HINTS: &[&str] = &[
    "bank", "banking", "wallet", "crypto", "tax", "payroll",
    "health", "medical", "patient", "pharmacy", "authenticator",
];

/// The resolved capture rules for this machine.
#[derive(Debug, Clone, Default)]
pub struct Policies {
    overrides: HashMap<String, CapturePolicy>,
}

impl Policies {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load user overrides, e.g. from the settings table.
    pub fn with_overrides(overrides: HashMap<String, CapturePolicy>) -> Self {
        Self { overrides }
    }

    pub fn set(&mut self, app_id: &str, policy: CapturePolicy) {
        self.overrides.insert(app_id.to_ascii_lowercase(), policy);
    }

    /// Decide what may be recorded about an app. The permanent deny list wins
    /// over every user override — that is the point of it being permanent.
    pub fn policy_for(&self, app_id: &str) -> CapturePolicy {
        let id = app_id.to_ascii_lowercase();
        let canonical = canonical_id(&id);

        if PERMANENT_DENY.contains(&id.as_str()) || PERMANENT_DENY.contains(&canonical.as_str()) {
            return CapturePolicy::Ignore;
        }
        if AUTO_DENY_HINTS.iter().any(|h| id.contains(h)) {
            // A user override can still relax this one: it is a guess, not a rule.
            return self.overrides.get(&id).copied().unwrap_or(CapturePolicy::Ignore);
        }
        // An override is keyed on whichever name the user was shown.
        if let Some(p) = self.overrides.get(&id).or_else(|| self.overrides.get(&canonical)) {
            return *p;
        }
        lookup(&id).map(|k| k.default_policy).unwrap_or(CapturePolicy::Full)
    }

    /// True when the app was denied by a shipped rule rather than by the user.
    pub fn is_auto_denied(&self, app_id: &str) -> bool {
        let id = app_id.to_ascii_lowercase();
        let canonical = canonical_id(&id);
        PERMANENT_DENY.contains(&id.as_str())
            || PERMANENT_DENY.contains(&canonical.as_str())
            || AUTO_DENY_HINTS.iter().any(|h| id.contains(h))
            || lookup(&id).is_some_and(|k| k.default_policy == CapturePolicy::Ignore)
    }

    pub fn entry(&self, app_id: &str) -> AppEntry {
        AppEntry {
            app_id: app_id.to_ascii_lowercase(),
            display_name: display_name(app_id),
            category: category_of(app_id),
            policy: self.policy_for(app_id),
            auto_denied: self.is_auto_denied(app_id),
        }
    }
}

/// Real executables are not named the way a catalogue expects. WhatsApp ships
/// as `whatsapp.root.exe`, and an exact-match deny list lets it straight
/// through — which is how a messaging app that is off by default ended up
/// recorded. Falling back to the first token of the file name closes that gap
/// for every installer that decorates its binary with a suffix.
fn canonical_id(id: &str) -> String {
    if CATALOGUE.iter().any(|k| k.id == id) || PERMANENT_DENY.contains(&id) {
        return id.to_string();
    }
    let stem = id.strip_suffix(".exe").unwrap_or(id);
    match stem.split(['.', '-', '_']).next() {
        Some(first) if !first.is_empty() && first != stem => format!("{first}.exe"),
        _ => id.to_string(),
    }
}

fn lookup(id: &str) -> Option<&'static Known> {
    CATALOGUE
        .iter()
        .find(|k| k.id == id)
        .or_else(|| {
            let c = canonical_id(id);
            CATALOGUE.iter().find(|k| k.id == c)
        })
}

/// Pretty name for an executable. Falls back to a title-cased stem, so an
/// unknown `my-tool.exe` shows up as "My Tool" rather than a filename.
pub fn display_name(app_id: &str) -> String {
    let id = app_id.to_ascii_lowercase();
    if let Some(k) = lookup(&id) {
        return k.name.to_string();
    }
    let stem = id.strip_suffix(".exe").unwrap_or(&id);
    stem.split(['-', '_', '.'])
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn category_of(app_id: &str) -> Category {
    lookup(&app_id.to_ascii_lowercase())
        .map(|k| k.category)
        .unwrap_or(Category::Other)
}

/// Every app the catalogue knows about, for the settings list.
pub fn catalogue(policies: &Policies) -> Vec<AppEntry> {
    let mut out: Vec<AppEntry> = CATALOGUE.iter().map(|k| policies.entry(k.id)).collect();
    out.sort_by(|a, b| {
        a.category
            .as_str()
            .cmp(b.category.as_str())
            .then(a.display_name.cmp(&b.display_name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_managers_cannot_be_re_enabled() {
        let mut p = Policies::new();
        p.set("1password.exe", CapturePolicy::Full);
        assert_eq!(p.policy_for("1Password.exe"), CapturePolicy::Ignore);
    }

    #[test]
    fn messaging_is_off_but_can_be_turned_on() {
        let mut p = Policies::new();
        assert_eq!(p.policy_for("slack.exe"), CapturePolicy::Ignore);
        p.set("slack.exe", CapturePolicy::Full);
        assert_eq!(p.policy_for("slack.exe"), CapturePolicy::Full);
    }

    #[test]
    fn finance_apps_are_denied_by_name() {
        let p = Policies::new();
        assert_eq!(p.policy_for("MercuryBanking.exe"), CapturePolicy::Ignore);
        assert!(p.is_auto_denied("MercuryBanking.exe"));
    }

    /// WhatsApp ships as `whatsapp.root.exe`, which an exact-match deny list
    /// lets straight through. It did, and it was recorded.
    #[test]
    fn whatsapp_root_exe_is_denied_like_plain_whatsapp() {
        let p = Policies::new();
        assert_eq!(p.policy_for("WhatsApp.root.exe"), CapturePolicy::Ignore);
        assert!(p.is_auto_denied("WhatsApp.root.exe"));
        assert_eq!(display_name("WhatsApp.root.exe"), "WhatsApp");
        assert_eq!(category_of("WhatsApp.root.exe"), Category::Comms);
    }

    #[test]
    fn decorated_executable_names_still_resolve() {
        let p = Policies::new();
        // Installers decorate binaries; the deny decision must survive it.
        assert_eq!(p.policy_for("1password.helper.exe"), CapturePolicy::Ignore);
        assert_eq!(p.policy_for("slack-desktop.exe"), CapturePolicy::Ignore);
    }

    #[test]
    fn chronicle_stays_out_of_its_own_record() {
        let p = Policies::new();
        assert_eq!(p.policy_for("chronicle.exe"), CapturePolicy::Ignore);
        assert_eq!(p.policy_for("chronicled.exe"), CapturePolicy::Ignore);
    }

    #[test]
    fn unknown_apps_are_captured_and_named() {
        let p = Policies::new();
        assert_eq!(p.policy_for("my-cool-tool.exe"), CapturePolicy::Full);
        assert_eq!(display_name("my-cool-tool.exe"), "My Cool Tool");
    }
}

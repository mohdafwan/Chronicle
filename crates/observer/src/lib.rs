//! The observer: the only part of Chronicle that watches, and the smallest
//! part it could reasonably be.
//!
//! One sample is one look at the foreground window. Enrichers turn that look
//! into artifacts, capture policy decides whether any of it may be kept, and
//! redaction runs before the result is handed to the store.

pub mod chrome;
pub mod enrich;
#[cfg(windows)]
pub mod explorer;

#[cfg(windows)]
pub mod win;

#[cfg(not(windows))]
pub mod win {
    //! Stubs so the workspace type-checks off Windows. The real observer is
    //! per-platform; macOS lands on the Accessibility API, not this file.
    use chronicle_core::model::Frame;

    #[derive(Debug, Clone)]
    pub struct Sample {
        pub hwnd: isize,
        pub title: String,
        pub pid: u32,
        pub exe_path: Option<String>,
        pub app_id: String,
        pub class: String,
        pub frame: Option<Frame>,
        pub display_id: Option<String>,
    }

    pub fn foreground() -> Option<Sample> {
        None
    }
    pub fn all_windows() -> Vec<Sample> {
        Vec::new()
    }
    pub fn idle_seconds() -> u64 {
        0
    }
    pub fn is_locked() -> bool {
        false
    }
}

use chrono::{DateTime, Utc};
use chronicle_core::model::{ArtifactKind, ArtifactObs, Observation};
use chronicle_core::policy::{self, CapturePolicy, Policies};

pub use enrich::Enricher;
pub use win::{Sample, all_windows, foreground, idle_seconds, is_locked};

/// Explorer window classes that are an actual folder view.
///
/// `CabinetWClass` is every modern Explorer window, including This PC and a
/// search result. `ExploreWClass` is the legacy two-pane variant, still
/// reachable from some context menus.
const FOLDER_CLASSES: [&str; 2] = ["CabinetWClass", "ExploreWClass"];

/// Whether this window is the shell itself rather than anything the user works in.
///
/// `explorer.exe` owns the desktop, the taskbar, the Start menu and Alt+Tab in
/// the same process as folder windows, so the executable name alone cannot tell
/// them apart. Recorded as-is, a glance at Alt+Tab became a "File Explorer"
/// artifact filed under Folders with a restore checkbox next to it — an entry
/// that could never resolve to a path, and that no restore could ever honour.
///
/// The test is the window class, not the title: "Program Manager" and "Task
/// View" are localised, and a Hindi or German Windows would sail straight past
/// a title match. Window classes are not translated.
fn is_shell_surface(s: &Sample) -> bool {
    s.app_id == "explorer.exe" && !FOLDER_CLASSES.contains(&s.class.as_str())
}

/// Turns foreground windows into observations the store can keep.
pub struct Sampler {
    enrichers: Vec<Box<dyn Enricher>>,
    policies: Policies,
}

impl Sampler {
    pub fn new(policies: Policies) -> Self {
        Self {
            enrichers: enrich::default_enrichers(),
            policies,
        }
    }

    /// Policies are reloaded rather than mutated, so a settings change takes
    /// effect on the next sample without restarting the daemon.
    pub fn set_policies(&mut self, policies: Policies) {
        self.policies = policies;
    }

    pub fn policies(&self) -> &Policies {
        &self.policies
    }

    /// Take one observation. `None` when there is nothing focused, or when the
    /// focused app must not be recorded at all.
    pub fn sample(&self, now: DateTime<Utc>) -> Option<Observation> {
        let s = win::foreground()?;
        self.observe(&s, now)
    }

    /// The same path as [`Sampler::sample`], split out so it can be tested
    /// against a synthetic window without a desktop.
    pub fn observe(&self, s: &Sample, now: DateTime<Utc>) -> Option<Observation> {
        if self.policies.policy_for(&s.app_id) == CapturePolicy::Ignore || is_shell_surface(s) {
            return None;
        }

        let app_name = policy::display_name(&s.app_id);
        let category = policy::category_of(&s.app_id);

        let mut artifacts: Vec<ArtifactObs> = self
            .enrichers
            .iter()
            .filter(|e| e.matches(s))
            .flat_map(|e| e.enrich(s))
            .collect();

        // Deduplicate by uri, keeping the first (most specific) sighting.
        let mut seen = std::collections::HashSet::new();
        artifacts.retain(|a| seen.insert(a.uri.clone()));

        // Every sample yields at least the app itself, so that a session can
        // record "Figma was open for 40 minutes" even when nothing inside it
        // could be resolved.
        artifacts.push(ArtifactObs::new(
            ArtifactKind::App,
            format!("app://{}", s.app_id),
            app_name.clone(),
        ));

        let obs = Observation {
            at: now,
            app_id: s.app_id.clone(),
            app_name,
            exe_path: s.exe_path.clone(),
            category,
            title: String::new(), // filled in by normalise, after redaction
            pid: s.pid,
            frame: s.frame,
            display_id: s.display_id.clone(),
            artifacts,
        };

        chronicle_core::normalise(&self.policies, &s.app_id, &s.title, obs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(app: &str, title: &str) -> Sample {
        classed(app, title, "CabinetWClass")
    }

    fn classed(app: &str, title: &str, class: &str) -> Sample {
        Sample {
            hwnd: 1,
            title: title.into(),
            pid: 42,
            exe_path: None,
            app_id: app.into(),
            class: class.into(),
            frame: None,
            display_id: None,
        }
    }

    #[test]
    fn the_desktop_and_alt_tab_are_not_folders() {
        // Both arrive as explorer.exe. Recorded, they became a File Explorer
        // artifact under Folders that no restore could ever open.
        let s = Sampler::new(Policies::new());
        assert!(s.observe(&classed("explorer.exe", "Program Manager", "Progman"), Utc::now()).is_none());
        assert!(
            s.observe(
                &classed("explorer.exe", "Task View", "XamlExplorerHostIslandWindow"),
                Utc::now()
            )
            .is_none()
        );
        assert!(
            s.observe(&classed("explorer.exe", "Start", "Windows.UI.Core.CoreWindow"), Utc::now())
                .is_none()
        );
    }

    #[test]
    fn a_real_folder_window_still_records() {
        let s = Sampler::new(Policies::new());
        let o = s
            .observe(&classed("explorer.exe", "Downloads", "CabinetWClass"), Utc::now())
            .unwrap();
        assert!(o.artifacts.iter().any(|a| a.uri == "app://explorer.exe"));
    }

    #[test]
    fn shell_chrome_from_another_process_is_left_alone() {
        // The rule is about Explorer hosting the shell, not about window
        // classes in general — a normal app must not be caught by it.
        let s = Sampler::new(Policies::new());
        assert!(s.observe(&classed("code.exe", "main.rs", "Chrome_WidgetWin_1"), Utc::now()).is_some());
    }

    #[test]
    fn a_denied_app_produces_nothing_at_all() {
        let s = Sampler::new(Policies::new());
        assert!(s.observe(&sample("1password.exe", "Vault"), Utc::now()).is_none());
    }

    #[test]
    fn every_sample_carries_the_app_itself() {
        let s = Sampler::new(Policies::new());
        let o = s.observe(&sample("figma.exe", "Login — Figma"), Utc::now()).unwrap();
        assert!(o.artifacts.iter().any(|a| a.uri == "app://figma.exe"));
        assert_eq!(o.title, "Login — Figma");
    }

    #[test]
    fn a_title_blind_app_reports_only_its_name() {
        let mut p = Policies::new();
        p.set("excel.exe", CapturePolicy::TitlesOff);
        let s = Sampler::new(p);
        let o = s.observe(&sample("excel.exe", "Payroll Q3.xlsx"), Utc::now()).unwrap();
        assert_eq!(o.title, "Excel");
        assert!(o.artifacts.iter().all(|a| a.kind == ArtifactKind::App));
    }

    #[test]
    fn a_title_carrying_a_token_loses_the_title_not_the_session() {
        let s = Sampler::new(Policies::new());
        let o = s
            .observe(&sample("chrome.exe", "token=ghp_abcdefghijklmnopqrstuvwxyz01"), Utc::now())
            .unwrap();
        assert_eq!(o.title, "Google Chrome");
    }
}

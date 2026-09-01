//! The whole loop, on realistic data: observations go in, sessions come out,
//! and a restore plan comes off the back of them.
//!
//! This is the test that would have caught a broken SQL join, a sessioniser
//! that splits a day into forty fragments, or a planner that tries to launch a
//! file that is not there.

use chrono::{DateTime, Duration, TimeZone, Utc};
use chronicle_core::model::*;
use chronicle_core::sessionize::{self, SessionRules};
use chronicle_core::Store;

fn temp_store() -> (Store, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "chronicle-test-{}-{}.db",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let store = Store::open(&path).expect("open store");
    (store, path)
}

fn obs(
    at: DateTime<Utc>,
    app_id: &str,
    app_name: &str,
    category: Category,
    title: &str,
    artifacts: Vec<ArtifactObs>,
) -> Observation {
    Observation {
        at,
        app_id: app_id.into(),
        app_name: app_name.into(),
        exe_path: Some(format!("C:/Program Files/{app_name}/{app_id}")),
        category,
        title: title.into(),
        pid: 1234,
        frame: Some(Frame { x: 0, y: 0, w: 1920, h: 1080 }),
        display_id: Some(r"\\.\DISPLAY1".into()),
        artifacts,
    }
}

/// One stretch of work on one project: editor, terminal and a few tabs,
/// sampled every 30 seconds the way the daemon would.
fn record_block(
    store: &Store,
    start: DateTime<Utc>,
    minutes: i64,
    project: &str,
    branch: &str,
    tabs: &[(&str, &str)],
) {
    let root = format!("C:/work/{project}");
    let samples = minutes * 2; // every 30s

    for i in 0..samples {
        let at = start + Duration::seconds(i * 30);

        match i % 4 {
            // Editor, on the project, on a branch.
            0 | 1 => {
                let a = ArtifactObs::new(
                    ArtifactKind::Project,
                    format!("file:///{}", root.to_lowercase()),
                    project,
                )
                .with_root(&root)
                .with_state("branch", branch);
                store
                    .record(&obs(
                        at,
                        "code.exe",
                        "Visual Studio Code",
                        Category::Editor,
                        &format!("main.rs - {project} - Visual Studio Code"),
                        vec![a],
                    ))
                    .unwrap();
            }
            // Terminal, in the project directory.
            2 => {
                let a = ArtifactObs::new(
                    ArtifactKind::Terminal,
                    format!("file:///{}/terminal", root.to_lowercase()),
                    project,
                )
                .with_root(&root)
                .with_state("cwd", &root);
                store
                    .record(&obs(
                        at,
                        "windowsterminal.exe",
                        "Windows Terminal",
                        Category::Terminal,
                        project,
                        vec![a],
                    ))
                    .unwrap();
            }
            // Browser: the focused tab first, the rest merely present.
            _ => {
                let artifacts: Vec<ArtifactObs> = tabs
                    .iter()
                    .enumerate()
                    .map(|(n, (url, name))| {
                        let mut a = ArtifactObs::new(ArtifactKind::Url, *url, *name);
                        if n == 0 {
                            a = a.with_state("scroll", "0");
                        }
                        a
                    })
                    .collect();
                store
                    .record(&obs(
                        at,
                        "chrome.exe",
                        "Google Chrome",
                        Category::Browser,
                        tabs[0].1,
                        artifacts,
                    ))
                    .unwrap();
            }
        }
    }
}

#[test]
fn a_days_work_becomes_two_named_sessions_and_a_restore_plan() {
    let (store, path) = temp_store();
    let rules = SessionRules::default();

    // 2:15 PM — 100 minutes on the Android app.
    let block_a = Utc.with_ymd_and_hms(2026, 8, 28, 14, 15, 0).unwrap();
    record_block(
        &store,
        block_a,
        100,
        "spotted-android",
        "feature/login-otp",
        &[
            ("https://developer.android.com/identity/sign-in/credential-manager",
             "Credential Manager — sign in with passkeys"),
            ("https://firebase.google.com/docs/auth/android/phone-auth",
             "Phone auth on Android"),
            ("https://stackoverflow.com/questions/78123/otp-autofill",
             "OTP autofill not firing on API 34"),
        ],
    );

    // A 30-minute break, then different work. Longer than the 12-minute idle
    // gap, and a different project root, so no stitching.
    let block_b = block_a + Duration::minutes(130);
    record_block(
        &store,
        block_b,
        60,
        "northbeam-web",
        "main",
        &[("https://vercel.com/docs/deployments", "Deployments — Vercel")],
    );

    // Sessionise from a point well after the work ended, so both sessions close.
    let now = block_b + Duration::minutes(90);
    sessionize::run(&store, &rules, now).expect("sessionise");

    // ── the day reads back as two sessions, named after the projects ────
    let sessions = store.recent_sessions(10).unwrap();
    assert_eq!(
        sessions.len(),
        2,
        "expected two sessions, got {:?}",
        sessions.iter().map(|s| &s.title).collect::<Vec<_>>()
    );

    let titles: Vec<&str> = sessions.iter().map(|s| s.title.as_str()).collect();
    assert!(titles.contains(&"Spotted Android"), "titles were {titles:?}");
    assert!(titles.contains(&"Northbeam Web"), "titles were {titles:?}");

    let android = sessions
        .iter()
        .find(|s| s.title == "Spotted Android")
        .unwrap();

    // Active time should be close to the 100 minutes actually worked.
    let minutes = android.active_seconds / 60;
    assert!(
        (95..=105).contains(&minutes),
        "active time was {minutes} minutes, expected about 100"
    );
    assert!(android.ended_at.is_some(), "a finished session must be closed");

    // ── the session carries the context, including restorable state ─────
    let detail = store.session_detail(android.id).unwrap().unwrap();
    let project = detail
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Project)
        .expect("the project artifact survived");
    assert_eq!(project.state_of("branch"), Some("feature/login-otp"));
    assert_eq!(
        project.project_root.as_deref(),
        Some("C:/work/spotted-android")
    );

    let tabs = detail
        .artifacts
        .iter()
        .filter(|a| a.kind == ArtifactKind::Url)
        .count();
    assert_eq!(tabs, 3, "all three tabs belong to the session");

    // The editor held the most attention, so it ranks first.
    assert_eq!(detail.artifacts[0].category, Category::Editor);

    // ── search finds it by something the user would actually type ───────
    assert_eq!(store.search("spotted", 10).unwrap().len(), 1);
    assert_eq!(store.search("otp autofill", 10).unwrap().len(), 1);
    assert_eq!(store.search("vercel", 10).unwrap().len(), 1);

    // ── and a restore plan comes off the back of it ─────────────────────
    let plan = chronicle_restore::plan(&detail);

    let browser_items: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.category == Category::Browser)
        .collect();
    assert_eq!(
        browser_items.len(),
        1,
        "three tabs must collapse into one browser window"
    );
    assert_eq!(browser_items[0].artifact_ids.len(), 3);

    // Editors and terminals are launched before browsers.
    let order: Vec<Category> = plan.items.iter().map(|i| i.category).collect();
    let first_browser = order.iter().position(|c| *c == Category::Browser).unwrap();
    let last_editor = order.iter().rposition(|c| *c == Category::Editor).unwrap();
    assert!(
        last_editor < first_browser,
        "editors must be planned before browsers, order was {order:?}"
    );

    // Nothing in this plan points at a real path on disk, so nothing claims to
    // be restorable — the planner must not pretend otherwise.
    let (ready, total) = plan.readiness();
    assert_eq!(total, plan.items.len());
    assert_eq!(
        ready, 0,
        "fixture paths do not exist, so no item may be marked actionable"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn forgetting_an_hour_leaves_nothing_behind() {
    let (store, path) = temp_store();
    let rules = SessionRules::default();

    let start = Utc::now() - Duration::minutes(90);
    record_block(&store, start, 60, "secret-project", "main", &[(
        "https://example.com/private",
        "Something private",
    )]);
    sessionize::run(&store, &rules, Utc::now()).unwrap();

    let (sessions, artifacts, observations) = store.counts().unwrap();
    assert!(sessions > 0 && artifacts > 0 && observations > 0);

    store
        .forget_range(Utc::now() - Duration::hours(3), Utc::now())
        .unwrap();

    let (sessions, artifacts, observations) = store.counts().unwrap();
    assert_eq!(
        (sessions, artifacts, observations),
        (0, 0, 0),
        "forget must be a hard delete, not a hide"
    );
    assert!(
        store.search("secret", 10).unwrap().is_empty(),
        "the search index must be cleared too"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_interrupted_session_is_recoverable_after_a_crash() {
    let (store, path) = temp_store();
    let rules = SessionRules::default();

    let start = Utc::now() - Duration::minutes(40);
    record_block(&store, start, 30, "spotted-android", "main", &[(
        "https://example.com/a",
        "A",
    )]);

    // The daemon was running and never got to write its marker.
    store.mark_running().unwrap();
    sessionize::run(&store, &rules, Utc::now() - Duration::minutes(9)).unwrap();

    assert!(store.crashed_last_run().unwrap());
    let open = store.open_session().unwrap().expect("a session was in flight");

    // What the daemon does on the next launch.
    let ended = store.last_observation_at(open.id).unwrap().unwrap();
    store
        .close_session(open.id, ended, open.active_seconds, EndReason::Interrupted)
        .unwrap();

    let recovered = store.session_detail(open.id).unwrap().unwrap();
    assert_eq!(recovered.session.end_reason, EndReason::Interrupted);
    assert!(!recovered.artifacts.is_empty(), "the context survived the crash");

    let _ = std::fs::remove_file(&path);
}

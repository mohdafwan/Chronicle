//! Chronicle's window.
//!
//! A thin layer: it reads the same store the recorder writes, shapes it for
//! display, and hands restore plans to the restore engine. Every string the
//! frontend renders is formatted here, in Rust, so the local timezone and the
//! day boundary have exactly one opinion about them.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chronicle_core::model::{Category, EndReason};
use chronicle_core::{fmt, policy, CapturePolicy, Store};
use chronicle_restore::{Action, RestoreOptions};
use serde::Serialize;
use std::sync::Mutex;

/// `rusqlite::Connection` is `Send` but not `Sync`, so the store lives behind
/// a mutex. Every command is short, so contention is not a concern.
struct AppState {
    store: Mutex<Store>,
}

type Reply<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ── what the frontend receives ──────────────────────────────────────────

#[derive(Serialize)]
struct Tile {
    text: String,
    category: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionCard {
    id: i64,
    title: String,
    day_key: String,
    day_heading: String,
    time_range: String,
    duration: String,
    interrupted: bool,
    live: bool,
    pinned: bool,
    tiles: Vec<Tile>,
}

#[derive(Serialize)]
struct Band {
    flex: f64,
    category: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    key: String,
    name: String,
    detail: String,
    lines: Vec<String>,
    app_name: String,
    monogram: String,
    category: String,
    fidelity: String,
    fidelity_label: String,
    actionable: bool,
    selected: bool,
    note: Option<String>,
    command: Option<String>,
}

#[derive(Serialize)]
struct Group {
    label: String,
    category: String,
    items: Vec<Item>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    id: i64,
    title: String,
    day_heading: String,
    time_range: String,
    duration: String,
    interrupted: bool,
    pinned: bool,
    end_note: Option<String>,
    bands: Vec<Band>,
    ruler: Vec<String>,
    groups: Vec<Group>,
    ready: usize,
    total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeRow {
    label: String,
    ok: bool,
    message: String,
    fidelity_label: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RestoreReceipt {
    restored: usize,
    total: usize,
    seconds: f64,
    rows: Vec<OutcomeRow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    database: String,
    size_mb: f64,
    sessions: i64,
    artifacts: i64,
    observations: i64,
    retention_days: i64,
    recording: bool,
    last_beat: Option<String>,
    crashed_last_run: bool,
    current_session: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    app_id: String,
    display_name: String,
    category: String,
    category_label: String,
    policy: String,
    auto_denied: bool,
}

// ── commands ────────────────────────────────────────────────────────────

fn card(store: &Store, s: &chronicle_core::Session) -> SessionCard {
    let apps = store.session_apps(s.id).unwrap_or_default();
    SessionCard {
        id: s.id,
        title: s.title.clone(),
        day_key: fmt::day_key(s.started_at),
        day_heading: fmt::day_heading(s.started_at),
        time_range: fmt::time_range(s.started_at, s.ended_at),
        duration: fmt::duration(s.active_seconds),
        interrupted: s.end_reason == EndReason::Interrupted,
        live: s.ended_at.is_none(),
        pinned: s.pinned,
        tiles: apps
            .iter()
            .take(6)
            .map(|(name, cat, _)| Tile {
                text: fmt::monogram(name),
                category: cat.as_str().to_string(),
            })
            .collect(),
    }
}

#[tauri::command]
fn list_sessions(state: tauri::State<AppState>, limit: usize) -> Reply<Vec<SessionCard>> {
    let store = state.store.lock().map_err(err)?;
    let sessions = store.recent_sessions(limit).map_err(err)?;
    Ok(sessions.iter().map(|s| card(&store, s)).collect())
}

#[tauri::command]
fn search_sessions(state: tauri::State<AppState>, query: String) -> Reply<Vec<SessionCard>> {
    let store = state.store.lock().map_err(err)?;
    let sessions = store.search(&query, 60).map_err(err)?;
    Ok(sessions.iter().map(|s| card(&store, s)).collect())
}

/// The detail pane renders the restore plan itself, so what you see listed is
/// exactly what the button will act on — including the rows it cannot.
#[tauri::command]
fn get_session(state: tauri::State<AppState>, id: i64) -> Reply<Option<Detail>> {
    let store = state.store.lock().map_err(err)?;
    let Some(detail) = store.session_detail(id).map_err(err)? else {
        return Ok(None);
    };
    let plan = chronicle_restore::plan(&detail);
    let (ready, total) = plan.readiness();
    let s = &detail.session;

    // Group the plan by category, keeping launch order.
    let mut groups: Vec<Group> = Vec::new();
    for item in &plan.items {
        let row = Item {
            key: item.key.clone(),
            name: item.label.clone(),
            detail: item.detail.lines().next().unwrap_or("").to_string(),
            lines: item.detail.lines().skip(1).map(str::to_string).collect(),
            app_name: item.app_name.clone(),
            monogram: fmt::monogram(&item.app_name),
            category: item.category.as_str().to_string(),
            fidelity: format!("{:?}", item.fidelity).to_lowercase(),
            fidelity_label: item.fidelity.label().to_string(),
            actionable: item.fidelity.actionable(),
            selected: item.selected,
            note: item.note.clone(),
            command: match &item.action {
                Action::Launch { program, args } => {
                    Some(format!("{} {}", program.display(), args.join(" ")))
                }
                Action::Shell { path } => Some(format!("open {}", path.display())),
                Action::Manual { instruction } => Some(instruction.clone()),
            },
        };
        match groups.iter_mut().find(|g| g.category == row.category) {
            Some(g) => g.items.push(row),
            None => groups.push(Group {
                label: item.category.label().to_string(),
                category: row.category.clone(),
                items: vec![row],
            }),
        }
    }

    Ok(Some(Detail {
        id: s.id,
        title: s.title.clone(),
        day_heading: fmt::day_heading(s.started_at),
        time_range: fmt::time_range(s.started_at, s.ended_at),
        duration: fmt::duration(s.active_seconds),
        interrupted: s.end_reason == EndReason::Interrupted,
        pinned: s.pinned,
        end_note: match s.end_reason {
            EndReason::Interrupted => Some("ended by an unclean shutdown".into()),
            EndReason::ContextSwitch => Some("you moved on to something else".into()),
            EndReason::Locked => Some("the machine was locked".into()),
            _ => None,
        },
        bands: bands(&store, s.id),
        ruler: ruler(s.started_at, s.ended_at),
        groups,
        ready,
        total,
    }))
}

/// Divide the session into twenty slots and colour each by the kind of app
/// that held it. Not a chart — a shape you can recognise at a glance.
fn bands(store: &Store, id: i64) -> Vec<Band> {
    const SLOTS: usize = 20;
    let shape = store.session_shape(id).unwrap_or_default();
    if shape.len() < 2 {
        return Vec::new();
    }
    let start = shape[0].0;
    let span = (shape[shape.len() - 1].0 - start).num_milliseconds().max(1) as f64;

    let mut slots: Vec<Option<Category>> = vec![None; SLOTS];
    let mut counts: Vec<std::collections::HashMap<Category, usize>> =
        vec![Default::default(); SLOTS];
    for (at, cat) in &shape {
        let t = (*at - start).num_milliseconds() as f64 / span;
        let i = ((t * SLOTS as f64) as usize).min(SLOTS - 1);
        *counts[i].entry(*cat).or_default() += 1;
    }
    for (i, c) in counts.iter().enumerate() {
        slots[i] = c.iter().max_by_key(|(_, n)| **n).map(|(cat, _)| *cat);
    }

    // Collapse runs of the same category so the bar reads as blocks of work.
    let mut out: Vec<Band> = Vec::new();
    for slot in slots {
        let cat = slot.unwrap_or(Category::Other).as_str().to_string();
        match out.last_mut() {
            Some(b) if b.category == cat => b.flex += 1.0,
            _ => out.push(Band { flex: 1.0, category: cat }),
        }
    }
    out
}

fn ruler(start: chrono::DateTime<chrono::Utc>, end: Option<chrono::DateTime<chrono::Utc>>) -> Vec<String> {
    let end = end.unwrap_or_else(chrono::Utc::now);
    let mid = start + (end - start) / 2;
    vec![fmt::clock(start), fmt::clock(mid), fmt::clock(end)]
}

#[tauri::command]
fn restore_session(
    state: tauri::State<AppState>,
    id: i64,
    keys: Vec<String>,
) -> Reply<RestoreReceipt> {
    let store = state.store.lock().map_err(err)?;
    let detail = store
        .session_detail(id)
        .map_err(err)?
        .ok_or("that session is gone")?;

    let mut plan = chronicle_restore::plan(&detail);
    // The window sends exactly what the user left ticked.
    for item in &mut plan.items {
        item.selected = item.fidelity.actionable() && keys.contains(&item.key);
    }

    let outcome = chronicle_restore::execute(
        &plan,
        &RestoreOptions {
            dry_run: false,
            stagger_ms: 400,
        },
    );

    Ok(RestoreReceipt {
        restored: outcome.restored(),
        total: plan.items.iter().filter(|i| i.selected).count(),
        seconds: outcome.elapsed_ms as f64 / 1000.0,
        rows: outcome
            .items
            .iter()
            .filter(|i| i.message != "skipped")
            .map(|i| OutcomeRow {
                label: i.label.clone(),
                ok: i.ok,
                message: i.message.lines().next().unwrap_or("").to_string(),
                fidelity_label: i.fidelity.label().to_string(),
            })
            .collect(),
    })
}

#[tauri::command]
fn rename_session(state: tauri::State<AppState>, id: i64, title: String) -> Reply<()> {
    let title = title.trim();
    if title.is_empty() {
        return Err("a session needs a name".into());
    }
    state
        .store
        .lock()
        .map_err(err)?
        .rename_session(id, title)
        .map_err(err)
}

#[tauri::command]
fn set_pinned(state: tauri::State<AppState>, id: i64, pinned: bool) -> Reply<()> {
    state
        .store
        .lock()
        .map_err(err)?
        .set_pinned(id, pinned)
        .map_err(err)
}

#[tauri::command]
fn delete_session(state: tauri::State<AppState>, id: i64) -> Reply<()> {
    state
        .store
        .lock()
        .map_err(err)?
        .delete_session(id)
        .map_err(err)
}

#[tauri::command]
fn forget_hours(state: tauri::State<AppState>, hours: i64) -> Reply<usize> {
    let store = state.store.lock().map_err(err)?;
    let to = chrono::Utc::now();
    store
        .forget_range(to - chrono::Duration::hours(hours), to)
        .map_err(err)
}

#[tauri::command]
fn get_status(state: tauri::State<AppState>) -> Reply<Status> {
    let store = state.store.lock().map_err(err)?;
    let path = Store::default_path().map_err(err)?;
    let (sessions, artifacts, observations) = store.counts().map_err(err)?;
    let beat = store.last_heartbeat().map_err(err)?;

    Ok(Status {
        database: path.display().to_string(),
        size_mb: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0,
        sessions,
        artifacts,
        observations,
        retention_days: store
            .meta_get("retention_days")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(90),
        // The recorder is a separate process; a fresh heartbeat is how the
        // window knows it is alive without talking to it.
        recording: beat.is_some_and(|b| (chrono::Utc::now() - b).num_seconds() < 180),
        last_beat: beat.map(fmt::stamp),
        crashed_last_run: store.crashed_last_run().map_err(err)?,
        current_session: store.open_session().map_err(err)?.map(|s| s.title),
    })
}

#[tauri::command]
fn list_sources(state: tauri::State<AppState>) -> Reply<Vec<Source>> {
    let store = state.store.lock().map_err(err)?;
    let policies = store.policies().map_err(err)?;
    Ok(policy::catalogue(&policies)
        .into_iter()
        .map(|e| Source {
            app_id: e.app_id,
            display_name: e.display_name,
            category: e.category.as_str().to_string(),
            category_label: e.category.label().to_string(),
            policy: match e.policy {
                CapturePolicy::Full => "full",
                CapturePolicy::TitlesOff => "titles_off",
                CapturePolicy::Ignore => "ignore",
            }
            .to_string(),
            auto_denied: e.auto_denied,
        })
        .collect())
}

#[tauri::command]
fn set_source_policy(state: tauri::State<AppState>, app_id: String, policy: String) -> Reply<()> {
    let p = match policy.as_str() {
        "titles_off" => CapturePolicy::TitlesOff,
        "ignore" => CapturePolicy::Ignore,
        _ => CapturePolicy::Full,
    };
    state
        .store
        .lock()
        .map_err(err)?
        .set_policy(&app_id, p)
        .map_err(err)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CHRONICLE_LOG")
                .unwrap_or_else(|_| "chronicle_app=info".into()),
        )
        .with_target(false)
        .init();

    let path = Store::default_path().expect("resolve the data directory");
    let store = Store::open(&path).expect("open the Chronicle database");
    tracing::info!(db = %path.display(), "window opened");

    tauri::Builder::default()
        .manage(AppState {
            store: Mutex::new(store),
        })
        .invoke_handler(tauri::generate_handler![
            list_sessions,
            search_sessions,
            get_session,
            restore_session,
            rename_session,
            set_pinned,
            delete_session,
            forget_hours,
            get_status,
            list_sources,
            set_source_policy,
        ])
        .run(tauri::generate_context!())
        .expect("run Chronicle");
}

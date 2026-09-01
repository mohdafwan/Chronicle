//! `chronicled` — the always-on half of Chronicle.
//!
//! It samples the foreground window, hands the result to the store, and runs
//! the sessioniser on a slow timer. The subcommands exist so the whole loop can
//! be exercised and inspected before there is any window to click in.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, Utc};
use chronicle_core::model::{Category, EndReason};
use chronicle_core::sessionize::{self, SessionRules};
use chronicle_core::{CapturePolicy, Store};
use chronicle_observer::Sampler;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

mod startup;
mod update;

/// How often the foreground window is looked at. Cheap: three Win32 calls.
const POLL: std::time::Duration = std::time::Duration::from_secs(2);
/// A record is written on any change, and at least this often regardless, so
/// a long stretch in one window still produces focus time.
const HEARTBEAT_SECS: i64 = 30;
/// The sessioniser is incremental and idempotent; running it often is cheap.
const SESSIONISE_SECS: i64 = 60;
/// Stop recording after this much silence. Matches the session idle rule.
const IDLE_CUTOFF_SECS: u64 = 12 * 60;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    let background = args.iter().any(|a| a == "--background");

    init_logging(background)?;

    match cmd {
        "run" | "--background" => run(background),
        "autostart" => startup::command_line(args.get(1).map(String::as_str)),
        "update" => update::command_line(&args[1..]),
        "doctor" => doctor(),
        "scan" => scan(),
        "status" => status(),
        "sessions" => sessions(arg_num(&args, 1).unwrap_or(20) as usize),
        "show" => show(arg_num(&args, 1).context("usage: chronicled show <session-id>")?),
        "search" => search(&args[1..].join(" ")),
        "sources" => sources(),
        "tabs" => tabs(),
        "plan" => plan(arg_num(&args, 1).context("usage: chronicled plan <session-id>")?),
        "restore" => restore(
            arg_num(&args, 1).context("usage: chronicled restore <session-id> [--go]")?,
            !args.iter().any(|a| a == "--go"),
        ),
        "forget" => forget(arg_num(&args, 1).unwrap_or(1)),
        "path" => {
            println!("{}", Store::default_path()?.display());
            Ok(())
        }
        "help" | "--help" | "-h" => {
            help();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n");
            help();
            std::process::exit(2);
        }
    }
}

fn help() {
    println!(
        "chronicled — Chronicle's recorder

  run                 watch the desktop and record sessions (default)
  autostart [on|off]  start recording automatically when you log in
  update [--install]  check github for a newer Chronicle (--auto off|check|install)
  doctor              show what Chronicle can see in the window in front of you
  scan                run the enrichers over every window you have open
  status              database location, size, counts, recording state
  sessions [n]        list the most recent sessions
  show <id>           everything Chronicle remembers about one session
  search <words>      full-text search across sessions and artifacts
  plan <id>           what a restore would do, without doing any of it
  restore <id> --go   actually reopen the workspace (dry run without --go)
  tabs                what Chronicle can read from your browsers right now
  sources [--all]     per-app capture policy for the apps installed here
  forget <hours>      permanently delete everything from the last N hours
  path                print the database path
"
    );
}

/// Where the recorder writes its log when it has no console to write to.
fn log_path() -> Result<std::path::PathBuf> {
    Ok(Store::default_path()?.with_file_name("chronicled.log"))
}

/// Interactive runs log to the terminal; a background run logs to a file.
///
/// The file is truncated once it passes a megabyte rather than rotated. These
/// logs exist to answer "did the recorder come up at login?", and a
/// rotation scheme is machinery for a question that only needs the recent past.
fn init_logging(background: bool) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CHRONICLE_LOG")
        .unwrap_or_else(|_| "chronicled=info,chronicle_core=info".into());

    if !background {
        tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
        return Ok(());
    }

    let path = log_path()?;
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 1_048_576 {
        let _ = std::fs::remove_file(&path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    Ok(())
}

fn arg_num(args: &[String], i: usize) -> Option<i64> {
    args.get(i)?.parse().ok()
}

fn open_store() -> Result<Store> {
    Store::open(Store::default_path()?)
}

// ── the recorder ────────────────────────────────────────────────────────

fn run(background: bool) -> Result<()> {
    // Let go of the console Windows hands a Run-key launch, or it sits on the
    // desktop for as long as the recorder lives — which is all day.
    if background {
        startup::detach_console();
    }

    // One recorder per machine. Two would double every observation and leave
    // two sessionisers disagreeing about where a session ends.
    let _lock = match startup::acquire_lock() {
        Some(lock) => lock,
        None => {
            tracing::info!("another recorder is already running — nothing to do");
            if !background {
                eprintln!("Chronicle is already recording. Only one recorder runs at a time.");
            }
            return Ok(());
        }
    };

    let store = open_store()?;
    let rules = SessionRules::default();

    // A missing clean-shutdown marker means the last run did not get to say
    // goodbye. That session is exactly the one the user will come looking for.
    if store.crashed_last_run()? {
        if let Some(open) = store.open_session()? {
            let ended = store
                .last_observation_at(open.id)?
                .or(store.last_heartbeat()?)
                .unwrap_or_else(Utc::now);
            store.close_session(open.id, ended, open.active_seconds, EndReason::Interrupted)?;
            tracing::warn!(
                session = open.id,
                title = %open.title,
                ended = %ended.with_timezone(&Local).format("%a %d %b, %-I:%M %p"),
                "recovered an interrupted session"
            );
        }
    }
    // A previous update left the binary it replaced beside the new one; it is
    // only unlocked now that nothing is running from it.
    update::sweep_retired();

    store.mark_running()?;
    // Beat once immediately. The window decides "is the recorder alive?" from
    // heartbeat freshness, and waiting a whole minute for the first one makes
    // a running recorder look stopped.
    store.beat(Utc::now())?;

    let mut sampler = Sampler::new(store.policies()?);

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("installing the shutdown handler")?;
    }

    tracing::info!(db = %Store::default_path()?.display(), "recording");

    let started_at = Utc::now();
    let mut last_update_check = Utc::now();
    let mut last_key = String::new();
    let mut last_write = Utc::now() - Duration::seconds(HEARTBEAT_SECS);
    let mut last_sessionise = Utc::now();
    let mut last_policy_reload = Utc::now();
    let mut was_idle = false;

    while !stop.load(Ordering::SeqCst) {
        let now = Utc::now();

        // Someone — an update, most likely — has asked this recorder to stand
        // down. Leaving by the front door means the session in progress ends
        // properly rather than being recovered as interrupted.
        if store.shutdown_requested(started_at).unwrap_or(false) {
            tracing::info!("stopping at request");
            break;
        }

        let idle = chronicle_observer::idle_seconds();
        if idle >= IDLE_CUTOFF_SECS || chronicle_observer::is_locked() {
            if !was_idle {
                tracing::debug!(idle, "paused — idle or locked");
                was_idle = true;
            }
        } else {
            was_idle = false;
            if let Some(obs) = sampler.sample(now) {
                // Write on a change of window, or on the heartbeat.
                let key = format!("{}\u{1}{}", obs.app_id, obs.title);
                let due = (now - last_write).num_seconds() >= HEARTBEAT_SECS;
                if key != last_key || due {
                    store.record(&obs)?;
                    last_key = key;
                    last_write = now;
                }
            }
        }

        if (now - last_sessionise).num_seconds() >= SESSIONISE_SECS {
            match sessionize::run(&store, &rules, now) {
                Ok(n) if n > 0 => tracing::debug!(sessions = n, "sessionised"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "sessioniser failed"),
            }
            store.beat(now)?;
            last_sessionise = now;
        }

        // The one piece of network in the product, and only if asked for.
        if (now - last_update_check).num_seconds() >= 3600 {
            last_update_check = now;
            check_for_updates(&store, now);
        }

        // Pick up settings changes without a restart.
        if (now - last_policy_reload).num_seconds() >= 300 {
            if let Ok(p) = store.policies() {
                sampler.set_policies(p);
            }
            let _ = store.prune(retention_days(&store));
            last_policy_reload = now;
        }

        std::thread::sleep(POLL);
    }

    // Close the books properly, so the next launch knows this was not a crash.
    let now = Utc::now();
    let _ = sessionize::run(&store, &rules, now);
    store.beat(now)?;
    store.mark_clean_shutdown()?;
    tracing::info!("stopped cleanly");
    Ok(())
}

/// The automatic half of the updater, run from the recorder's loop.
///
/// Does nothing at all unless the user turned it on, and even then contacts
/// the network at most once a day. A failure is logged and forgotten: an
/// unreachable GitHub is not a reason to stop recording.
fn check_for_updates(store: &Store, now: DateTime<Utc>) {
    let mode = update::auto_setting(store);
    if mode == update::Auto::Off || !update::check_due(store, now) {
        return;
    }

    let release = match update::latest() {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "update check failed");
            update::mark_checked(store, now, None);
            return;
        }
    };

    if release.version <= update::current() {
        update::mark_checked(store, now, None);
        return;
    }
    update::mark_checked(store, now, Some(release.version));
    tracing::info!(version = %release.version, "a newer Chronicle is available");

    if mode != update::Auto::Install {
        return;
    }
    match update::install(&release) {
        // The new binary is on disk but this process is still the old one, so
        // it asks itself to stop; the next start is the new version.
        Ok(done) => {
            tracing::info!(version = %done.version, files = ?done.files, "installed an update");
            let _ = store.request_shutdown(now);
        }
        Err(e) => tracing::warn!(error = %e, "could not install the update"),
    }
}

fn retention_days(store: &Store) -> i64 {
    store
        .meta_get("retention_days")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90)
}

// ── inspection ──────────────────────────────────────────────────────────

/// What Chronicle can see in the window in front of you, right now. The
/// fastest way to tell whether an enricher works on a real application.
fn doctor() -> Result<()> {
    let store = open_store()?;
    let policies = store.policies()?;

    let Some(sample) = chronicle_observer::foreground() else {
        println!("Nothing is focused.");
        return Ok(());
    };

    println!("Foreground window");
    println!("  app id     {}", sample.app_id);
    println!("  title      {}", sample.title);
    println!("  pid        {}", sample.pid);
    println!(
        "  exe        {}",
        sample.exe_path.as_deref().unwrap_or("(unreadable)")
    );
    if let Some(f) = sample.frame {
        println!("  frame      {}x{} at {},{}", f.w, f.h, f.x, f.y);
    }
    println!(
        "  display    {}",
        sample.display_id.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "  idle       {}s   locked: {}",
        chronicle_observer::idle_seconds(),
        chronicle_observer::is_locked()
    );

    let policy = policies.policy_for(&sample.app_id);
    println!("\nPolicy       {policy:?}");
    if policy == CapturePolicy::Ignore {
        println!("This app is not recorded, so nothing below would be stored.");
    }

    let sampler = Sampler::new(policies);
    match sampler.observe(&sample, Utc::now()) {
        None => println!("\nNothing would be recorded."),
        Some(obs) => {
            println!("\nWould record");
            println!("  title      {}", obs.title);
            println!("  category   {:?}", obs.category);
            println!("  artifacts  {}", obs.artifacts.len());
            for (i, a) in obs.artifacts.iter().enumerate() {
                let marker = if i == 0 { "focused" } else { "present" };
                println!("    [{marker}] {:?}  {}", a.kind, a.display_name);
                println!("              {}", a.uri);
                if let Some(r) = &a.project_root {
                    println!("              root: {r}");
                }
                for (k, v) in &a.state {
                    println!("              {k}: {v}");
                }
            }
        }
    }
    Ok(())
}

/// Run the enrichers over every open window and report what each one yields.
/// The fastest way to see coverage without clicking through every app.
fn scan() -> Result<()> {
    let store = open_store()?;
    let policies = store.policies()?;
    let sampler = Sampler::new(store.policies()?);
    let now = Utc::now();

    let mut windows = chronicle_observer::all_windows();
    windows.sort_by(|a, b| a.app_id.cmp(&b.app_id));
    windows.dedup_by(|a, b| a.app_id == b.app_id && a.title == b.title);

    let (mut enriched, mut bare, mut skipped) = (0usize, 0usize, 0usize);

    for w in &windows {
        let policy = policies.policy_for(&w.app_id);
        if policy == CapturePolicy::Ignore {
            println!("  --  {:<24} not recorded (deny list)", w.app_id);
            skipped += 1;
            continue;
        }

        let Some(obs) = sampler.observe(w, now) else {
            // The only other way `observe` declines is a shell surface: the
            // desktop, the taskbar, Alt+Tab. Saying so beats a silent gap in a
            // command whose whole job is explaining coverage.
            println!("  --  {:<24} not recorded (part of the shell, not work)", w.app_id);
            skipped += 1;
            continue;
        };

        // Anything beyond the bare `app://` entry means an enricher fired.
        let real: Vec<_> = obs
            .artifacts
            .iter()
            .filter(|a| a.kind != chronicle_core::ArtifactKind::App)
            .collect();

        if real.is_empty() {
            bare += 1;
            println!("  ..  {:<24} {}", w.app_id, truncate(&obs.title, 46));
            println!("      app only — no project, file or page resolved");
        } else {
            enriched += 1;
            println!("  OK  {:<24} {}", w.app_id, truncate(&obs.title, 46));
            for a in real {
                println!("      {:?}  {}", a.kind, a.display_name);
                println!("        {}", a.uri);
                if let Some(r) = &a.project_root {
                    println!("        root: {r}");
                }
                for (k, v) in &a.state {
                    println!("        {k}: {v}");
                }
            }
        }
    }

    println!(
        "
{} window(s): {enriched} enriched, {bare} app-only, {skipped} not recorded",
        windows.len()
    );
    if bare > 0 {
        println!("App-only windows restore as \"launch the app\", not \"reopen this work\".");
    }
    Ok(())
}

fn status() -> Result<()> {
    let path = Store::default_path()?;
    let store = Store::open(&path)?;
    let (sessions, artifacts, observations) = store.counts()?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    println!("Chronicle");
    println!("  database     {}", path.display());
    println!("  size         {:.1} MB", size as f64 / 1_048_576.0);
    println!("  device       {}", store.device_id());
    println!("  sessions     {sessions}");
    println!("  artifacts    {artifacts}");
    println!("  observations {observations}");
    println!("  retention    {} days", retention_days(&store));
    println!("  autostart    {}", startup::describe());
    println!("  version      {}", update::describe(&store));
    match store.last_heartbeat()? {
        Some(t) => println!("  last beat    {}", stamp(t)),
        None => println!("  last beat    never"),
    }
    println!(
        "  last run     {}",
        if store.crashed_last_run()? {
            "did not shut down cleanly"
        } else {
            "clean"
        }
    );
    if let Some(open) = store.open_session()? {
        println!("  in progress  {} (since {})", open.title, stamp(open.started_at));
    }
    Ok(())
}

fn sessions(limit: usize) -> Result<()> {
    let store = open_store()?;
    let list = store.recent_sessions(limit)?;
    if list.is_empty() {
        println!("No sessions yet. Run `chronicled run` and do some work.");
        return Ok(());
    }
    let mut last_day = String::new();
    for s in list {
        let day = day_label(s.started_at);
        if day != last_day {
            println!("\n{day}");
            last_day = day;
        }
        let end = s
            .ended_at
            .map(|e| e.with_timezone(&Local).format("%-I:%M %p").to_string())
            .unwrap_or_else(|| "now".into());
        let flag = match s.end_reason {
            EndReason::Interrupted => "  [interrupted]",
            EndReason::Open => "  [recording]",
            _ => "",
        };
        println!(
            "  {:>4}  {:<38} {} – {}  {}{}",
            s.id,
            truncate(&s.title, 38),
            s.started_at.with_timezone(&Local).format("%-I:%M %p"),
            end,
            duration(s.active_seconds),
            flag
        );
    }
    println!();
    Ok(())
}

fn show(id: i64) -> Result<()> {
    let store = open_store()?;
    let Some(detail) = store.session_detail(id)? else {
        println!("No session {id}.");
        return Ok(());
    };
    let s = &detail.session;

    println!("{}", s.title);
    println!(
        "{}  ·  {} – {}  ·  {} active",
        day_label(s.started_at),
        s.started_at.with_timezone(&Local).format("%-I:%M %p"),
        s.ended_at
            .map(|e| e.with_timezone(&Local).format("%-I:%M %p").to_string())
            .unwrap_or_else(|| "now".into()),
        duration(s.active_seconds),
    );
    if s.end_reason == EndReason::Interrupted {
        println!("Ended by an unclean shutdown.");
    }

    let mut by_category: Vec<(Category, Vec<_>)> = Vec::new();
    for a in &detail.artifacts {
        match by_category.iter_mut().find(|(c, _)| *c == a.category) {
            Some((_, v)) => v.push(a),
            None => by_category.push((a.category, vec![a])),
        }
    }
    by_category.sort_by_key(|(c, _)| c.launch_order());

    for (cat, items) in by_category {
        println!("\n{}", cat.label().to_uppercase());
        for a in items {
            println!(
                "  {:<44} {:>8}",
                truncate(&a.display_name, 44),
                duration(a.focus_seconds)
            );
            println!("      {}", a.uri);
            if let Some(r) = &a.project_root {
                println!("      root: {r}");
            }
            for (k, v) in &a.state {
                println!("      {k}: {v}");
            }
        }
    }
    println!();
    Ok(())
}

fn search(query: &str) -> Result<()> {
    if query.trim().is_empty() {
        println!("usage: chronicled search <words>");
        return Ok(());
    }
    let store = open_store()?;
    let hits = store.search(query, 20)?;
    if hits.is_empty() {
        println!("Nothing matched \"{query}\".");
        return Ok(());
    }
    println!("{} session(s) matching \"{}\"\n", hits.len(), query);
    for s in hits {
        println!(
            "  {:>4}  {:<38} {}  {}",
            s.id,
            truncate(&s.title, 38),
            day_label(s.started_at),
            s.started_at.with_timezone(&Local).format("%-I:%M %p")
        );
    }
    println!();
    Ok(())
}

/// What the SNSS reader makes of the browsers actually installed here. The
/// browser equivalent of `doctor`.
fn tabs() -> Result<()> {
    let browsers = ["chrome.exe", "msedge.exe", "brave.exe", "vivaldi.exe"];
    let mut found_any = false;

    for app in browsers {
        let state = chronicle_observer::chrome::read_state(app);
        let windows = &state.windows;
        if windows.is_empty() {
            continue;
        }
        found_any = true;
        let total: usize = windows.iter().map(|w| w.tabs.len()).sum();
        println!(
            "
{}  —  {} window(s), {} tab(s)",
            chronicle_core::policy::display_name(app),
            windows.len(),
            total
        );
        if let Some(t) = state.as_of {
            let mins = t.elapsed().map(|d| d.as_secs() / 60).unwrap_or(0);
            println!("  as of {mins} minute(s) ago");
        }
        if state.live_file_locked {
            println!(
                "  the live session file is locked by the running browser, so this is \
                 the previous one"
            );
        }
        for w in windows {
            println!("  window {}", w.window_id);
            for t in &w.tabs {
                let pin = if t.pinned { " [pinned]" } else { "" };
                println!("    {:>2}. {}{}", t.index, truncate(&t.title, 60), pin);
                match chronicle_core::redact::redact_url(&t.url) {
                    Some(u) => println!("        {}", truncate(&u, 92)),
                    None => println!("        (dropped by redaction — not safe to store)"),
                }
            }
        }
    }

    if !found_any {
        println!("No browser session files could be read.");
        println!("Chrome writes them a few seconds after a tab changes, so open a tab and retry.");
    } else {
        println!("
Incognito windows are never written to these files, so they cannot appear here.");
    }
    Ok(())
}

// ── restore ─────────────────────────────────────────────────────────────

fn plan(id: i64) -> Result<()> {
    let store = open_store()?;
    let Some(detail) = store.session_detail(id)? else {
        println!("No session {id}.");
        return Ok(());
    };
    let plan = chronicle_restore::plan(&detail);
    print_plan(&plan);
    Ok(())
}

fn print_plan(plan: &chronicle_restore::Plan) {
    use chronicle_restore::Action;

    let (ready, total) = plan.readiness();
    println!("Restore plan — {}", plan.session_title);
    println!("{ready} of {total} items ready, {} need you
", total - ready);

    for item in &plan.items {
        println!(
            "  [{}] {:<38} {}",
            if item.selected { "x" } else { " " },
            truncate(&item.label, 38),
            item.fidelity.label()
        );
        match &item.action {
            Action::Launch { program, args } => {
                println!("      run   {} {}", program.display(), args.join(" "));
            }
            Action::Shell { path } => println!("      open  {}", path.display()),
            Action::Manual { instruction } => {
                for line in instruction.lines().take(4) {
                    println!("      you   {line}");
                }
            }
        }
        if let Some(n) = &item.note {
            println!("      note  {n}");
        }
    }
    println!();
}

fn restore(id: i64, dry_run: bool) -> Result<()> {
    let store = open_store()?;
    let Some(detail) = store.session_detail(id)? else {
        println!("No session {id}.");
        return Ok(());
    };
    let plan = chronicle_restore::plan(&detail);
    print_plan(&plan);

    if dry_run {
        println!("Dry run — nothing was started. Add --go to restore for real.");
        return Ok(());
    }

    let outcome = chronicle_restore::execute(
        &plan,
        &chronicle_restore::RestoreOptions { dry_run: false, stagger_ms: 400 },
    );

    println!(
        "Restore receipt — {} of {} in {:.1}s
",
        outcome.restored(),
        outcome.items.len(),
        outcome.elapsed_ms as f64 / 1000.0
    );
    for i in &outcome.items {
        println!(
            "  {} {:<38} {}",
            if i.ok { "ok  " } else { "--  " },
            truncate(&i.label, 38),
            i.message.lines().next().unwrap_or("")
        );
    }
    println!("
Undo is not implemented yet; close anything you did not want by hand.");
    Ok(())
}

/// Every application Chronicle could be asked about on this machine: what the
/// registry says is installed, plus everything the recorder has already seen.
///
/// The second half matters for Store apps — WhatsApp, Windows Terminal — which
/// live under `WindowsApps` and appear in no readable registry view, but which
/// Chronicle has watched running and therefore knows perfectly well are here.
fn discovered_apps(store: &Store) -> Vec<chronicle_core::policy::Discovered> {
    use chronicle_core::policy::Discovered;
    let mut out: Vec<Discovered> = chronicle_observer::installed::scan()
        .into_iter()
        .map(|a| Discovered {
            app_id: a.app_id,
            display_name: a.display_name,
        })
        .collect();

    let seen: std::collections::HashSet<String> = out.iter().map(|d| d.app_id.clone()).collect();
    for (app_id, display_name) in store.known_apps().unwrap_or_default() {
        if seen.contains(&app_id)
            || chronicle_observer::installed::is_supporting_software(&display_name)
        {
            continue;
        }
        out.push(Discovered {
            app_id,
            display_name: Some(display_name),
        });
    }
    out
}

fn sources() -> Result<()> {
    let store = open_store()?;
    let policies = store.policies()?;

    let everything = std::env::args().any(|a| a == "--all");
    let rows = if everything {
        chronicle_core::policy::catalogue(&policies)
    } else {
        chronicle_core::policy::sources(&policies, &discovered_apps(&store))
    };

    let mut hidden: Vec<String> = Vec::new();
    let mut last = String::new();
    for e in rows {
        // Not on this machine, so it cannot be recorded whatever the setting.
        // Naming them in one line keeps "are password managers excluded?"
        // answerable without a screen of software the user does not have.
        if !everything && !e.installed {
            hidden.push(e.display_name.clone());
            continue;
        }
        let cat = e.category.label().to_string();
        if cat != last {
            println!("\n{}", cat.to_uppercase());
            last = cat;
        }
        let state = match e.policy {
            CapturePolicy::Full => "full",
            CapturePolicy::TitlesOff => "titles off",
            CapturePolicy::Ignore => "ignore",
        };
        // A permanently denied app can carry a user override that does nothing;
        // saying "you turned this on" about a password manager Chronicle will
        // never record either way would be alarming and untrue.
        let overruled = e.auto_denied && e.policy == CapturePolicy::Ignore;
        let why = match (e.auto_denied, e.configured, overruled) {
            (true, _, true) => "  (never recorded, whatever the setting says)",
            (true, true, _) => "  (you turned this on; the shipped default is off)",
            (true, false, _) => "  (off by default)",
            (false, true, _) => "  (your setting)",
            (false, false, _) => "",
        };
        println!("  {:<24} {:<12}{}", e.display_name, state, why);
    }
    println!();
    if !everything {
        if !hidden.is_empty() {
            println!("Not installed here, so never recorded either way:");
            println!("  {}", hidden.join(", "));
            println!();
        }
        println!("`chronicled sources --all` lists the whole catalogue instead.");
    }
    Ok(())
}

fn forget(hours: i64) -> Result<()> {
    let store = open_store()?;
    let to = Utc::now();
    let from = to - Duration::hours(hours);
    let n = store.forget_range(from, to)?;
    println!(
        "Deleted {n} observations from the last {hours} hour(s). This cannot be undone."
    );
    Ok(())
}

// ── formatting ──────────────────────────────────────────────────────────

fn stamp(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%a %d %b, %-I:%M %p").to_string()
}

fn day_label(t: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local).date_naive();
    let today = Local::now().date_naive();
    match (today - local).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => local.format("%A").to_string(),
        _ => local.format("%a %d %b").to_string(),
    }
}

fn duration(seconds: i64) -> String {
    let (h, m) = (seconds / 3600, (seconds % 3600) / 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{seconds}s")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

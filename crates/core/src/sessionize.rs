//! The sessioniser: turns a day of window switching into four things the user
//! recognises.
//!
//! Every threshold here is a default and every one is meant to be editable.
//! People who work in twenty-minute bursts and people who sit on one problem
//! for six hours both have to recognise their own day in the list.

use anyhow::Result;
use chrono::{DateTime, Duration, Timelike, Utc};
use std::collections::{HashMap, HashSet};

use crate::model::*;
use crate::store::{RawObs, Store};

#[derive(Debug, Clone)]
pub struct SessionRules {
    /// No focus change for this long closes the session.
    pub idle_gap: Duration,
    /// A previously dominant project root reappearing inside this window
    /// stitches back instead of starting a new session.
    pub stitch_window: Duration,
    /// Fraction of the focused artifact set that must turn over to count as a
    /// context switch.
    pub turnover_threshold: f64,
    /// The window either side of a candidate split point.
    pub turnover_window: Duration,
    /// Below this, it was a glance, not a session.
    pub min_duration: Duration,
    pub min_artifacts: usize,
    /// A single observation may never be credited with more than this.
    pub max_credit: Duration,
}

impl Default for SessionRules {
    fn default() -> Self {
        Self {
            idle_gap: Duration::minutes(12),
            stitch_window: Duration::minutes(45),
            turnover_threshold: 0.70,
            turnover_window: Duration::minutes(10),
            min_duration: Duration::minutes(4),
            min_artifacts: 3,
            max_credit: Duration::seconds(60),
        }
    }
}

/// A candidate session, before it is written.
#[derive(Debug, Clone)]
pub struct Segment {
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub active_seconds: i64,
    pub end_reason: EndReason,
    pub title: String,
    pub title_source: TitleSource,
    pub obs_ids: Vec<i64>,
    pub artifact_count: usize,
}

/// Split an ordered run of observations into sessions.
pub fn segment(obs: &[RawObs], rules: &SessionRules) -> Vec<Segment> {
    if obs.is_empty() {
        return Vec::new();
    }

    // 1 — cut on idle gaps.
    let mut runs: Vec<&[RawObs]> = Vec::new();
    let mut start = 0usize;
    for i in 1..obs.len() {
        if obs[i].at - obs[i - 1].at > rules.idle_gap {
            runs.push(&obs[start..i]);
            start = i;
        }
    }
    runs.push(&obs[start..]);

    // 2 — cut each run again where the working set turns over.
    let mut pieces: Vec<(&[RawObs], EndReason)> = Vec::new();
    for run in runs {
        let cuts = context_switches(run, rules);
        let mut from = 0usize;
        for c in cuts {
            pieces.push((&run[from..c], EndReason::ContextSwitch));
            from = c;
        }
        pieces.push((&run[from..], EndReason::Idle));
    }

    // 3 — stitch neighbours that are obviously the same work resumed.
    let mut stitched: Vec<(Vec<&RawObs>, EndReason)> = Vec::new();
    for (piece, reason) in pieces {
        if piece.is_empty() {
            continue;
        }
        if let Some((prev, prev_reason)) = stitched.last_mut() {
            let gap = piece[0].at - prev.last().map(|o| o.at).unwrap_or(piece[0].at);
            if gap <= rules.stitch_window && shares_dominant_root(prev, piece) {
                prev.extend(piece.iter());
                *prev_reason = reason;
                continue;
            }
        }
        stitched.push((piece.iter().collect(), reason));
    }

    // 4 — drop the glances, and name what is left.
    stitched
        .into_iter()
        .filter_map(|(rows, reason)| build(&rows, reason, rules))
        .collect()
}

fn build(rows: &[&RawObs], reason: EndReason, rules: &SessionRules) -> Option<Segment> {
    let first = rows.first()?;
    let last = rows.last()?;
    let span = last.at - first.at;
    let artifacts: HashSet<ArtifactId> = rows.iter().filter_map(|o| o.artifact_id).collect();

    if span < rules.min_duration || artifacts.len() < rules.min_artifacts {
        return None;
    }

    let mut active_ms = 0i64;
    for w in rows.windows(2) {
        active_ms += (w[1].at - w[0].at).num_milliseconds().min(rules.max_credit.num_milliseconds());
    }

    let (title, title_source) = title_for(rows);
    Some(Segment {
        started_at: first.at,
        ended_at: last.at,
        active_seconds: active_ms / 1000,
        end_reason: reason,
        title,
        title_source,
        obs_ids: rows.iter().map(|o| o.id).collect(),
        artifact_count: artifacts.len(),
    })
}

/// Indices where the working set turns over enough to be different work.
fn context_switches(run: &[RawObs], rules: &SessionRules) -> Vec<usize> {
    let mut cuts = Vec::new();
    if run.len() < 8 {
        return cuts;
    }
    let w = rules.turnover_window;
    let mut last_cut_at = run[0].at;

    for i in 2..run.len() - 2 {
        let t = run[i].at;
        if t - last_cut_at < w {
            continue;
        }
        let before: HashSet<ArtifactId> = run[..i]
            .iter()
            .filter(|o| t - o.at <= w)
            .filter_map(|o| o.artifact_id)
            .collect();
        let after: HashSet<ArtifactId> = run[i..]
            .iter()
            .filter(|o| o.at - t <= w)
            .filter_map(|o| o.artifact_id)
            .collect();
        if before.len() < 2 || after.len() < 2 {
            continue;
        }
        let shared = before.intersection(&after).count() as f64;
        let union = before.union(&after).count() as f64;
        let turnover = 1.0 - (shared / union);

        // A shared project root means it is the same work seen from a
        // different angle, however much the window set changed.
        let roots_before = roots(&run[..i]);
        let roots_after = roots(&run[i..]);
        let shared_root = roots_before.intersection(&roots_after).next().is_some();

        if turnover >= rules.turnover_threshold && !shared_root {
            cuts.push(i);
            last_cut_at = t;
        }
    }
    cuts
}

fn roots(rows: &[RawObs]) -> HashSet<String> {
    rows.iter().filter_map(|o| o.project_root.clone()).collect()
}

fn shares_dominant_root(a: &[&RawObs], b: &[RawObs]) -> bool {
    let Some(ra) = dominant_root(a.iter().copied()) else {
        return false;
    };
    let Some(rb) = dominant_root(b.iter()) else {
        return false;
    };
    ra == rb
}

/// The project root that held most of the attention, if any single one held
/// at least 40% of the observations that had a root at all.
fn dominant_root<'a>(rows: impl Iterator<Item = &'a RawObs>) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut total = 0usize;
    for o in rows {
        if let Some(r) = o.project_root.as_deref() {
            *counts.entry(r).or_default() += 1;
            total += 1;
        }
    }
    if total == 0 {
        return None;
    }
    let (root, n) = counts.into_iter().max_by_key(|(_, n)| *n)?;
    ((n as f64 / total as f64) >= 0.40).then(|| root.to_string())
}

/// Names come from structure, never from the contents of documents.
fn title_for(rows: &[&RawObs]) -> (String, TitleSource) {
    // 1 — the dominant repository or project root.
    if let Some(root) = dominant_root(rows.iter().copied()) {
        let base = root
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&root);
        if !base.is_empty() {
            return (humanise(base), TitleSource::ProjectRoot);
        }
    }

    // 2 — the dominant design file.
    if let Some(name) = dominant_by(rows, Category::Design) {
        return (name, TitleSource::DesignFile);
    }

    // 3 — the most-visited site.
    let mut hosts: HashMap<String, usize> = HashMap::new();
    for o in rows.iter().filter(|o| o.category == Category::Browser) {
        if let Some(h) = host_of(&o.artifact_uri) {
            *hosts.entry(h).or_default() += 1;
        }
    }
    if let Some((host, _)) = hosts.into_iter().max_by_key(|(_, n)| *n) {
        return (format!("Research — {host}"), TitleSource::Domain);
    }

    // 4 — the app mix and the time of day.
    let when = match rows[0].at.with_timezone(&chrono::Local).hour() {
        5..=11 => "Morning",
        12..=16 => "Afternoon",
        17..=21 => "Evening",
        _ => "Late night",
    };
    let app = rows
        .iter()
        .fold(HashMap::<&str, usize>::new(), |mut m, o| {
            *m.entry(o.app_id.as_str()).or_default() += 1;
            m
        })
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(a, _)| crate::policy::display_name(a))
        .unwrap_or_else(|| "Mixed".into());
    (format!("{when} — {app}"), TitleSource::Fallback)
}

fn dominant_by(rows: &[&RawObs], cat: Category) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for o in rows.iter().filter(|o| o.category == cat) {
        if !o.artifact_name.is_empty() {
            *counts.entry(o.artifact_name.as_str()).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(n, _)| n.to_string())
}

fn host_of(uri: &str) -> Option<String> {
    let rest = uri.split_once("://")?.1;
    let host = rest.split('/').next()?.trim_start_matches("www.");
    (!host.is_empty()).then(|| host.to_string())
}

/// `spotted-android` becomes `Spotted Android`. Recognisable, and honest about
/// the fact that it came from a folder name.
fn humanise(s: &str) -> String {
    s.split(['-', '_', ' ', '.'])
        .filter(|w| !w.is_empty())
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

/// Run one incremental pass. Cheap, idempotent, and safe to re-run after a
/// crash — which is exactly when it matters most.
pub fn run(store: &Store, rules: &SessionRules, now: DateTime<Utc>) -> Result<usize> {
    let lookback = now - Duration::hours(24);
    let rows = store.unsessioned_observations(lookback)?;
    if rows.is_empty() {
        return Ok(0);
    }

    let segments = segment(&rows, rules);
    let mut written = 0usize;

    // The open session, if any, and when it last saw anything.
    let open = store.open_session()?;
    let open_last = match &open {
        Some(s) => store.last_observation_at(s.id)?,
        None => None,
    };

    for (i, seg) in segments.iter().enumerate() {
        let is_last = i == segments.len() - 1;
        let still_live = is_last && now - seg.ended_at <= rules.idle_gap;

        // Continue the open session when this segment picks up where it left off.
        let extend = match (&open, open_last) {
            (Some(s), Some(last)) if seg.started_at - last <= rules.idle_gap => Some(s.id),
            _ => None,
        };

        let id = match extend {
            Some(id) => id,
            None => store.create_session(&seg.title, seg.title_source, seg.started_at)?,
        };

        store.attach_observations(id, &seg.obs_ids)?;
        store.rebuild_session_artifacts(id)?;

        if still_live {
            store.update_progress(id, seg.active_seconds)?;
        } else {
            store.close_session(id, seg.ended_at, seg.active_seconds, seg.end_reason)?;
        }
        written += 1;
    }

    // Close a stale open session that this pass produced nothing for.
    if let (Some(s), Some(last)) = (&open, open_last) {
        if now - last > rules.idle_gap && s.ended_at.is_none() {
            store.close_session(s.id, last, s.active_seconds, EndReason::Idle)?;
        }
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(id: i64, min: i64, app: &str, artifact: i64, root: Option<&str>) -> RawObs {
        RawObs {
            id,
            at: Utc::now() - Duration::hours(6) + Duration::minutes(min),
            app_id: app.into(),
            title: "t".into(),
            artifact_id: Some(artifact),
            artifact_uri: format!("file:///a/{artifact}"),
            artifact_name: format!("a{artifact}"),
            project_root: root.map(str::to_string),
            category: Category::Editor,
        }
    }

    #[test]
    fn a_twelve_minute_gap_ends_the_session() {
        let rules = SessionRules::default();
        let rows: Vec<RawObs> = (0..10)
            .map(|i| raw(i, i, "code.exe", i % 4, Some("C:/work/alpha")))
            .chain((0..10).map(|i| raw(100 + i, 40 + i, "code.exe", 10 + i % 4, Some("C:/work/beta"))))
            .collect();
        let segs = segment(&rows, &rules);
        assert_eq!(segs.len(), 2, "one gap should produce two sessions");
        assert_eq!(segs[0].title, "Alpha");
        assert_eq!(segs[1].title, "Beta");
    }

    #[test]
    fn returning_to_the_same_project_stitches_back() {
        let rules = SessionRules::default();
        let rows: Vec<RawObs> = (0..10)
            .map(|i| raw(i, i, "code.exe", i % 4, Some("C:/work/alpha")))
            .chain((0..10).map(|i| raw(100 + i, 35 + i, "code.exe", i % 4, Some("C:/work/alpha"))))
            .collect();
        let segs = segment(&rows, &rules);
        assert_eq!(segs.len(), 1, "same root inside 45 minutes is one session");
    }

    #[test]
    fn a_glance_is_not_a_session() {
        let rules = SessionRules::default();
        let rows: Vec<RawObs> = (0..3).map(|i| raw(i, i, "code.exe", i, None)).collect();
        assert!(segment(&rows, &rules).is_empty());
    }

    #[test]
    fn titles_come_from_the_project_root() {
        let rules = SessionRules::default();
        let rows: Vec<RawObs> = (0..12)
            .map(|i| raw(i, i, "code.exe", i % 5, Some("C:/work/spotted-android")))
            .collect();
        let segs = segment(&rows, &rules);
        assert_eq!(segs[0].title, "Spotted Android");
        assert_eq!(segs[0].title_source, TitleSource::ProjectRoot);
    }
}

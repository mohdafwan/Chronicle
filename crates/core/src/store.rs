//! The store: one SQLite file, on this machine, holding everything Chronicle
//! knows. Portable, inspectable, and the only thing a user has to delete to
//! make Chronicle forget.
//!
//! Timestamps are unix milliseconds throughout. Two logs feed the sessioniser:
//! `observations` (which window was focused) and `sightings` (which artifacts
//! were present). Keeping them apart is what lets Chrome contribute seven tabs
//! to a session without claiming all seven were focused.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::*;
use crate::policy::{CapturePolicy, Policies};

pub const SCHEMA_VERSION: i64 = 1;

/// How long a single focused observation may be credited with, when the next
/// observation is late. Stops a sleep or a stall from inflating focus time.
const MAX_CREDIT_MS: i64 = 60_000;

pub struct Store {
    conn: Connection,
    device_id: String,
}

impl Store {
    /// `%APPDATA%\Chronicle\data\chronicle.db` on Windows.
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("dev", "chronicle", "Chronicle")
            .context("could not resolve an application data directory")?;
        Ok(dirs.data_dir().join("chronicle.db"))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        let mut store = Self { conn, device_id: String::new() };
        store.migrate()?;
        store.device_id = store.ensure_device_id()?;
        Ok(store)
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    // ── schema ──────────────────────────────────────────────────────────

    fn migrate(&mut self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);

        if current < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
            self.conn.pragma_update(None, "user_version", 1)?;
        }
        Ok(())
    }

    fn ensure_device_id(&self) -> Result<String> {
        if let Some(id) = self.meta_get("device_id")? {
            return Ok(id);
        }
        // Enough entropy to distinguish this machine's history from another's.
        // Not an identity, not sent anywhere.
        let id = format!(
            "{:x}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64
                ^ (std::process::id() as u64).rotate_left(32)
        );
        self.meta_set("device_id", &id)?;
        Ok(id)
    }

    // ── meta / settings ─────────────────────────────────────────────────

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Load per-app capture overrides the user has set.
    pub fn policies(&self) -> Result<Policies> {
        let mut stmt = self
            .conn
            .prepare("SELECT app_id, policy FROM app_policy")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (id, p) = row?;
            let policy = match p.as_str() {
                "titles_off" => CapturePolicy::TitlesOff,
                "ignore" => CapturePolicy::Ignore,
                _ => CapturePolicy::Full,
            };
            map.insert(id, policy);
        }
        Ok(Policies::with_overrides(map))
    }

    pub fn set_policy(&self, app_id: &str, policy: CapturePolicy) -> Result<()> {
        let p = match policy {
            CapturePolicy::Full => "full",
            CapturePolicy::TitlesOff => "titles_off",
            CapturePolicy::Ignore => "ignore",
        };
        self.conn.execute(
            "INSERT INTO app_policy (app_id, policy) VALUES (?1, ?2)
             ON CONFLICT(app_id) DO UPDATE SET policy = excluded.policy",
            params![app_id.to_ascii_lowercase(), p],
        )?;
        Ok(())
    }

    // ── the write path ──────────────────────────────────────────────────

    /// Record one observation. Callers must have applied policy and redaction
    /// already; this function writes what it is given.
    pub fn record(&self, obs: &Observation) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let at = ms(obs.at);

        tx.execute(
            "INSERT INTO apps (app_id, display_name, category, exe_path, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(app_id) DO UPDATE SET
                display_name = excluded.display_name,
                category     = excluded.category,
                exe_path     = COALESCE(excluded.exe_path, apps.exe_path),
                last_seen    = excluded.last_seen",
            params![obs.app_id, obs.app_name, obs.category.as_str(), obs.exe_path, at],
        )?;

        // The first artifact is the focused one; the rest were merely present.
        let mut primary: Option<ArtifactId> = None;
        for (i, a) in obs.artifacts.iter().enumerate() {
            let id = upsert_artifact(&tx, a, &obs.app_id, at)?;
            tx.execute(
                "INSERT INTO sightings (artifact_id, at, focused) VALUES (?1, ?2, ?3)",
                params![id, at, i == 0],
            )?;
            if i == 0 {
                primary = Some(id);
            }
        }

        let frame = obs.frame.as_ref().and_then(|f| serde_json::to_string(f).ok());
        tx.execute(
            "INSERT INTO observations (at, app_id, title, pid, artifact_id, frame, display_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![at, obs.app_id, obs.title, obs.pid, primary, frame, obs.display_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Observations not yet assigned to a session, oldest first.
    pub fn unsessioned_observations(&self, since: DateTime<Utc>) -> Result<Vec<RawObs>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.id, o.at, o.app_id, o.title, o.artifact_id, a.project_root, ap.category,
                    COALESCE(a.uri, ''), COALESCE(a.display_name, '')
             FROM observations o
             LEFT JOIN artifacts a ON a.id = o.artifact_id
             LEFT JOIN apps ap ON ap.app_id = o.app_id
             WHERE o.session_id IS NULL AND o.at >= ?1
             ORDER BY o.at ASC",
        )?;
        let rows = stmt.query_map(params![ms(since)], |r| {
            Ok(RawObs {
                id: r.get(0)?,
                at: dt(r.get(1)?),
                app_id: r.get(2)?,
                title: r.get(3)?,
                artifact_id: r.get(4)?,
                project_root: r.get(5)?,
                category: Category::parse(&r.get::<_, Option<String>>(6)?.unwrap_or_default()),
                artifact_uri: r.get(7)?,
                artifact_name: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ── sessions ────────────────────────────────────────────────────────

    pub fn create_session(
        &self,
        title: &str,
        title_source: TitleSource,
        started_at: DateTime<Utc>,
    ) -> Result<SessionId> {
        self.conn.execute(
            "INSERT INTO sessions (title, title_source, started_at, device_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![title, title_source.as_str(), ms(started_at), self.device_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn close_session(
        &self,
        id: SessionId,
        ended_at: DateTime<Utc>,
        active_seconds: i64,
        reason: EndReason,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?2, active_seconds = ?3, end_reason = ?4
             WHERE id = ?1",
            params![id, ms(ended_at), active_seconds, reason.as_str()],
        )?;
        Ok(())
    }

    /// Update the running total for a session that has not ended yet.
    /// `active_seconds` used to be written only by `close_session`, so a
    /// session still in progress always displayed "0s active".
    pub fn update_progress(&self, id: SessionId, active_seconds: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET active_seconds = ?2 WHERE id = ?1 AND ended_at IS NULL",
            params![id, active_seconds],
        )?;
        Ok(())
    }

    /// Attach observations to a session and roll their artifacts up into
    /// `session_artifacts` with focus time.
    pub fn attach_observations(&self, session: SessionId, obs_ids: &[i64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for chunk in obs_ids.chunks(400) {
            let holes = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE observations SET session_id = ? WHERE id IN ({holes})"
            );
            let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(chunk.len() + 1);
            vals.push(Box::new(session));
            for id in chunk {
                vals.push(Box::new(*id));
            }
            let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
            tx.execute(&sql, refs.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Recompute `session_artifacts` for one session from the raw sighting log.
    ///
    /// Membership comes from the observations actually assigned to this
    /// session, never from a wall-clock window. A window would swallow the
    /// artifacts of any session interleaved with this one, and — while a
    /// session is still open and has no end yet — everything recorded since.
    pub fn rebuild_session_artifacts(&self, session: SessionId) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM session_artifacts WHERE session_id = ?1",
            params![session],
        )?;
        // Focus time is the gap from a focused sighting to this session's next
        // observation, capped so a sleep cannot inflate it.
        tx.execute(
            "INSERT INTO session_artifacts (session_id, artifact_id, focus_seconds, first_seen, last_seen)
             SELECT ?1,
                    s.artifact_id,
                    CAST(COALESCE(SUM(
                      CASE WHEN s.focused = 1 THEN
                        MIN(COALESCE(
                          (SELECT MIN(o.at) FROM observations o
                            WHERE o.session_id = ?1 AND o.at > s.at) - s.at, ?2), ?2)
                      ELSE 0 END
                    ), 0) / 1000 AS INTEGER),
                    MIN(s.at),
                    MAX(s.at)
             FROM sightings s
             WHERE s.at IN (SELECT at FROM observations WHERE session_id = ?1)
             GROUP BY s.artifact_id",
            params![session, MAX_CREDIT_MS],
        )?;
        tx.commit()?;
        self.refresh_search(session)?;
        Ok(())
    }

    pub fn recent_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, title_source, started_at, ended_at, active_seconds,
                    end_reason, pinned, device_id
             FROM sessions
             ORDER BY started_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_session)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn open_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, title_source, started_at, ended_at, active_seconds,
                    end_reason, pinned, device_id
             FROM sessions WHERE ended_at IS NULL
             ORDER BY started_at DESC LIMIT 1",
        )?;
        Ok(stmt.query_row([], row_to_session).optional()?)
    }

    /// When this session last had an observation attached to it.
    pub fn last_observation_at(&self, id: SessionId) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT MAX(at) FROM observations WHERE session_id = ?1",
                params![id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
            .map(dt))
    }

    pub fn session_detail(&self, id: SessionId) -> Result<Option<SessionDetail>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, title_source, started_at, ended_at, active_seconds,
                    end_reason, pinned, device_id
             FROM sessions WHERE id = ?1",
        )?;
        let Some(session) = stmt.query_row(params![id], row_to_session).optional()? else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.kind, a.uri, a.display_name, a.app_id, a.project_root,
                    sa.focus_seconds, sa.first_seen, sa.last_seen,
                    ap.display_name, ap.category, ap.exe_path
             FROM session_artifacts sa
             JOIN artifacts a ON a.id = sa.artifact_id
             LEFT JOIN apps ap ON ap.app_id = a.app_id
             WHERE sa.session_id = ?1
             ORDER BY sa.focus_seconds DESC, sa.last_seen DESC",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok(SessionArtifact {
                artifact_id: r.get(0)?,
                kind: ArtifactKind::parse(&r.get::<_, String>(1)?),
                uri: r.get(2)?,
                display_name: r.get(3)?,
                app_id: r.get(4)?,
                project_root: r.get(5)?,
                focus_seconds: r.get(6)?,
                first_seen: dt(r.get(7)?),
                last_seen: dt(r.get(8)?),
                app_name: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                category: Category::parse(&r.get::<_, Option<String>>(10)?.unwrap_or_default()),
                app_exe: r.get(11)?,
                state: Vec::new(),
            })
        })?;
        let mut artifacts: Vec<SessionArtifact> =
            rows.collect::<std::result::Result<Vec<_>, _>>()?;

        for a in &mut artifacts {
            a.state = self.artifact_state(a.artifact_id)?;
        }
        Ok(Some(SessionDetail { session, artifacts }))
    }

    /// The distinct applications a session touched, most-used first. Feeds the
    /// row of tiles on each card without loading the whole session.
    pub fn session_apps(&self, id: SessionId) -> Result<Vec<(String, Category, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(ap.display_name, a.app_id),
                    COALESCE(ap.category, 'other'),
                    SUM(sa.focus_seconds) AS focus
             FROM session_artifacts sa
             JOIN artifacts a ON a.id = sa.artifact_id
             LEFT JOIN apps ap ON ap.app_id = a.app_id
             WHERE sa.session_id = ?1
             GROUP BY a.app_id
             ORDER BY focus DESC, 1 ASC",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Category::parse(&r.get::<_, String>(1)?),
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// When each observation in a session happened, and what kind of app it
    /// was. Feeds the band across the top of the detail pane.
    pub fn session_shape(&self, id: SessionId) -> Result<Vec<(DateTime<Utc>, Category)>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.at, COALESCE(ap.category, 'other')
             FROM observations o
             LEFT JOIN apps ap ON ap.app_id = o.app_id
             WHERE o.session_id = ?1
             ORDER BY o.at ASC",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((dt(r.get(0)?), Category::parse(&r.get::<_, String>(1)?)))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn artifact_state(&self, id: ArtifactId) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM artifact_state WHERE artifact_id = ?1 ORDER BY key")?;
        let rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn rename_session(&self, id: SessionId, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?2, title_source = 'user' WHERE id = ?1",
            params![id, title],
        )?;
        self.refresh_search(id)?;
        Ok(())
    }

    pub fn set_pinned(&self, id: SessionId, pinned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pinned = ?2 WHERE id = ?1",
            params![id, pinned],
        )?;
        Ok(())
    }

    // ── search ──────────────────────────────────────────────────────────

    fn refresh_search(&self, id: SessionId) -> Result<()> {
        self.conn
            .execute("DELETE FROM search WHERE session_id = ?1", params![id])?;
        let title: String = self
            .conn
            .query_row("SELECT title FROM sessions WHERE id = ?1", params![id], |r| {
                r.get(0)
            })?;
        let body: String = self
            .conn
            .query_row(
                "SELECT COALESCE(GROUP_CONCAT(a.display_name || ' ' || a.uri || ' ' ||
                        COALESCE(a.project_root, ''), ' '), '')
                 FROM session_artifacts sa JOIN artifacts a ON a.id = sa.artifact_id
                 WHERE sa.session_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or_default();
        self.conn.execute(
            "INSERT INTO search (session_id, title, body) VALUES (?1, ?2, ?3)",
            params![id, title, body],
        )?;
        Ok(())
    }

    /// Full-text search across session names, artifact names and paths.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Session>> {
        let cleaned = sanitise_fts(query);
        if cleaned.is_empty() {
            return self.recent_sessions(limit);
        }
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.title_source, s.started_at, s.ended_at,
                    s.active_seconds, s.end_reason, s.pinned, s.device_id
             FROM search f
             JOIN sessions s ON s.id = f.session_id
             WHERE search MATCH ?1
             ORDER BY bm25(search, 4.0, 1.0), s.started_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cleaned, limit as i64], row_to_session)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn sessions_between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, title_source, started_at, ended_at, active_seconds,
                    end_reason, pinned, device_id
             FROM sessions
             WHERE started_at >= ?1 AND started_at < ?2
             ORDER BY started_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![ms(from), ms(to), limit as i64], row_to_session)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ── forgetting ──────────────────────────────────────────────────────

    /// Hard delete, then vacuum. No tombstones, no trash.
    pub fn delete_session(&self, id: SessionId) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM search WHERE session_id = ?1", params![id])?;
        tx.execute("DELETE FROM observations WHERE session_id = ?1", params![id])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()?;
        self.sweep_orphans()?;
        Ok(())
    }

    /// The menu-bar panic button. Removes every trace in a time range.
    pub fn forget_range(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<usize> {
        let (a, b) = (ms(from), ms(to));
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM sightings WHERE at >= ?1 AND at <= ?2", params![a, b])?;
        let n = tx.execute(
            "DELETE FROM observations WHERE at >= ?1 AND at <= ?2",
            params![a, b],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE started_at >= ?1 AND COALESCE(ended_at, started_at) <= ?2",
            params![a, b],
        )?;
        tx.commit()?;
        self.sweep_orphans()?;
        self.conn.execute_batch("VACUUM")?;
        Ok(n)
    }

    /// Rolling retention. Pinned sessions are never pruned.
    pub fn prune(&self, retention_days: i64) -> Result<usize> {
        if retention_days <= 0 {
            return Ok(0);
        }
        let cutoff = Utc::now() - chrono::Duration::days(retention_days);
        let c = ms(cutoff);
        let tx = self.conn.unchecked_transaction()?;
        let n = tx.execute(
            "DELETE FROM sessions WHERE started_at < ?1 AND pinned = 0",
            params![c],
        )?;
        tx.execute("DELETE FROM observations WHERE at < ?1", params![c])?;
        tx.execute("DELETE FROM sightings WHERE at < ?1", params![c])?;
        tx.commit()?;
        self.sweep_orphans()?;
        Ok(n)
    }

    fn sweep_orphans(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM artifacts
              WHERE id NOT IN (SELECT artifact_id FROM session_artifacts)
                AND id NOT IN (SELECT artifact_id FROM sightings);
             DELETE FROM search
              WHERE session_id NOT IN (SELECT id FROM sessions);",
        )?;
        Ok(())
    }

    // ── crash detection ─────────────────────────────────────────────────

    pub fn beat(&self, at: DateTime<Utc>) -> Result<()> {
        self.meta_set("heartbeat", &ms(at).to_string())
    }

    pub fn last_heartbeat(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .meta_get("heartbeat")?
            .and_then(|s| s.parse::<i64>().ok())
            .map(dt))
    }

    pub fn mark_running(&self) -> Result<()> {
        self.meta_set("clean_shutdown", "0")
    }

    pub fn mark_clean_shutdown(&self) -> Result<()> {
        self.meta_set("clean_shutdown", "1")
    }

    /// True when the previous run ended without writing its marker — which is
    /// exactly the session the user is about to come looking for.
    pub fn crashed_last_run(&self) -> Result<bool> {
        Ok(matches!(self.meta_get("clean_shutdown")?.as_deref(), Some("0")))
    }

    pub fn counts(&self) -> Result<(i64, i64, i64)> {
        let s: i64 = self.conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let a: i64 = self.conn.query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))?;
        let o: i64 = self.conn.query_row("SELECT COUNT(*) FROM observations", [], |r| r.get(0))?;
        Ok((s, a, o))
    }
}

/// A row from the observation log, as the sessioniser wants it.
#[derive(Debug, Clone)]
pub struct RawObs {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub app_id: String,
    pub title: String,
    pub artifact_id: Option<ArtifactId>,
    pub artifact_uri: String,
    pub artifact_name: String,
    pub project_root: Option<String>,
    pub category: Category,
}

fn upsert_artifact(
    tx: &rusqlite::Transaction<'_>,
    a: &ArtifactObs,
    app_id: &str,
    at: i64,
) -> rusqlite::Result<ArtifactId> {
    tx.execute(
        "INSERT INTO artifacts (kind, uri, display_name, app_id, project_root, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(uri) DO UPDATE SET
            display_name = excluded.display_name,
            project_root = COALESCE(excluded.project_root, artifacts.project_root),
            last_seen    = excluded.last_seen",
        params![a.kind.as_str(), a.uri, a.display_name, app_id, a.project_root, at],
    )?;
    let id: ArtifactId = tx.query_row(
        "SELECT id FROM artifacts WHERE uri = ?1",
        params![a.uri],
        |r| r.get(0),
    )?;
    for (k, v) in &a.state {
        tx.execute(
            "INSERT INTO artifact_state (artifact_id, key, value, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(artifact_id, key) DO UPDATE SET
                value = excluded.value, updated_at = excluded.updated_at",
            params![id, k, v, at],
        )?;
    }
    Ok(id)
}

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        title: r.get(1)?,
        title_source: TitleSource::parse(&r.get::<_, String>(2)?),
        started_at: dt(r.get(3)?),
        ended_at: r.get::<_, Option<i64>>(4)?.map(dt),
        active_seconds: r.get(5)?,
        end_reason: EndReason::parse(&r.get::<_, String>(6)?),
        pinned: r.get::<_, i64>(7)? != 0,
        device_id: r.get(8)?,
    })
}

/// FTS5 treats a lot of punctuation as syntax. Users type questions, not
/// queries, so everything is reduced to prefix-matched terms.
fn sanitise_fts(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|t| t.len() > 1)
        .map(|t| format!("{}*", t.to_lowercase()))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

const SCHEMA_V1: &str = r#"
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE apps (
  app_id       TEXT PRIMARY KEY,
  display_name TEXT NOT NULL,
  category     TEXT NOT NULL,
  exe_path     TEXT,
  first_seen   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL
);

CREATE TABLE app_policy (
  app_id TEXT PRIMARY KEY,
  policy TEXT NOT NULL
);

CREATE TABLE artifacts (
  id           INTEGER PRIMARY KEY,
  kind         TEXT NOT NULL,
  uri          TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  app_id       TEXT NOT NULL,
  project_root TEXT,
  first_seen   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL
);
CREATE INDEX artifacts_root ON artifacts(project_root);
CREATE INDEX artifacts_seen ON artifacts(last_seen DESC);

CREATE TABLE artifact_state (
  artifact_id INTEGER NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  key         TEXT NOT NULL,
  value       TEXT NOT NULL,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (artifact_id, key)
);

CREATE TABLE sessions (
  id             INTEGER PRIMARY KEY,
  title          TEXT NOT NULL,
  title_source   TEXT NOT NULL,
  started_at     INTEGER NOT NULL,
  ended_at       INTEGER,
  active_seconds INTEGER NOT NULL DEFAULT 0,
  end_reason     TEXT NOT NULL DEFAULT 'open',
  pinned         INTEGER NOT NULL DEFAULT 0,
  device_id      TEXT NOT NULL
);
CREATE INDEX sessions_started ON sessions(started_at DESC);

CREATE TABLE session_artifacts (
  session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  artifact_id   INTEGER NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  focus_seconds INTEGER NOT NULL DEFAULT 0,
  first_seen    INTEGER NOT NULL,
  last_seen     INTEGER NOT NULL,
  PRIMARY KEY (session_id, artifact_id)
);

CREATE TABLE observations (
  id          INTEGER PRIMARY KEY,
  at          INTEGER NOT NULL,
  app_id      TEXT NOT NULL,
  title       TEXT NOT NULL,
  pid         INTEGER NOT NULL,
  artifact_id INTEGER REFERENCES artifacts(id) ON DELETE SET NULL,
  frame       TEXT,
  display_id  TEXT,
  session_id  INTEGER REFERENCES sessions(id) ON DELETE SET NULL
);
CREATE INDEX observations_at ON observations(at);
CREATE INDEX observations_session ON observations(session_id);

CREATE TABLE sightings (
  artifact_id INTEGER NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  at          INTEGER NOT NULL,
  focused     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX sightings_at ON sightings(at);
CREATE INDEX sightings_artifact ON sightings(artifact_id, at);

CREATE TABLE restores (
  id           INTEGER PRIMARY KEY,
  session_id   INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  ran_at       INTEGER NOT NULL,
  plan_json    TEXT NOT NULL,
  outcome_json TEXT
);

CREATE VIRTUAL TABLE search USING fts5(
  session_id UNINDEXED,
  title,
  body,
  tokenize = 'unicode61'
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(at: DateTime<Utc>, app: &str, uri: &str) -> Observation {
        Observation {
            at,
            app_id: app.into(),
            app_name: app.into(),
            exe_path: Some(format!("C:/Program Files/{app}")),
            category: Category::Editor,
            title: "test".into(),
            pid: 1,
            frame: None,
            display_id: None,
            artifacts: vec![ArtifactObs::new(ArtifactKind::Project, uri, "proj")
                .with_root("C:/work/proj")
                .with_state("branch", "main")],
        }
    }

    /// Attach every observation recorded so far, the way the sessioniser does.
    /// Artifact membership follows the observations, not the clock, so this
    /// step is not optional.
    fn attach_everything(s: &Store, id: SessionId, since: DateTime<Utc>) -> Result<()> {
        let ids: Vec<i64> = s
            .unsessioned_observations(since)?
            .into_iter()
            .map(|o| o.id)
            .collect();
        s.attach_observations(id, &ids)?;
        s.rebuild_session_artifacts(id)
    }

    #[test]
    fn records_and_reads_back_a_session() -> Result<()> {
        let s = Store::open_in_memory()?;
        let t0 = Utc::now();
        s.record(&obs(t0, "code.exe", "file:///c:/work/proj"))?;
        s.record(&obs(t0 + chrono::Duration::seconds(20), "code.exe", "file:///c:/work/proj"))?;

        let id = s.create_session("Proj", TitleSource::ProjectRoot, t0)?;
        attach_everything(&s, id, t0 - chrono::Duration::minutes(1))?;
        s.close_session(id, t0 + chrono::Duration::minutes(30), 1800, EndReason::Idle)?;

        let d = s.session_detail(id)?.expect("session exists");
        assert_eq!(d.session.title, "Proj");
        assert_eq!(d.artifacts.len(), 1);
        assert_eq!(d.artifacts[0].state_of("branch"), Some("main"));
        Ok(())
    }

    #[test]
    fn search_finds_a_session_by_its_artifact_path() -> Result<()> {
        let s = Store::open_in_memory()?;
        let t0 = Utc::now();
        s.record(&obs(t0, "code.exe", "file:///c:/work/spotted-android"))?;
        let id = s.create_session("Building the Android App", TitleSource::ProjectRoot, t0)?;
        attach_everything(&s, id, t0 - chrono::Duration::minutes(1))?;

        assert_eq!(s.search("spotted", 10)?.len(), 1);
        assert_eq!(s.search("android app", 10)?.len(), 1);
        assert_eq!(s.search("figma", 10)?.len(), 0);
        Ok(())
    }

    /// Two sessions running in the same stretch of time must not inherit each
    /// other's artifacts. This is the bug the wall-clock window used to have.
    #[test]
    fn interleaved_sessions_do_not_share_artifacts() -> Result<()> {
        let s = Store::open_in_memory()?;
        let t0 = Utc::now();

        s.record(&obs(t0, "code.exe", "file:///c:/work/alpha"))?;
        s.record(&obs(t0 + chrono::Duration::seconds(10), "code.exe", "file:///c:/work/beta"))?;

        let all = s.unsessioned_observations(t0 - chrono::Duration::minutes(1))?;
        assert_eq!(all.len(), 2);

        let a = s.create_session("Alpha", TitleSource::ProjectRoot, t0)?;
        let b = s.create_session("Beta", TitleSource::ProjectRoot, t0)?;
        s.attach_observations(a, &[all[0].id])?;
        s.attach_observations(b, &[all[1].id])?;
        s.rebuild_session_artifacts(a)?;
        s.rebuild_session_artifacts(b)?;

        let da = s.session_detail(a)?.unwrap();
        let db = s.session_detail(b)?.unwrap();
        assert_eq!(da.artifacts.len(), 1, "Alpha kept only its own artifact");
        assert_eq!(db.artifacts.len(), 1, "Beta kept only its own artifact");
        assert!(da.artifacts[0].uri.ends_with("alpha"));
        assert!(db.artifacts[0].uri.ends_with("beta"));
        Ok(())
    }

    #[test]
    fn deleting_a_session_takes_its_artifacts_with_it() -> Result<()> {
        let s = Store::open_in_memory()?;
        let t0 = Utc::now();
        s.record(&obs(t0, "code.exe", "file:///c:/work/proj"))?;
        let id = s.create_session("Proj", TitleSource::ProjectRoot, t0)?;
        s.rebuild_session_artifacts(id)?;
        s.forget_range(t0 - chrono::Duration::minutes(1), t0 + chrono::Duration::minutes(1))?;

        let (sessions, artifacts, observations) = s.counts()?;
        assert_eq!((sessions, artifacts, observations), (0, 0, 0));
        Ok(())
    }

    #[test]
    fn crash_marker_survives_a_reopen() -> Result<()> {
        let s = Store::open_in_memory()?;
        assert!(!s.crashed_last_run()?);
        s.mark_running()?;
        assert!(s.crashed_last_run()?);
        s.mark_clean_shutdown()?;
        assert!(!s.crashed_last_run()?);
        Ok(())
    }
}

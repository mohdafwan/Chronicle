//! Reading a Chromium browser's open tabs without an extension.
//!
//! Chrome continuously writes its own crash-recovery state to `Sessions/Session_*`
//! in the SNSS container format. Chronicle reads that file — the same data the
//! browser itself uses to answer "restore my tabs" — and never touches page
//! content, cookies, or history.
//!
//! Two properties matter and both come free: **incognito windows are never
//! written to session files at all**, so they cannot leak here even by
//! accident; and the file is Chrome's own, so reading it costs the browser
//! nothing.
//!
//! The file lags reality by a few seconds because Chrome flushes
//! asynchronously. That is fine — Chronicle samples over minutes, not frames.

use chronicle_core::model::{ArtifactKind, ArtifactObs};
use chronicle_core::redact;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::enrich::Enricher;
use crate::win::Sample;

/// One tab, as the browser recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub tab_id: i32,
    pub window_id: i32,
    /// Position in the tab strip. Restore replays this order.
    pub index: i32,
    pub url: String,
    pub title: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserWindow {
    pub window_id: i32,
    pub tabs: Vec<Tab>,
}

// ── the SNSS container ──────────────────────────────────────────────────

const SNSS_MAGIC: &[u8; 4] = b"SNSS";

// Command ids from Chromium's session_service_commands.cc. Only the handful
// that carry tab identity are read; everything else is skipped by length.
const CMD_SET_TAB_WINDOW: u8 = 0;
const CMD_SET_TAB_INDEX_IN_WINDOW: u8 = 2;
const CMD_UPDATE_TAB_NAVIGATION: u8 = 6;
const CMD_SET_SELECTED_NAVIGATION_INDEX: u8 = 7;
const CMD_SET_PINNED_STATE: u8 = 12;
const CMD_TAB_CLOSED: u8 = 16;
const CMD_WINDOW_CLOSED: u8 = 17;

/// Chromium's `base::Pickle` wire format: little-endian, every field padded
/// out to a four-byte boundary.
struct Pickle<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Pickle<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Chromium builds most session commands from a raw C struct, but a few —
    /// tab navigation among them — from a `base::Pickle`, and a pickle
    /// serialises its own four-byte length header ahead of the fields.
    ///
    /// The header is verified rather than assumed, so a future Chrome that
    /// drops it still parses instead of returning nonsense.
    fn new_pickle(buf: &'a [u8]) -> Self {
        let mut p = Self::new(buf);
        match p.read_i32() {
            Some(len) if len as usize == buf.len().saturating_sub(4) => p,
            _ => Self::new(buf),
        }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn read_i32(&mut self) -> Option<i32> {
        let b = self.take(4)?;
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_bool(&mut self) -> Option<bool> {
        Some(self.read_i32()? != 0)
    }

    /// Length-prefixed bytes, position advanced to the next four-byte boundary.
    fn read_string(&mut self) -> Option<String> {
        let len = self.read_i32()?;
        if len < 0 {
            return None;
        }
        let len = len as usize;
        let bytes = self.buf.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += align4(len);
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Same, but the length counts UTF-16 code units rather than bytes.
    fn read_string16(&mut self) -> Option<String> {
        let len = self.read_i32()?;
        if len < 0 {
            return None;
        }
        let bytes_len = (len as usize).checked_mul(2)?;
        let bytes = self.buf.get(self.pos..self.pos.checked_add(bytes_len)?)?;
        self.pos += align4(bytes_len);
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Parse an SNSS session file into the windows and tabs it describes.
///
/// The format is a replay log, not a snapshot: later commands overwrite
/// earlier ones, and close commands remove entries. Applying them in order is
/// what produces the browser's current state.
pub fn parse_snss(data: &[u8]) -> Vec<BrowserWindow> {
    if data.len() < 8 || &data[0..4] != SNSS_MAGIC {
        return Vec::new();
    }

    let mut tabs: HashMap<i32, Tab> = HashMap::new();
    let mut closed_windows: Vec<i32> = Vec::new();
    let mut pos = 8; // magic + version

    while pos + 3 <= data.len() {
        let size = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        if size == 0 {
            break;
        }
        let id = data[pos + 2];
        let payload_start = pos + 3;
        let payload_end = match payload_start.checked_add(size - 1) {
            Some(e) if e <= data.len() => e,
            // A truncated trailing command is normal: Chrome was mid-write.
            _ => break,
        };
        let payload = &data[payload_start..payload_end];
        pos = payload_end;

        let mut p = Pickle::new(payload);
        match id {
            CMD_SET_TAB_WINDOW => {
                // window id, then tab id.
                if let (Some(w), Some(t)) = (p.read_i32(), p.read_i32()) {
                    tabs.entry(t).or_insert_with(|| blank_tab(t)).window_id = w;
                }
            }
            CMD_SET_TAB_INDEX_IN_WINDOW => {
                if let (Some(t), Some(i)) = (p.read_i32(), p.read_i32()) {
                    tabs.entry(t).or_insert_with(|| blank_tab(t)).index = i;
                }
            }
            CMD_UPDATE_TAB_NAVIGATION => {
                // tab id, navigation index, url, title — the rest of the
                // serialised navigation entry is deliberately not read.
                let mut p = Pickle::new_pickle(payload);
                if let (Some(t), Some(_nav), Some(url), Some(title)) =
                    (p.read_i32(), p.read_i32(), p.read_string(), p.read_string16())
                {
                    let tab = tabs.entry(t).or_insert_with(|| blank_tab(t));
                    tab.url = url;
                    tab.title = title;
                }
            }
            CMD_SET_SELECTED_NAVIGATION_INDEX => {
                // Present for completeness; navigation history is not kept.
            }
            CMD_SET_PINNED_STATE => {
                if let (Some(t), Some(pinned)) = (p.read_i32(), p.read_bool()) {
                    tabs.entry(t).or_insert_with(|| blank_tab(t)).pinned = pinned;
                }
            }
            CMD_TAB_CLOSED => {
                if let Some(t) = p.read_i32() {
                    tabs.remove(&t);
                }
            }
            CMD_WINDOW_CLOSED => {
                if let Some(w) = p.read_i32() {
                    closed_windows.push(w);
                }
            }
            _ => {}
        }
    }

    let mut by_window: HashMap<i32, Vec<Tab>> = HashMap::new();
    for tab in tabs.into_values() {
        if tab.url.is_empty() || closed_windows.contains(&tab.window_id) {
            continue;
        }
        by_window.entry(tab.window_id).or_default().push(tab);
    }

    let mut windows: Vec<BrowserWindow> = by_window
        .into_iter()
        .map(|(window_id, mut tabs)| {
            tabs.sort_by_key(|t| (t.index, t.tab_id));
            BrowserWindow { window_id, tabs }
        })
        .filter(|w| !w.tabs.is_empty())
        .collect();
    windows.sort_by_key(|w| w.window_id);
    windows
}

fn blank_tab(tab_id: i32) -> Tab {
    Tab {
        tab_id,
        window_id: -1,
        index: 0,
        url: String::new(),
        title: String::new(),
        pinned: false,
    }
}

// ── finding the files ───────────────────────────────────────────────────

/// Where each Chromium browser keeps its profiles.
fn user_data_dir(app_id: &str) -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    let rel = match app_id {
        "chrome.exe" => r"Google\Chrome\User Data",
        "msedge.exe" => r"Microsoft\Edge\User Data",
        "brave.exe" => r"BraveSoftware\Brave-Browser\User Data",
        "vivaldi.exe" => r"Vivaldi\User Data",
        _ => return None,
    };
    Some(PathBuf::from(local).join(rel))
}

/// Every `Session_*` file of every profile, newest first, grouped by profile.
fn session_files(user_data: &Path) -> Vec<Vec<(PathBuf, SystemTime)>> {
    let Ok(profiles) = std::fs::read_dir(user_data) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    for profile in profiles.flatten() {
        let sessions = profile.path().join("Sessions");
        let Ok(entries) = std::fs::read_dir(&sessions) else {
            continue;
        };
        let mut files: Vec<(PathBuf, SystemTime)> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("Session_"))
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
            .collect();
        if !files.is_empty() {
            files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
            out.push(files);
        }
    }
    out
}

/// What Chronicle could actually read, and how current it is.
#[derive(Debug, Clone, Default)]
pub struct BrowserState {
    pub windows: Vec<BrowserWindow>,
    /// Last-write time of the file the data came from.
    pub as_of: Option<SystemTime>,
    /// True when a newer session file exists but Windows would not open it.
    pub live_file_locked: bool,
}

/// Read the open windows of one browser.
///
/// A running Chromium browser holds its *current* session file open without
/// sharing reads, so the newest file is usually unreadable while the browser
/// is the very thing you want to observe. Chronicle falls back to the newest
/// file it can actually open, and reports that the result is behind — a stale
/// tab set that says so beats a confident empty one.
pub fn read_state(app_id: &str) -> BrowserState {
    let Some(dir) = user_data_dir(app_id) else {
        return BrowserState::default();
    };
    let mut state = BrowserState::default();

    for profile in session_files(&dir) {
        let newest = profile.first().map(|(_, m)| *m);
        let mut used: Option<SystemTime> = None;

        for (path, modified) in &profile {
            let Ok(data) = std::fs::read(path) else {
                continue; // locked by the running browser, or vanished
            };
            let windows = parse_snss(&data);
            if windows.is_empty() {
                continue;
            }
            state.windows.extend(windows);
            used = Some(*modified);
            break;
        }

        if used.is_some() && used != newest {
            state.live_file_locked = true;
        }
        state.as_of = match (state.as_of, used) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
    state
}

/// Convenience wrapper for callers that only want the tabs.
pub fn open_windows(app_id: &str) -> Vec<BrowserWindow> {
    read_state(app_id).windows
}

// ── the enricher ────────────────────────────────────────────────────────

/// Parsing a session file on every two-second sample would be wasteful, so
/// results are held briefly and re-read only when the file actually changes.
struct Cache {
    read_at: Instant,
    mtime: Option<SystemTime>,
    windows: Vec<BrowserWindow>,
}

pub struct Chromium {
    cache: Mutex<HashMap<String, Cache>>,
}

impl Default for Chromium {
    fn default() -> Self {
        Self::new()
    }
}

impl Chromium {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    const TTL: Duration = Duration::from_secs(15);

    fn windows_for(&self, app_id: &str) -> Vec<BrowserWindow> {
        let mtime = user_data_dir(app_id).and_then(|d| {
            session_files(&d)
                .iter()
                .filter_map(|profile| profile.first().map(|(_, m)| *m))
                .max()
        });

        let mut cache = match self.cache.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(hit) = cache.get(app_id) {
            if hit.read_at.elapsed() < Self::TTL && hit.mtime == mtime {
                return hit.windows.clone();
            }
        }

        let windows = open_windows(app_id);
        cache.insert(
            app_id.to_string(),
            Cache {
                read_at: Instant::now(),
                mtime,
                windows: windows.clone(),
            },
        );
        windows
    }
}

impl Enricher for Chromium {
    fn name(&self) -> &'static str {
        "chromium"
    }

    fn matches(&self, s: &Sample) -> bool {
        user_data_dir(&s.app_id).is_some()
    }

    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs> {
        let windows = self.windows_for(&s.app_id);
        if windows.is_empty() {
            return Vec::new();
        }

        // The OS window title is the focused tab's title. Match it back to a
        // recorded window so the session gets *this* window's tabs, not every
        // tab in the browser.
        let page_title = s
            .title
            .rsplit_once(" - ")
            .map(|(head, _)| head)
            .unwrap_or(&s.title)
            .trim();

        let focused = windows
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.title == page_title));

        let (tabs, focused_tab) = match focused {
            Some(w) => (
                w.tabs.clone(),
                w.tabs.iter().position(|t| t.title == page_title),
            ),
            // No match — a new tab, a title Chrome has not flushed yet, or a
            // redacted title. Fall back to every open tab.
            None => (
                windows.iter().flat_map(|w| w.tabs.clone()).collect::<Vec<_>>(),
                None,
            ),
        };

        let mut out: Vec<ArtifactObs> = Vec::new();

        // The focused tab goes first: the store treats index 0 as focused, and
        // that is what drives focus time.
        let ordered: Vec<&Tab> = match focused_tab {
            Some(i) => std::iter::once(&tabs[i])
                .chain(tabs.iter().enumerate().filter(|(n, _)| *n != i).map(|(_, t)| t))
                .collect(),
            None => tabs.iter().collect(),
        };

        for tab in ordered.into_iter().take(40) {
            // Redaction is not optional here: a URL that cannot be made safe
            // is dropped rather than stored.
            let Some(url) = redact::redact_url(&tab.url) else {
                continue;
            };
            let title = if redact::looks_secret(&tab.title) {
                String::new()
            } else {
                tab.title.clone()
            };
            let mut a = ArtifactObs::new(
                ArtifactKind::Url,
                url.clone(),
                redact::url_display(&url, Some(&title)),
            );
            if tab.pinned {
                a = a.with_state("pinned", "true");
            }
            a = a.with_state("index", tab.index.to_string());
            out.push(a);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an SNSS buffer the way Chrome would.
    struct Writer {
        buf: Vec<u8>,
    }

    impl Writer {
        fn new() -> Self {
            let mut buf = Vec::new();
            buf.extend_from_slice(SNSS_MAGIC);
            buf.extend_from_slice(&1i32.to_le_bytes());
            Self { buf }
        }

        fn command(&mut self, id: u8, payload: &[u8]) {
            let size = (payload.len() + 1) as u16;
            self.buf.extend_from_slice(&size.to_le_bytes());
            self.buf.push(id);
            self.buf.extend_from_slice(payload);
        }
    }

    fn int(v: i32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    fn string(s: &str) -> Vec<u8> {
        let mut out = int(s.len() as i32);
        out.extend_from_slice(s.as_bytes());
        out.resize(4 + align4(s.len()), 0);
        out
    }

    fn string16(s: &str) -> Vec<u8> {
        let units: Vec<u16> = s.encode_utf16().collect();
        let mut out = int(units.len() as i32);
        for u in &units {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.resize(4 + align4(units.len() * 2), 0);
        out
    }

    /// Chrome builds this command from a `base::Pickle`, so the fields are
    /// preceded by the pickle's own four-byte length header. Emitting it here
    /// is the difference between a test that passes and a parser that works.
    fn navigation(tab: i32, url: &str, title: &str) -> Vec<u8> {
        let mut fields = int(tab);
        fields.extend(int(0)); // navigation index
        fields.extend(string(url));
        fields.extend(string16(title));

        let mut p = int(fields.len() as i32);
        p.extend(fields);
        p
    }

    fn sample_file() -> Vec<u8> {
        let mut w = Writer::new();
        // Window 1: three tabs, the second pinned.
        for (tab, idx) in [(10, 0), (11, 1), (12, 2)] {
            let mut p = int(1);
            p.extend(int(tab));
            w.command(CMD_SET_TAB_WINDOW, &p);

            let mut p = int(tab);
            p.extend(int(idx));
            w.command(CMD_SET_TAB_INDEX_IN_WINDOW, &p);
        }
        w.command(
            CMD_UPDATE_TAB_NAVIGATION,
            &navigation(10, "https://developer.android.com/identity", "Credential Manager"),
        );
        w.command(CMD_UPDATE_TAB_NAVIGATION, &navigation(11, "https://firebase.google.com/docs/auth", "Phone auth"));
        w.command(CMD_UPDATE_TAB_NAVIGATION, &navigation(12, "https://stackoverflow.com/q/78123", "OTP autofill"));

        let mut p = int(11);
        p.extend(int(1));
        w.command(CMD_SET_PINNED_STATE, &p);
        w.buf
    }

    #[test]
    fn reads_windows_and_tabs_in_strip_order() {
        let windows = parse_snss(&sample_file());
        assert_eq!(windows.len(), 1);
        let tabs = &windows[0].tabs;
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].url, "https://developer.android.com/identity");
        assert_eq!(tabs[1].title, "Phone auth");
        assert_eq!(tabs[2].index, 2);
        assert!(tabs[1].pinned, "pinned state is applied");
    }

    #[test]
    fn a_closed_tab_does_not_come_back() {
        let mut data = sample_file();
        let mut w = Writer { buf: Vec::new() };
        w.command(CMD_TAB_CLOSED, &int(11));
        data.extend_from_slice(&w.buf);

        let windows = parse_snss(&data);
        let tabs = &windows[0].tabs;
        assert_eq!(tabs.len(), 2);
        assert!(tabs.iter().all(|t| t.tab_id != 11));
    }

    #[test]
    fn a_closed_window_takes_its_tabs_with_it() {
        let mut data = sample_file();
        let mut w = Writer { buf: Vec::new() };
        w.command(CMD_WINDOW_CLOSED, &int(1));
        data.extend_from_slice(&w.buf);

        assert!(parse_snss(&data).is_empty());
    }

    #[test]
    fn a_later_navigation_replaces_an_earlier_one() {
        let mut data = sample_file();
        let mut w = Writer { buf: Vec::new() };
        w.command(
            CMD_UPDATE_TAB_NAVIGATION,
            &navigation(10, "https://example.com/moved-on", "Moved on"),
        );
        data.extend_from_slice(&w.buf);

        let windows = parse_snss(&data);
        let tab = windows[0].tabs.iter().find(|t| t.tab_id == 10).unwrap();
        assert_eq!(tab.url, "https://example.com/moved-on");
        assert_eq!(tab.title, "Moved on");
    }

    #[test]
    fn a_file_truncated_mid_write_yields_what_it_can() {
        let full = sample_file();
        let truncated = &full[..full.len() - 7];
        let windows = parse_snss(truncated);
        assert!(!windows.is_empty(), "a partial flush must not lose everything");
    }

    #[test]
    fn rubbish_is_not_mistaken_for_a_session_file() {
        assert!(parse_snss(b"not an snss file at all").is_empty());
        assert!(parse_snss(&[]).is_empty());
    }

    #[test]
    fn unicode_titles_survive_the_utf16_round_trip() {
        let mut w = Writer::new();
        let mut p = int(1);
        p.extend(int(10));
        w.command(CMD_SET_TAB_WINDOW, &p);
        w.command(
            CMD_UPDATE_TAB_NAVIGATION,
            &navigation(10, "https://example.com/", "स्पॉटेड — लॉगिन"),
        );
        let windows = parse_snss(&w.buf);
        assert_eq!(windows[0].tabs[0].title, "स्पॉटेड — लॉगिन");
    }
}

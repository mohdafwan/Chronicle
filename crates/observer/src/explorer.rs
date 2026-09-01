//! Resolving which folder an Explorer window is actually showing.
//!
//! An Explorer window title is only a folder *name* — "Downloads", or
//! "Program Manager" for the desktop itself. A name cannot be reopened, which
//! is why folders used to come back from a restore marked "needs you".
//!
//! The real path comes from `IShellWindows`, the same COM object Explorer uses
//! to talk to itself. Each shell window exposes a `LocationURL`, and matching
//! it back to a window handle is what lets Chronicle say "this window was
//! showing that directory" rather than "File Explorer was open".

use chronicle_core::model::{ArtifactKind, ArtifactObs};
use std::cell::Cell;
use std::path::PathBuf;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
use windows::core::Interface;

use crate::enrich::{Enricher, file_artifact, project_artifact, project_root};
use crate::win::Sample;

thread_local! {
    /// COM is per-thread and initialising it twice on one thread is wasteful
    /// but harmless; this keeps it to once.
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            // S_FALSE means someone already initialised this thread, which is
            // just as good as doing it here.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok().ok();
            ready.set(true);
        }
    });
}

/// Every open Explorer window, as (window handle, folder path).
///
/// Windows that are not showing a filesystem folder — This PC, Control Panel,
/// a search results view — resolve to no path and are left out.
pub fn open_folders() -> Vec<(isize, PathBuf)> {
    ensure_com();
    let mut out = Vec::new();

    unsafe {
        let Ok(shell) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL)
        else {
            return out;
        };
        let Ok(count) = shell.Count() else {
            return out;
        };

        for i in 0..count {
            let Ok(item) = shell.Item(&VARIANT::from(i)) else {
                continue;
            };
            let Ok(browser) = item.cast::<IWebBrowser2>() else {
                continue;
            };
            let Ok(hwnd) = browser.HWND() else {
                continue;
            };
            let Ok(url) = browser.LocationURL() else {
                continue;
            };
            let url = url.to_string();
            if let Some(path) = file_url_to_path(&url) {
                out.push((hwnd.0, path));
            }
        }
    }
    out
}

/// `file:///C:/Users/af3an/Downloads` back to a real path, percent-decoded.
///
/// Explorer also reports `::{GUID}` shell namespace locations for This PC and
/// the Recycle Bin. Those are not directories and are deliberately dropped.
fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:///")?;
    let decoded = percent_decode(rest);
    let path = PathBuf::from(decoded.replace('/', "\\"));
    path.is_dir().then_some(path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Turns an Explorer window into the folder it is showing.
pub struct Explorer;

impl Enricher for Explorer {
    fn name(&self) -> &'static str {
        "explorer"
    }

    fn matches(&self, s: &Sample) -> bool {
        s.app_id == "explorer.exe"
    }

    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs> {
        let folders = open_folders();

        // Prefer the folder belonging to this exact window. Explorer reuses one
        // process for every window, so the handle is the only thing that tells
        // two of them apart.
        let Some((_, path)) = folders.iter().find(|(hwnd, _)| *hwnd == s.hwnd) else {
            return Vec::new();
        };

        // A folder that is itself a project root is worth recording as one:
        // it is the same directory an editor would open.
        match project_root(path) {
            Some(root) if root == *path => vec![project_artifact(path)],
            _ => vec![file_artifact(path, ArtifactKind::Directory)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_file_url_with_spaces() {
        assert_eq!(
            percent_decode("C:/Users/af3an/My%20Documents"),
            "C:/Users/af3an/My Documents"
        );
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        assert_eq!(percent_decode("C:/work/proj"), "C:/work/proj");
    }

    #[test]
    fn shell_namespace_locations_are_not_folders() {
        // This PC and the Recycle Bin are not directories and must not be
        // recorded as though a restore could open them.
        assert!(file_url_to_path("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}").is_none());
        assert!(file_url_to_path("file:///C:/definitely/not/here").is_none());
    }
}

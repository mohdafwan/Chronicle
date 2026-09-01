//! The Windows observation surface.
//!
//! Everything here is read-only and unprivileged: the foreground window, its
//! title, its owning process, its geometry, and how long the machine has been
//! idle. No hooks, no injection, no kernel driver, and nothing that can see a
//! keystroke — the APIs to do that are simply not imported.

use chronicle_core::model::Frame;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, MAX_PATH, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::{BOOL, PWSTR};

/// One raw look at the foreground window, before policy or redaction.
#[derive(Debug, Clone)]
pub struct Sample {
    pub hwnd: isize,
    pub title: String,
    pub pid: u32,
    /// Full path to the executable, when it could be read.
    pub exe_path: Option<String>,
    /// Lowercased executable file name. Chronicle's stable app identity.
    pub app_id: String,
    /// Win32 window class. Not localised, unlike the title, which is why it is
    /// the only dependable way to tell a folder window from the desktop.
    pub class: String,
    pub frame: Option<Frame>,
    pub display_id: Option<String>,
}

/// Sample the foreground window. `None` when nothing is focused, which happens
/// during desktop switches and while the lock screen is up.
pub fn foreground() -> Option<Sample> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        sample_of(hwnd)
    }
}

/// Every visible top-level window on the desktop, not just the focused one.
///
/// The recorder never uses this — it only ever looks at what is in front of
/// you. It exists so `chronicled scan` can answer "what would Chronicle make of
/// each app I have open?" without asking you to click through them one by one.
pub fn all_windows() -> Vec<Sample> {
    let mut found: Vec<Sample> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect),
            LPARAM(&mut found as *mut Vec<Sample> as isize),
        );
    }
    found
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let out = &mut *(lparam.0 as *mut Vec<Sample>);
        if IsWindowVisible(hwnd).as_bool() {
            if let Some(s) = sample_of(hwnd) {
                // Untitled windows are tool windows and tray hosts, not work.
                if !s.title.trim().is_empty() {
                    out.push(s);
                }
            }
        }
        BOOL(1)
    }
}

unsafe fn sample_of(hwnd: HWND) -> Option<Sample> {
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }

        let exe_path = process_image_path(pid);
        let app_id = exe_path
            .as_deref()
            .and_then(|p| p.rsplit(['\\', '/']).next())
            .unwrap_or("unknown.exe")
            .to_ascii_lowercase();

        Some(Sample {
            hwnd: hwnd.0 as isize,
            title: window_text(hwnd),
            pid,
            exe_path,
            app_id,
            class: window_class(hwnd),
            frame: window_frame(hwnd),
            display_id: monitor_of(hwnd),
        })
    }
}

/// Seconds since the last keyboard or mouse input anywhere on the desktop.
/// This is a duration, not an input event — Windows exposes only the tick
/// count of the last input, never what it was.
pub fn idle_seconds() -> u64 {
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut lii).as_bool() {
            let now = GetTickCount64();
            let last = lii.dwTime as u64;
            // dwTime is a 32-bit tick count and wraps every ~49 days.
            let elapsed = now.wrapping_sub(last) & 0xFFFF_FFFF;
            elapsed / 1000
        } else {
            0
        }
    }
}

/// True when the workstation is locked. Detected from the foreground process
/// rather than a session hook, because those processes are on the permanent
/// deny list anyway and will never be recorded.
pub fn is_locked() -> bool {
    matches!(
        foreground().map(|s| s.app_id).as_deref(),
        Some("lockapp.exe") | Some("logonui.exe")
    )
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// The window class, e.g. `CabinetWClass` for a folder window.
fn window_class(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn process_image_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; MAX_PATH as usize];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn window_frame(hwnd: HWND) -> Option<Frame> {
    unsafe {
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        Some(Frame {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        })
    }
}

/// The device name of the monitor the window mostly sits on, so a restore can
/// tell "the laptop screen" from "the external monitor that is not here today".
fn monitor_of(hwnd: HWND) -> Option<String> {
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if hmon.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let ok = GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut MONITORINFO).as_bool();
        if !ok {
            return None;
        }
        let name = String::from_utf16_lossy(&mi.szDevice);
        Some(name.trim_end_matches('\0').to_string())
    }
}

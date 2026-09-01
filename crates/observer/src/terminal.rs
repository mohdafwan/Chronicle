//! Resolving which directory a terminal window is actually sitting in.
//!
//! A terminal title says "Windows PowerShell" and nothing else, and
//! `windowsterminal.exe` is not itself in any directory — the shell is a
//! separate process. That is why terminals restored as "the app was open"
//! rather than "a prompt in this folder", and showed as **Needs you** in every
//! session the user had a terminal in, which is most of them.
//!
//! The working directory lives in the shell process's own memory, in the
//! `RTL_USER_PROCESS_PARAMETERS` block the loader hangs off the PEB. Reading it
//! needs `PROCESS_VM_READ`, which one gets for one's own processes without
//! elevation. Nothing here writes to another process, injects anything, or
//! reads a command line — only the current directory field.

use chronicle_core::model::{ArtifactKind, ArtifactObs};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::System::Threading::{
    GetProcessTimes, IsWow64Process, OpenProcess, PROCESS_BASIC_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

use crate::enrich::{Enricher, base_name, normalised_path, project_root};
use crate::win::Sample;

/// Windows that Chronicle will look for a shell underneath.
const TERMINAL_APPS: &[&str] = &[
    "windowsterminal.exe",
    "wt.exe",
    "conhost.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "alacritty.exe",
    "wezterm-gui.exe",
    "hyper.exe",
    "conemu64.exe",
    "mintty.exe",
    "tabby.exe",
];

/// Processes whose current directory is worth recording.
///
/// Deliberately not "any child process": a `cargo build` running under the
/// prompt has its own directory, and recording it would file a session under
/// whatever the compiler happened to be doing.
const SHELLS: &[&str] = &[
    "powershell.exe",
    "pwsh.exe",
    "cmd.exe",
    "bash.exe",
    "sh.exe",
    "zsh.exe",
    "fish.exe",
    "nu.exe",
    "wsl.exe",
];

// The x64 layout of the two structures involved. Both have been stable since
// Vista, and neither is in the public SDK headers — `winternl.h` stops at
// `CommandLine` and never declares `CurrentDirectory` — so the offsets are
// spelled out rather than borrowed from a struct definition.
//
// A 32-bit process has a different layout entirely; those are declined below
// rather than read with the wrong offsets, because a plausible-looking wrong
// path is worse than no path.
const PEB_PROCESS_PARAMETERS: usize = 0x20;
const PARAMS_CURDIR_LENGTH: usize = 0x38;
const PARAMS_CURDIR_BUFFER: usize = 0x40;

struct Proc {
    pid: u32,
    parent: u32,
    name: String,
}

/// Every running process, as (pid, parent pid, lowercased exe name).
fn snapshot() -> Vec<Proc> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                out.push(Proc {
                    pid: e.th32ProcessID,
                    parent: e.th32ParentProcessID,
                    name: String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase(),
                });
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        CloseHandle(snap).ok();
    }
    out
}

/// The shell processes belonging to one terminal window's process.
///
/// Windows Terminal hosts its tabs as descendants, one `OpenConsole.exe` per
/// tab with the shell beneath it. A classic console window is the other way
/// round: the window belongs to `conhost.exe`, which is a *child* of the shell.
/// Both shapes are checked, so a Command Prompt resolves as readily as a tab.
fn shells_under(pid: u32, procs: &[Proc]) -> Vec<u32> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let is_shell = |p: &Proc| SHELLS.contains(&p.name.as_str());

    if let Some(p) = by_pid.get(&pid) {
        if is_shell(p) {
            return vec![pid];
        }
    }

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        children.entry(p.parent).or_default().push(p.pid);
    }

    // Breadth-first, bounded: a terminal's shells are one or two levels down,
    // and an unbounded walk over a process tree with a cycle would not end.
    let mut found = Vec::new();
    let mut queue = vec![(pid, 0u32)];
    let mut seen = std::collections::HashSet::new();
    while let Some((current, depth)) = queue.pop() {
        if depth > 4 || !seen.insert(current) {
            continue;
        }
        for &child in children.get(&current).into_iter().flatten() {
            if let Some(p) = by_pid.get(&child) {
                if is_shell(p) {
                    found.push(child);
                }
            }
            queue.push((child, depth + 1));
        }
    }
    if !found.is_empty() {
        return found;
    }

    // The conhost case: the window's process is the console host, and the shell
    // that owns it is the parent.
    by_pid
        .get(&pid)
        .and_then(|p| by_pid.get(&p.parent))
        .filter(|p| is_shell(p))
        .map(|p| vec![p.pid])
        .unwrap_or_default()
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) }.ok();
    }
}

unsafe fn read_at<T: Copy + Default>(h: HANDLE, addr: usize) -> Option<T> {
    let mut value = T::default();
    unsafe {
        ReadProcessMemory(
            h,
            addr as *const _,
            &mut value as *mut T as *mut _,
            size_of::<T>(),
            None,
        )
    }
    .ok()?;
    Some(value)
}

/// The current directory of one process, read from its own memory.
///
/// `None` for anything that cannot be read: a process that exited between the
/// snapshot and here, one running at a higher integrity level, or a 32-bit
/// process whose structures do not match the offsets above.
fn current_directory(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()?;
        let h = Handle(handle);

        let mut wow64 = windows::core::BOOL(0);
        if IsWow64Process(h.0, &mut wow64).is_ok() && wow64.as_bool() {
            return None;
        }

        // Not `Default`: the struct holds raw pointers.
        let mut pbi: PROCESS_BASIC_INFORMATION = std::mem::zeroed();
        let mut written = 0u32;
        let status = NtQueryInformationProcess(
            h.0,
            ProcessBasicInformation,
            &mut pbi as *mut _ as *mut _,
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut written,
        );
        if status.is_err() {
            return None;
        }

        let peb = pbi.PebBaseAddress as usize;
        if peb == 0 {
            return None;
        }
        let params: usize = read_at(h.0, peb + PEB_PROCESS_PARAMETERS)?;
        if params == 0 {
            return None;
        }

        let len: u16 = read_at(h.0, params + PARAMS_CURDIR_LENGTH)?;
        let buffer: usize = read_at(h.0, params + PARAMS_CURDIR_BUFFER)?;
        if len == 0 || buffer == 0 || len as usize > 2 * 32_768 {
            return None;
        }

        let mut wide = vec![0u16; len as usize / 2];
        ReadProcessMemory(
            h.0,
            buffer as *const _,
            wide.as_mut_ptr() as *mut _,
            len as usize,
            None,
        )
        .ok()?;

        let s = String::from_utf16_lossy(&wide);
        let path = PathBuf::from(s.trim_end_matches('\\'));
        path.is_dir().then_some(path)
    }
}

/// When the process started, used only to keep tabs in the order they opened.
fn started_at(pid: u32) -> u64 {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return 0;
        };
        let h = Handle(handle);
        let mut creation = Default::default();
        let (mut exit, mut kernel, mut user) = (Default::default(), Default::default(), Default::default());
        if GetProcessTimes(h.0, &mut creation, &mut exit, &mut kernel, &mut user).is_err() {
            return 0;
        }
        ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64
    }
}

/// A directory that means "the shell started here", not "this is the work".
///
/// A prompt sitting in `System32` or straight in the profile root is the
/// default a terminal opens with. Recording it would put a folder nobody chose
/// into the session, and offer to restore a terminal there.
fn is_uninteresting(dir: &Path) -> bool {
    let lower = dir.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
    if lower.contains("/windows/") || lower.ends_with("/windows") {
        return true;
    }
    let home = std::env::var("USERPROFILE")
        .ok()
        .map(|h| h.to_ascii_lowercase().replace('\\', "/"));
    match home {
        Some(h) => lower == h.trim_end_matches('/'),
        None => false,
    }
}

/// Every distinct directory the terminal at `pid` has a prompt sitting in,
/// oldest tab first.
pub fn working_directories(pid: u32) -> Vec<PathBuf> {
    let procs = snapshot();
    let mut shells = shells_under(pid, &procs);
    shells.sort_by_key(|&p| started_at(p));

    let mut out: Vec<PathBuf> = Vec::new();
    for shell in shells {
        if let Some(dir) = current_directory(shell) {
            if !is_uninteresting(&dir) && !out.contains(&dir) {
                out.push(dir);
            }
        }
    }
    out
}

/// A terminal's directory gets its own URI scheme.
///
/// `artifacts.uri` is unique across the table and the owning application is
/// fixed the first time a URI is seen. Sharing `file:///` with the Explorer
/// enricher would mean whichever saw the folder first decided that restoring it
/// opens a file manager — or a prompt — for everyone afterwards.
fn terminal_uri(dir: &Path) -> String {
    format!("terminal://{}", normalised_path(dir))
}

fn terminal_artifact(dir: &Path) -> ArtifactObs {
    let mut a = ArtifactObs::new(ArtifactKind::Terminal, terminal_uri(dir), base_name(dir));
    if let Some(root) = project_root(dir) {
        a = a.with_root(root.to_string_lossy().replace('\\', "/"));
    }
    a
}

/// Turns a terminal window into the directories its prompts are sitting in.
pub struct Terminal;

impl Enricher for Terminal {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn matches(&self, s: &Sample) -> bool {
        TERMINAL_APPS.contains(&s.app_id.as_str())
    }

    fn enrich(&self, s: &Sample) -> Vec<ArtifactObs> {
        working_directories(s.pid).iter().map(|d| terminal_artifact(d)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent: u32, name: &str) -> Proc {
        Proc {
            pid,
            parent,
            name: name.into(),
        }
    }

    #[test]
    fn finds_the_shells_beneath_a_terminal_window() {
        // What Windows Terminal actually looks like: a tab is an OpenConsole
        // with the shell under it.
        let procs = vec![
            proc(100, 1, "windowsterminal.exe"),
            proc(200, 100, "openconsole.exe"),
            proc(300, 200, "powershell.exe"),
            proc(400, 100, "openconsole.exe"),
            proc(500, 400, "pwsh.exe"),
        ];
        let mut found = shells_under(100, &procs);
        found.sort();
        assert_eq!(found, vec![300, 500]);
    }

    #[test]
    fn a_classic_console_resolves_through_its_parent() {
        // conhost owns the window but is a child of the shell, not its parent.
        let procs = vec![proc(10, 1, "cmd.exe"), proc(20, 10, "conhost.exe")];
        assert_eq!(shells_under(20, &procs), vec![10]);
    }

    #[test]
    fn a_shell_window_is_its_own_shell() {
        let procs = vec![proc(10, 1, "powershell.exe")];
        assert_eq!(shells_under(10, &procs), vec![10]);
    }

    #[test]
    fn build_tools_running_under_the_prompt_are_not_shells() {
        // `cargo` has a working directory too, and it is not the session's.
        let procs = vec![
            proc(100, 1, "windowsterminal.exe"),
            proc(300, 100, "powershell.exe"),
            proc(400, 300, "cargo.exe"),
        ];
        assert_eq!(shells_under(100, &procs), vec![300]);
    }

    #[test]
    fn a_cycle_in_the_tree_does_not_hang() {
        let procs = vec![proc(1, 2, "a.exe"), proc(2, 1, "b.exe")];
        assert!(shells_under(1, &procs).is_empty());
    }

    #[test]
    fn terminal_directories_never_collide_with_explorer_folders() {
        let dir = Path::new("C:\\work\\proj");
        assert_eq!(terminal_uri(dir), "terminal://c:/work/proj");
        assert_ne!(terminal_uri(dir), crate::enrich::path_uri(dir));
    }

    #[test]
    fn the_default_directories_a_shell_opens_in_are_not_work() {
        assert!(is_uninteresting(Path::new("C:\\Windows\\System32")));
        assert!(!is_uninteresting(Path::new("C:\\work\\proj")));
    }
}

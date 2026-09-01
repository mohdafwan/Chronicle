//! How the recorder comes up: registering with Windows, and making sure only
//! one copy of it is ever running.
//!
//! Registration goes in the per-user `Run` key rather than a scheduled task.
//! A scheduled task would start without any console flash and could even run
//! before login — but it also hides from Task Manager's Startup tab, which is
//! the one place people go to ask "what starts itself on this machine?".
//! Chronicle's first principle is being transparent about what it records; a
//! recorder that cannot be found in the obvious place fails that principle
//! before it records anything. The Run key is visible, and the switch Windows
//! puts next to it genuinely turns Chronicle off.
//!
//! The single-instance lock exists because autostart makes a second recorder
//! likely: the daemon comes up at login, then a terminal window runs
//! `chronicled run` out of habit. Two recorders on one database double every
//! observation and leave two sessionisers disagreeing about where a session
//! ends.

use anyhow::Result;

/// The name Windows shows for the entry, in Task Manager and in Settings.
pub const ENTRY_NAME: &str = "Chronicle";

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result, bail};
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
    use windows::Win32::System::Console::FreeConsole;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
        RegSetKeyValueW,
    };
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    /// One recorder per user, not per session: a fast-user-switch should not
    /// start a second one against the same database.
    const LOCK_NAME: &str = r"Local\Chronicle.recorder";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn pcwstr(buf: &[u16]) -> PCWSTR {
        PCWSTR::from_raw(buf.as_ptr())
    }

    /// The exact command line that would be registered.
    ///
    /// Whatever binary you ran `autostart on` from is the binary that gets
    /// registered — quoted, because Chronicle installs under a path with a
    /// space in it more often than not.
    pub fn command() -> Result<String> {
        let exe = std::env::current_exe().context("finding this executable")?;
        Ok(format!("\"{}\" run --background", exe.display()))
    }

    /// What is registered right now, if anything.
    pub fn registered() -> Option<String> {
        let key = wide(RUN_KEY);
        let name = wide(super::ENTRY_NAME);
        let mut buf = [0u16; 1024];
        let mut size = std::mem::size_of_val(&buf) as u32;

        let err = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                pcwstr(&key),
                pcwstr(&name),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut _),
                Some(&mut size),
            )
        };
        if err.is_err() {
            return None;
        }
        // `size` comes back in bytes and includes the terminator.
        let len = (size as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..len.min(buf.len())]))
    }

    pub fn enable() -> Result<String> {
        let cmd = command()?;
        let key = wide(RUN_KEY);
        let name = wide(super::ENTRY_NAME);
        let value = wide(&cmd);
        let bytes = std::mem::size_of_val(&value[..]) as u32;

        let err = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                pcwstr(&key),
                pcwstr(&name),
                REG_SZ.0,
                Some(value.as_ptr() as *const _),
                bytes,
            )
        };
        if err.is_err() {
            bail!("could not write the Run key: {err:?}");
        }
        Ok(cmd)
    }

    pub fn disable() -> Result<bool> {
        if registered().is_none() {
            return Ok(false);
        }
        let key = wide(RUN_KEY);
        let name = wide(super::ENTRY_NAME);
        let err = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, pcwstr(&key), pcwstr(&name)) };
        if err.is_err() {
            bail!("could not remove the Run key: {err:?}");
        }
        Ok(true)
    }

    /// Held for the lifetime of the recorder. Dropping it lets the next one in.
    pub struct InstanceLock(HANDLE);

    impl Drop for InstanceLock {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) }.ok();
        }
    }

    /// `None` when another recorder already holds the lock.
    pub fn acquire_lock() -> Option<InstanceLock> {
        let name = wide(LOCK_NAME);
        let handle = unsafe { CreateMutexW(None, true, pcwstr(&name)) }.ok()?;
        // The handle comes back even when the mutex already existed, so the
        // last error is the only thing that distinguishes owner from gatecrasher.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) }.ok();
            return None;
        }
        Some(InstanceLock(handle))
    }

    /// Drop the console this process was given at login.
    ///
    /// A console-subsystem binary launched from the Run key gets a console
    /// window, and the recorder never returns, so the window would sit on the
    /// desktop for the whole session. Detaching closes it — conhost exits once
    /// nothing is attached — leaving the brief flash and nothing else.
    pub fn detach_console() {
        unsafe { FreeConsole() }.ok();
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, bail};

    pub fn registered() -> Option<String> {
        None
    }
    pub fn enable() -> Result<String> {
        bail!("autostart is only implemented on Windows")
    }
    pub fn disable() -> Result<bool> {
        bail!("autostart is only implemented on Windows")
    }
    pub struct InstanceLock;
    pub fn acquire_lock() -> Option<InstanceLock> {
        Some(InstanceLock)
    }
    pub fn detach_console() {}
}

pub use imp::{acquire_lock, detach_console, disable, enable, registered};

/// Whether the registered command still points at a binary that exists.
///
/// A stale entry is worse than none: Windows tries it every login, fails
/// silently, and the user is left believing Chronicle is recording.
pub fn registered_target_exists() -> Option<bool> {
    let cmd = registered()?;
    let exe = cmd
        .strip_prefix('"')
        .and_then(|rest| rest.split_once('"'))
        .map(|(exe, _)| exe.to_string())
        .unwrap_or_else(|| cmd.split_whitespace().next().unwrap_or("").to_string());
    Some(std::path::Path::new(&exe).exists())
}

/// One line for `chronicled status`.
pub fn describe() -> String {
    match registered() {
        None => "not registered".to_string(),
        Some(cmd) => match registered_target_exists() {
            Some(false) => format!("registered, but the binary is missing — {cmd}"),
            _ => format!("starts with Windows — {cmd}"),
        },
    }
}

/// `chronicled autostart [on|off]`
pub fn command_line(action: Option<&str>) -> Result<()> {
    match action {
        Some("on") => {
            let cmd = enable()?;
            println!("Chronicle will now start with Windows.");
            println!("  {cmd}");
            println!(
                "\nTask Manager → Startup apps lists it as \"{ENTRY_NAME}\", and the switch there turns it off."
            );
            if cmd.contains("target\\debug") || cmd.contains("target/debug") {
                println!(
                    "\nNote: that is a debug build inside the source tree. Re-run `autostart on`\nfrom the release binary once you have one, or the entry breaks when you clean."
                );
            }
        }
        Some("off") => {
            if disable()? {
                println!("Chronicle will no longer start with Windows.");
            } else {
                println!("Chronicle was not registered to start with Windows.");
            }
        }
        None | Some("status") => println!("{}", describe()),
        Some(other) => anyhow::bail!("unknown option {other:?} — use `on`, `off`, or `status`"),
    }
    Ok(())
}

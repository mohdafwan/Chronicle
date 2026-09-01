//! Which applications are actually on this machine.
//!
//! Settings → Sources used to list Chronicle's whole catalogue: every browser,
//! every JetBrains IDE, every password manager, whether or not any of them were
//! installed. That is a list of Chronicle's opinions, not of the user's
//! computer, and the one app they actually wanted to switch off was somewhere
//! past Krita. Worse, an installed app the catalogue had never heard of could
//! not be configured at all until Chronicle happened to see it running — which
//! is exactly backwards for a privacy control.
//!
//! Nothing here is stored. The scan runs when the Sources panel is opened, the
//! result is used to build that list, and it is dropped. Chronicle keeps no
//! inventory of installed software, and this never touches the network.
//!
//! Two registry views, because neither is complete on its own:
//!
//! * `App Paths` maps an executable name straight to its full path, which is
//!   the same identity Chronicle already files sessions under.
//! * The uninstall keys are what "Add or remove programs" reads, and catch the
//!   applications that never registered an app path.
//!
//! Store apps appear in neither, since `WindowsApps` is not readable. Those
//! arrive from the recorder instead: anything Chronicle has watched running is
//! installed by definition, and the caller merges that in.

#[cfg(windows)]
use std::collections::BTreeMap;
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

/// One application found on this machine.
#[derive(Debug, Clone)]
pub struct InstalledApp {
    /// Lowercased executable file name — Chronicle's app identity.
    pub app_id: String,
    /// The name the installer registered, when there is one.
    pub display_name: Option<String>,
    pub exe_path: PathBuf,
}

#[cfg(windows)]
pub fn scan() -> Vec<InstalledApp> {
    let mut found: BTreeMap<String, InstalledApp> = BTreeMap::new();

    // App Paths first: it is keyed by executable name, so it needs no guessing,
    // and anything it reports overrides a name derived from an uninstall entry.
    for (name, path) in imp::app_paths() {
        insert(&mut found, path, Some(name));
    }
    for (name, path) in imp::uninstall_entries() {
        insert(&mut found, path, Some(name));
    }

    found.into_values().collect()
}

#[cfg(not(windows))]
pub fn scan() -> Vec<InstalledApp> {
    Vec::new()
}

#[cfg(windows)]
fn insert(into: &mut BTreeMap<String, InstalledApp>, exe: PathBuf, name: Option<String>) {
    let Some(app_id) = app_id_of(&exe) else {
        return;
    };
    // An installer that points at `uninstall.exe` has told us where it lives,
    // not what it is. Recording that as an application would offer the user a
    // capture setting for a program they never run.
    if is_not_an_application(&app_id) || is_system_path(&exe) {
        return;
    }
    if name.as_deref().is_some_and(is_supporting_software) {
        return;
    }
    if !exe.exists() {
        return;
    }
    into.entry(app_id.clone()).or_insert(InstalledApp {
        app_id,
        display_name: name.map(|n| clean_name(&n)),
        exe_path: exe,
    });
}

#[cfg(windows)]
fn app_id_of(exe: &Path) -> Option<String> {
    let name = exe.file_name()?.to_string_lossy().to_ascii_lowercase();
    name.ends_with(".exe").then_some(name)
}

/// Words that mark an uninstall entry as part of something rather than a thing.
///
/// The uninstall list is not a list of applications — it is a list of anything
/// that knows how to remove itself, which includes every redistributable,
/// driver and language pack on the machine. None of those has a window, so
/// none of them can ever appear in a session, and a capture setting for one is
/// a row of noise between the user and the app they came to switch off.
///
/// This is a heuristic and it will occasionally be wrong in both directions,
/// which is why `chronicled sources --all` still lists the whole catalogue.
const NOT_A_USER_APP: &[&str] = &[
    "redistributable",
    "runtime",
    "driver",
    " sdk",
    "sdk ",
    " update",
    "plugin",
    "add-in",
    "service pack",
    "diagnostic",
    "setup",
    "installer",
    "framework",
    "codec",
    "language pack",
    "recovery",
    "firmware",
    "support assist",
    "supportassist",
    "webview2",
    "visual c++",
    "windows software development",
];

/// Public because the same judgement has to be applied to applications the
/// recorder has seen: an installer's progress window is a window, and VS Code
/// updating itself should not leave `CodeSetup-stable-08d4889f` in the
/// settings list forever.
pub fn is_supporting_software(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // A DisplayName that is just a file name was written by something that had
    // no name to give.
    if lower.ends_with(".exe") {
        return true;
    }
    NOT_A_USER_APP.iter().any(|w| lower.contains(w))
}

/// Executables that live inside Windows itself rather than being installed.
#[cfg(windows)]
fn is_system_path(exe: &Path) -> bool {
    let lower = exe.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
    lower.contains("/windows/system32/")
        || lower.contains("/windows/syswow64/")
        || lower.contains("/windows/servicing/")
}

/// Executables that an uninstall entry points at which are not the application.
#[cfg(windows)]
fn is_not_an_application(app_id: &str) -> bool {
    const NOT_APPS: &[&str] = &[
        "uninstall.exe",
        "uninst.exe",
        "unins000.exe",
        "setup.exe",
        "install.exe",
        "installer.exe",
        "msiexec.exe",
        "rundll32.exe",
        "cmd.exe",
        "powershell.exe",
        "control.exe",
        "regsvr32.exe",
    ];
    NOT_APPS.contains(&app_id) || app_id.starts_with("unins")
}

/// Installer display names carry version numbers and vendor prefixes that make
/// a settings list hard to scan. "Mozilla Firefox (x64 en-US)" is Firefox.
#[cfg(windows)]
fn clean_name(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(i) = s.find(" (") {
        s = &s[..i];
    }
    let s = s
        .trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ')
        .trim();
    if s.is_empty() { raw.trim().to_string() } else { s.to_string() }
}

/// `DisplayIcon` is usually `C:\path\app.exe,0`, sometimes quoted.
#[cfg(windows)]
fn exe_from_icon(icon: &str) -> Option<PathBuf> {
    let s = icon.trim().trim_matches('"');
    let s = match s.rfind(',') {
        // Only strip a trailing `,0`, never a comma inside a directory name.
        Some(i) if s[i + 1..].trim().parse::<i32>().is_ok() => &s[..i],
        _ => s,
    };
    let s = s.trim().trim_matches('"');
    s.to_ascii_lowercase().ends_with(".exe").then(|| PathBuf::from(s))
}

#[cfg(windows)]
mod imp {
    use super::exe_from_icon;
    use std::path::PathBuf;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_READ,
        KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_SAM_FLAGS, RRF_RT_REG_SZ, RegCloseKey,
        RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    };
    use windows::core::{PCWSTR, PWSTR};

    const APP_PATHS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths";
    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            let _ = unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(root: HKEY, path: &str, extra: REG_SAM_FLAGS) -> Option<Key> {
        let w = wide(path);
        let mut key = HKEY::default();
        let err = unsafe {
            RegOpenKeyExW(
                root,
                PCWSTR::from_raw(w.as_ptr()),
                None,
                KEY_READ | KEY_ENUMERATE_SUB_KEYS | extra,
                &mut key,
            )
        };
        (err == ERROR_SUCCESS).then_some(Key(key))
    }

    fn subkeys(key: &Key) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0u32;
        loop {
            // 255 is the documented maximum length of a registry key name.
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            let err = unsafe {
                RegEnumKeyExW(
                    key.0,
                    i,
                    Some(PWSTR::from_raw(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if err != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
            i += 1;
        }
        out
    }

    /// One string value from `root\path`, empty name meaning the default value.
    fn string_value(root: HKEY, path: &str, name: &str, extra: REG_SAM_FLAGS) -> Option<String> {
        let key = open(root, path, extra)?;
        let name_w = wide(name);
        let mut buf = [0u16; 1024];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let err = unsafe {
            RegGetValueW(
                key.0,
                PCWSTR::null(),
                PCWSTR::from_raw(name_w.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut _),
                Some(&mut size),
            )
        };
        if err.is_err() {
            return None;
        }
        let len = (size as usize / 2).saturating_sub(1);
        let s = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
        (!s.trim().is_empty()).then(|| s.trim().to_string())
    }

    /// The registry is split for 32- and 64-bit software, and a user can
    /// install for themselves alone. All four views, or half the machine's
    /// applications look uninstalled.
    fn views() -> [(HKEY, REG_SAM_FLAGS); 3] {
        [
            (HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY),
            (HKEY_LOCAL_MACHINE, KEY_WOW64_32KEY),
            (HKEY_CURRENT_USER, REG_SAM_FLAGS(0)),
        ]
    }

    /// `App Paths` subkeys are named after the executable, and the key's
    /// default value is its full path.
    pub fn app_paths() -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for (root, flags) in views() {
            let Some(key) = open(root, APP_PATHS, flags) else {
                continue;
            };
            for name in subkeys(&key) {
                let path = format!("{APP_PATHS}\\{name}");
                if let Some(full) = string_value(root, &path, "", flags) {
                    let full = full.trim_matches('"').to_string();
                    out.push((name.trim_end_matches(".exe").to_string(), PathBuf::from(full)));
                }
            }
        }
        out
    }

    /// What "Add or remove programs" lists. `DisplayIcon` is the only field
    /// that reliably names an executable; `InstallLocation` is a directory and
    /// guessing which file inside it is the application would be guessing.
    pub fn uninstall_entries() -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for (root, flags) in views() {
            let Some(key) = open(root, UNINSTALL, flags) else {
                continue;
            };
            for name in subkeys(&key) {
                let path = format!("{UNINSTALL}\\{name}");
                // A system component is a patch or a runtime, not something a
                // person opens.
                if string_value(root, &path, "SystemComponent", flags).is_some() {
                    continue;
                }
                let Some(display) = string_value(root, &path, "DisplayName", flags) else {
                    continue;
                };
                let Some(exe) = string_value(root, &path, "DisplayIcon", flags)
                    .as_deref()
                    .and_then(exe_from_icon)
                else {
                    continue;
                };
                out.push((display, exe));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uninstall_list_is_not_a_list_of_applications() {
        // Everything here was really in the list on the machine this was
        // written on, between Brave and Visual Studio Code.
        assert!(is_supporting_software("Microsoft Visual C++ v14 Redistributable"));
        assert!(is_supporting_software("Microsoft Windows Desktop Runtime"));
        assert!(is_supporting_software("Dell PointStick Driver"));
        assert!(is_supporting_software("Dell SupportAssist OS Recovery Plugin for Dell Update"));
        assert!(is_supporting_software("IEDIAG.EXE"));
        assert!(is_supporting_software("Codesetup Stable 08d4889f9ec4a1685d257b9b95de036c8"));

        assert!(!is_supporting_software("Visual Studio Code"));
        assert!(!is_supporting_software("Brave"));
        assert!(!is_supporting_software("Figma"));
        assert!(!is_supporting_software("Android Studio"));
    }
}

/// Windows shapes: a registry `DisplayIcon`, an installer's display name, a
/// path under `C:\Windows`. None of them mean anything on another platform,
/// and the functions they cover are not compiled there.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn a_display_icon_gives_up_its_executable() {
        assert_eq!(
            exe_from_icon(r"C:\Program Files\Thing\thing.exe,0"),
            Some(PathBuf::from(r"C:\Program Files\Thing\thing.exe"))
        );
        assert_eq!(
            exe_from_icon(r#""C:\Program Files\Thing\thing.exe""#),
            Some(PathBuf::from(r"C:\Program Files\Thing\thing.exe"))
        );
    }

    #[test]
    fn a_comma_in_a_folder_name_is_not_an_icon_index() {
        assert_eq!(
            exe_from_icon(r"C:\Apps\Smith, Jones\app.exe"),
            Some(PathBuf::from(r"C:\Apps\Smith, Jones\app.exe"))
        );
    }

    #[test]
    fn an_icon_that_is_not_an_executable_is_ignored() {
        assert!(exe_from_icon(r"C:\Program Files\Thing\thing.ico").is_none());
        assert!(exe_from_icon("").is_none());
    }

    #[test]
    fn installer_names_lose_their_version_and_locale() {
        assert_eq!(clean_name("Mozilla Firefox (x64 en-US)"), "Mozilla Firefox");
        assert_eq!(clean_name("Visual Studio Code 1.95.2"), "Visual Studio Code");
        assert_eq!(clean_name("Figma"), "Figma");
    }

    #[test]
    fn a_name_that_is_only_digits_is_left_alone() {
        // Trimming would leave nothing, and no name is worse than an odd one.
        assert_eq!(clean_name("7"), "7");
    }

    #[test]
    fn windows_own_binaries_are_not_installed_applications() {
        assert!(is_system_path(Path::new(r"C:\Windows\System32\notepad.exe")));
        assert!(!is_system_path(Path::new(r"C:\Program Files\Thing\thing.exe")));
    }

    #[test]
    fn uninstallers_are_not_applications() {
        assert!(is_not_an_application("unins000.exe"));
        assert!(is_not_an_application("uninstall.exe"));
        assert!(is_not_an_application("msiexec.exe"));
        assert!(!is_not_an_application("code.exe"));
    }
}

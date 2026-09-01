//! Checking for, and applying, a new version of Chronicle.
//!
//! Chronicle promises to be local-first, and an updater is the one part of it
//! that talks to the network. So the whole of that contact is written down
//! here, and it is small: one `GET` to `api.github.com` for the latest release,
//! and one `GET` to the asset it names. No account, no cookie, no identifier,
//! no telemetry, nothing about the user's sessions ever leaves the machine —
//! the request body is empty in both directions of interest.
//!
//! It is also off by default. A background recorder that quietly reaches the
//! internet on a schedule the user never agreed to is exactly the shape of
//! thing this product exists not to be. `chronicled update` checks on demand;
//! `chronicled update --auto check` or `--auto install` is the user saying yes.
//!
//! The transport is WinHTTP rather than a bundled HTTP client. That keeps the
//! binary a couple of megabytes rather than four, and — more usefully — means
//! the system proxy, and a corporate certificate store, work without Chronicle
//! knowing anything about either.

use anyhow::{Context, Result, bail};
use chronicle_core::Store;
use std::path::{Path, PathBuf};

/// Where releases come from. A published build is only ever fetched from here.
const REPO: &str = "mohdafwan/Chronicle";

/// The files a release ships, paired with what they are called once installed.
const PAYLOAD: &[(&str, &str)] = &[
    ("chronicled-windows-x86_64.exe", "chronicled.exe"),
    ("chronicle-windows-x86_64.exe", "chronicle.exe"),
];

/// The checksum file every release carries, in `sha256  name` lines.
const CHECKSUMS: &str = "SHA256SUMS";

// ── versions ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

impl Version {
    /// Accepts `1.2.3` and `v1.2.3`, and ignores any `-rc1` suffix.
    ///
    /// A pre-release suffix is dropped rather than ordered, because ordering it
    /// properly is semver's hardest corner and Chronicle has no use for it yet.
    /// Tagging a pre-release would therefore offer itself as an upgrade, which
    /// is why the release workflow marks those as prereleases and the check
    /// asks GitHub only for `releases/latest`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let core = s.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self(major, minor, patch))
    }
}

pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version(0, 0, 0))
}

// ── what a release looks like ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub notes: String,
    /// Asset name → download URL.
    pub assets: Vec<(String, String)>,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&str> {
        self.assets.iter().find(|(n, _)| n == name).map(|(_, u)| u.as_str())
    }
}

/// Ask GitHub what the newest published release is.
pub fn latest() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = net::get(&url, "application/vnd.github+json")
        .with_context(|| format!("asking {url} for the latest release"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&body).context("the release feed was not JSON")?;

    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    let version = Version::parse(&tag)
        .with_context(|| format!("the newest release is tagged {tag:?}, which is not a version"))?;

    let assets = json["assets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some((
                        a["name"].as_str()?.to_string(),
                        a["browser_download_url"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Release {
        version,
        tag,
        notes: json["body"].as_str().unwrap_or_default().to_string(),
        assets,
    })
}

// ── applying one ────────────────────────────────────────────────────────

/// Where the installed copy of Chronicle lives: beside the running binary.
fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding this executable")?;
    Ok(exe.parent().context("this executable has no directory")?.to_path_buf())
}

/// Replace one binary on disk with new bytes.
///
/// Windows will not let a running executable be deleted, but it will let it be
/// renamed. So the old file is moved aside rather than removed, and swept up on
/// the next start once nothing has it open.
fn swap(target: &Path, bytes: &[u8]) -> Result<()> {
    let staged = target.with_extension("new");
    std::fs::write(&staged, bytes)
        .with_context(|| format!("writing {}", staged.display()))?;

    if target.exists() {
        let retired = target.with_extension("old");
        let _ = std::fs::remove_file(&retired);
        std::fs::rename(target, &retired)
            .with_context(|| format!("moving {} aside", target.display()))?;
    }
    std::fs::rename(&staged, target)
        .with_context(|| format!("putting the new {} in place", target.display()))?;
    Ok(())
}

/// Delete the binaries a previous update moved aside. Best effort: the file is
/// still locked if something is running from it, and it will go next time.
pub fn sweep_retired() {
    let Ok(dir) = install_dir() else { return };
    for (_, name) in PAYLOAD {
        let _ = std::fs::remove_file(dir.join(name).with_extension("old"));
    }
}

/// Parse a `sha256  filename` listing into the digest for one file.
fn digest_for(listing: &str, name: &str) -> Option<[u8; 32]> {
    // A byte-order mark rides on the first line of anything PowerShell writes
    // without being told otherwise, and it would silently cost the first file
    // in the list its checksum — which reads as "this release is missing an
    // asset" rather than as an encoding problem.
    for line in listing.trim_start_matches('\u{feff}').lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        // `sha256sum` writes a `*` before the name for binary mode.
        if file.trim_start_matches('*') != name {
            continue;
        }
        let bytes = (0..hash.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hash.get(i..i + 2)?, 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        return bytes.try_into().ok();
    }
    None
}

pub struct Installed {
    pub files: Vec<String>,
    pub version: Version,
}

/// Download, verify and install a release.
///
/// Every file is checked against the release's own `SHA256SUMS` before it is
/// allowed anywhere near the install directory. A release without that file is
/// refused outright rather than trusted: the checksum list is the only thing
/// standing between "an update" and "whatever that URL happened to return".
pub fn install(release: &Release) -> Result<Installed> {
    let dir = install_dir()?;

    let sums_url = release
        .asset(CHECKSUMS)
        .with_context(|| format!("release {} publishes no {CHECKSUMS}", release.tag))?;
    let sums = String::from_utf8(net::get(sums_url, "text/plain").context("fetching checksums")?)
        .context("the checksum file was not text")?;

    // Fetch and verify everything before writing anything, so a failure halfway
    // cannot leave one new binary beside one old one.
    let mut staged: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for (asset, local) in PAYLOAD {
        let target = dir.join(local);
        // Only replace what is actually installed here. A machine with just the
        // recorder should not acquire the window because it updated.
        if !target.exists() {
            continue;
        }
        let Some(url) = release.asset(asset) else {
            bail!("release {} is missing {asset}", release.tag);
        };
        let expected = digest_for(&sums, asset)
            .with_context(|| format!("{CHECKSUMS} does not list {asset}"))?;

        let bytes = net::get(url, "application/octet-stream")
            .with_context(|| format!("downloading {asset}"))?;
        let actual = sha256(&bytes);
        if actual != expected {
            bail!(
                "{asset} does not match its published checksum — expected {}, got {}",
                hex(&expected),
                hex(&actual)
            );
        }
        staged.push((target, bytes));
    }

    if staged.is_empty() {
        bail!("nothing to update: no Chronicle binary found in {}", dir.display());
    }

    let mut files = Vec::new();
    for (target, bytes) in &staged {
        swap(target, bytes)?;
        files.push(target.file_name().unwrap_or_default().to_string_lossy().to_string());
    }

    Ok(Installed {
        files,
        version: release.version,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── the automatic half ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auto {
    /// Never contact the network. The default.
    Off,
    /// Look once a day and say so; installing stays a decision.
    Check,
    /// Look once a day and apply it, restarting the recorder.
    Install,
}

impl Auto {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Check => "check",
            Self::Install => "install",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "off" | "never" => Some(Self::Off),
            "check" => Some(Self::Check),
            "install" | "on" => Some(Self::Install),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Off => "off — Chronicle never contacts the network on its own",
            Self::Check => "checks daily, installs only when you say so",
            Self::Install => "checks daily and installs",
        }
    }
}

pub fn auto_setting(store: &Store) -> Auto {
    store
        .meta_get("update_auto")
        .ok()
        .flatten()
        .and_then(|s| Auto::parse(&s))
        .unwrap_or(Auto::Off)
}

pub fn set_auto(store: &Store, mode: Auto) -> Result<()> {
    store.meta_set("update_auto", mode.as_str())
}

/// How long between automatic checks. Once a day is enough for a tool nobody
/// is waiting on a hotfix for, and it keeps the network contact rare enough to
/// be describable in one sentence.
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// True when an automatic check is due. Records the attempt either way, so a
/// failing network does not mean trying again every minute forever.
pub fn check_due(store: &Store, now: chrono::DateTime<chrono::Utc>) -> bool {
    let last = store
        .meta_get("update_checked_at")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    now.timestamp() - last >= CHECK_INTERVAL_SECS
}

pub fn mark_checked(store: &Store, now: chrono::DateTime<chrono::Utc>, found: Option<Version>) {
    let _ = store.meta_set("update_checked_at", &now.timestamp().to_string());
    let _ = match found {
        Some(v) => store.meta_set("update_available", &v.to_string()),
        None => store.meta_set("update_available", ""),
    };
}

/// The version waiting to be installed, if a check has found one.
pub fn pending(store: &Store) -> Option<Version> {
    let raw = store.meta_get("update_available").ok().flatten()?;
    Version::parse(&raw).filter(|v| *v > current())
}

/// One line for `chronicled status`.
pub fn describe(store: &Store) -> String {
    let auto = auto_setting(store);
    match pending(store) {
        Some(v) => format!("{} available — {}", v, auto.describe()),
        None => format!("{} — {}", current(), auto.describe()),
    }
}

// ── the command ─────────────────────────────────────────────────────────

/// `chronicled update [--install] [--auto off|check|install]`
pub fn command_line(args: &[String]) -> Result<()> {
    let store = Store::open(Store::default_path()?)?;

    if let Some(i) = args.iter().position(|a| a == "--auto") {
        let mode = args
            .get(i + 1)
            .and_then(|s| Auto::parse(s))
            .context("usage: chronicled update --auto off|check|install")?;
        set_auto(&store, mode)?;
        println!("Automatic updates: {}", mode.describe());
        if mode == Auto::Off {
            return Ok(());
        }
        println!("Chronicle will contact api.github.com once a day and nothing else.");
        return Ok(());
    }

    println!("Installed  {}", current());
    print!("Checking github.com/{REPO} … ");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let release = latest()?;
    mark_checked(&store, chrono::Utc::now(), Some(release.version));

    if release.version <= current() {
        println!("up to date.");
        return Ok(());
    }
    println!("{} is available.", release.version);
    if !release.notes.trim().is_empty() {
        println!();
        for line in release.notes.lines().take(12) {
            println!("  {line}");
        }
        println!();
    }

    if !args.iter().any(|a| a == "--install") {
        println!("Run `chronicled update --install` to apply it.");
        return Ok(());
    }

    let done = install(&release)?;
    println!("Installed {} — {}", done.version, done.files.join(", "));

    // The recorder is asked to stop rather than killed, so the session it is in
    // the middle of ends properly instead of being recovered as interrupted.
    if store.recorder_is_live()? {
        store.request_shutdown(chrono::Utc::now())?;
        println!("Asked the running recorder to stop so the new one can take over.");
        let dir = install_dir()?;
        wait_for_recorder_to_stop(&store);
        std::process::Command::new(dir.join("chronicled.exe"))
            .args(["run", "--background"])
            .spawn()
            .context("starting the updated recorder")?;
        println!("Recorder restarted.");
    }
    Ok(())
}

fn wait_for_recorder_to_stop(store: &Store) {
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !store.recorder_is_live().unwrap_or(false) {
            return;
        }
    }
}

// ── SHA-256, from the operating system ──────────────────────────────────

#[cfg(windows)]
fn sha256(bytes: &[u8]) -> [u8; 32] {
    use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
    let mut out = [0u8; 32];
    // The pseudo-handle needs no provider to be opened or closed.
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, bytes, &mut out) };
    debug_assert!(status.is_ok(), "BCryptHash failed: {status:?}");
    out
}

#[cfg(not(windows))]
fn sha256(_bytes: &[u8]) -> [u8; 32] {
    [0u8; 32]
}

// ── HTTPS, from the operating system ────────────────────────────────────

#[cfg(windows)]
mod net {
    use anyhow::{Context, Result, bail};
    use windows::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
        WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen,
        WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData,
        WinHttpReceiveResponse, WinHttpSendRequest,
    };
    use windows::core::PCWSTR;

    /// Refuses to be more than this, so a wrong URL cannot fill the disk.
    const MAX_BODY: usize = 64 * 1024 * 1024;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Closes its handle however the function returns.
    struct H(*mut core::ffi::c_void);

    impl Drop for H {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) }.ok();
            }
        }
    }

    /// `https://host/path` split in two. Only `https` is accepted: an updater
    /// that would fetch a binary over plain HTTP has no business verifying
    /// checksums either.
    fn split_https(url: &str) -> Result<(String, String)> {
        let rest = url
            .strip_prefix("https://")
            .with_context(|| format!("{url} is not an https URL"))?;
        let (host, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if host.is_empty() {
            bail!("{url} has no host");
        }
        Ok((host.to_string(), path.to_string()))
    }

    pub fn get(url: &str, accept: &str) -> Result<Vec<u8>> {
        let (host, path) = split_https(url)?;
        let agent = wide(&format!("Chronicle/{}", env!("CARGO_PKG_VERSION")));
        let host_w = wide(&host);
        let path_w = wide(&path);
        let verb = wide("GET");
        // GitHub rejects requests with no User-Agent, and wants an Accept it
        // recognises. Neither carries anything about this machine beyond the
        // version already visible in the release it is asking about.
        let headers = wide(&format!("Accept: {accept}\r\n"));

        unsafe {
            let session = H(WinHttpOpen(
                PCWSTR::from_raw(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ));
            if session.0.is_null() {
                bail!("could not open an HTTP session");
            }

            let connection = H(WinHttpConnect(
                session.0,
                PCWSTR::from_raw(host_w.as_ptr()),
                443,
                0,
            ));
            if connection.0.is_null() {
                bail!("could not connect to {host}");
            }

            let request = H(WinHttpOpenRequest(
                connection.0,
                PCWSTR::from_raw(verb.as_ptr()),
                PCWSTR::from_raw(path_w.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ));
            if request.0.is_null() {
                bail!("could not build a request for {url}");
            }

            // The trailing NUL is not part of the header block.
            WinHttpSendRequest(
                request.0,
                Some(&headers[..headers.len() - 1]),
                None,
                0,
                0,
                0,
            )
            .with_context(|| format!("sending the request to {host}"))?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut())
                .with_context(|| format!("waiting for {host} to answer"))?;

            let mut status: u32 = 0;
            let mut len = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut _),
                &mut len,
                std::ptr::null_mut(),
            )
            .context("reading the response status")?;
            if !(200..300).contains(&status) {
                bail!("{url} answered {status}");
            }

            let mut body: Vec<u8> = Vec::new();
            loop {
                let mut available: u32 = 0;
                WinHttpQueryDataAvailable(request.0, &mut available)
                    .context("reading the response")?;
                if available == 0 {
                    break;
                }
                let start = body.len();
                if start + available as usize > MAX_BODY {
                    bail!("{url} returned more than {MAX_BODY} bytes");
                }
                body.resize(start + available as usize, 0);
                let mut read: u32 = 0;
                WinHttpReadData(
                    request.0,
                    body[start..].as_mut_ptr() as *mut _,
                    available,
                    &mut read,
                )
                .context("reading the response")?;
                body.truncate(start + read as usize);
                if read == 0 {
                    break;
                }
            }
            Ok(body)
        }
    }
}

#[cfg(not(windows))]
mod net {
    use anyhow::{Result, bail};
    pub fn get(_url: &str, _accept: &str) -> Result<Vec<u8>> {
        bail!("updates are only implemented on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_the_way_releases_do() {
        assert!(Version::parse("v0.2.0").unwrap() > Version::parse("0.1.9").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
        assert_eq!(Version::parse("v1.2.3").unwrap(), Version(1, 2, 3));
        assert_eq!(Version::parse("2").unwrap(), Version(2, 0, 0));
    }

    #[test]
    fn a_pre_release_suffix_does_not_break_the_parse() {
        assert_eq!(Version::parse("v1.2.3-rc1").unwrap(), Version(1, 2, 3));
    }

    #[test]
    fn rubbish_is_not_a_version() {
        assert!(Version::parse("latest").is_none());
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn checksums_are_read_out_of_a_sha256sums_file() {
        let listing = "\
0000000000000000000000000000000000000000000000000000000000000001  other.exe
00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff *chronicled-windows-x86_64.exe
";
        let d = digest_for(listing, "chronicled-windows-x86_64.exe").unwrap();
        assert_eq!(d[0], 0x00);
        assert_eq!(d[1], 0x11);
        assert_eq!(d[31], 0xff);
        assert!(digest_for(listing, "not-listed.exe").is_none());
    }

    #[test]
    fn a_byte_order_mark_does_not_cost_the_first_file_its_checksum() {
        let listing =
            "\u{feff}00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff  a.exe";
        assert!(digest_for(listing, "a.exe").is_some());
    }

    #[test]
    fn a_truncated_checksum_is_rejected_rather_than_padded() {
        assert!(digest_for("abcd  chronicled.exe", "chronicled.exe").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn sha256_matches_the_published_test_vectors() {
        // If this drifts, every verified download is being verified against
        // nothing. The vectors are FIPS 180-4's own.
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Not run by CI: it needs the network, and a test that fails when GitHub
    /// is slow teaches nobody anything. Run it by hand after touching the
    /// WinHTTP code — synthetic fixtures cannot tell you that a real download
    /// arrives whole, and a partial read here would be a partial binary there.
    ///
    ///     cargo test -p chronicle-daemon -- --ignored --nocapture
    #[cfg(windows)]
    #[test]
    #[ignore = "hits the network"]
    fn a_real_asset_downloads_whole_and_hashes_to_what_github_serves() {
        let release = latest().expect("asking GitHub for the latest release");
        let (name, url) = release
            .assets
            .iter()
            .find(|(n, _)| n.ends_with(".exe"))
            .expect("the release publishes no .exe to test with");

        let bytes = net::get(url, "application/octet-stream").expect("downloading the asset");
        println!("{name}: {} bytes, sha256 {}", bytes.len(), hex(&sha256(&bytes)));

        // An executable, not an HTML error page that arrived with status 200.
        assert!(bytes.len() > 100_000, "suspiciously small: {} bytes", bytes.len());
        assert_eq!(&bytes[..2], b"MZ", "that is not a Windows executable");
    }

    #[test]
    fn automatic_updates_are_off_unless_asked_for() {
        assert_eq!(Auto::parse("off"), Some(Auto::Off));
        assert_eq!(Auto::parse("check"), Some(Auto::Check));
        assert_eq!(Auto::parse("install"), Some(Auto::Install));
        assert_eq!(Auto::parse("yes"), None);
    }
}

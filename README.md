# Chronicle

Remembers your digital workspace so you never have to remember where you left off.

Chronicle watches which applications, projects, files and pages you were working
on, groups them into **work sessions**, and puts the whole desk back with one
keystroke after a crash, a restart, or a weekend.

The full product design specification lives in [`chronicle.html`](chronicle.html).

---

## Where the build is

| Component | Crate | State |
|---|---|---|
| Data model, capture policy, redaction | `chronicle-core` | Working, 22 tests |
| SQLite store + FTS5 search | `chronicle-core::store` | Working |
| Sessioniser (boundary rules, titling) | `chronicle-core::sessionize` | Working |
| Windows observer (Win32) | `chronicle-observer` | Working on real windows, 10 tests |
| Enrichers: VS Code, JetBrains, docs, paths | `chronicle-observer::enrich` | Working |
| Recorder daemon + inspection CLI | `chronicle-daemon` | Recording |
| Restore planner, fidelity ladder, executor | `chronicle-restore` | Working, 7 tests |
| End-to-end pipeline | `crates/restore/tests` | 3 integration tests |
| Chrome/Edge/Brave tab capture (SNSS) | `chronicle-observer::chrome` | Working on real files, 7 tests |
| Explorer folder paths (IShellWindows COM) | `chronicle-observer::explorer` | Working on real windows |
| `chronicled scan` — enricher coverage report | `chronicle-daemon` | Working |
| Terminal working directory | — | Not started |
| Explorer folder paths | — | Not started |
| Restore: Undo | — | Not started |
| Window (Tauri 2 + WebView2) | `src-tauri`, `ui/` | Working, 38 MB resident

**59 tests, all passing.** `cargo clippy --workspace --all-targets` is clean
with zero warnings.

## Decisions taken

- **Windows first.** The design specification is macOS-first; development is
  Windows-first because that is the machine it has to run on.
- **No browser extension.** Chrome tabs will be read from Chrome's own `SNSS`
  session files and restored with `chrome.exe --new-window url1 url2 …`. Tab
  order survives; pinning and tab groups do not.
- **Rust core + Tauri shell.** Chosen against a ~60 MB resident budget, and so
  the observer, sessioniser and store are written once for macOS later.
- **Plain SQLite for now.** The design calls for encryption at rest via
  SQLCipher. The schema and access paths are ready for it; swapping the driver
  is a contained change and is tracked as its own milestone. **Until then the
  database is not encrypted.**

## Layout

```
crates/
  core/       model, policy, redaction, store, sessioniser  (cross-platform)
  observer/   Win32 sampling + enrichers + Chrome SNSS      (Windows)
  restore/    planner, fidelity ladder, executor
  daemon/     chronicled — the recorder and the CLI
src-tauri/    the window: IPC commands over the same store
ui/           frontend — three files, no framework, no build step
adapters/     declarative per-app restore recipes           (empty)
```

## Running it

```
cargo run -p chronicle-daemon -- doctor      # what Chronicle sees right now
cargo run -p chronicle-daemon -- run         # start recording
cargo run -p chronicle-daemon -- sessions    # what it remembered
cargo run -p chronicle-daemon -- show 3      # one session in full
cargo run -p chronicle-daemon -- search figma
cargo run -p chronicle-daemon -- tabs        # what it reads from your browsers
cargo run -p chronicle-daemon -- sources     # per-app capture policy
cargo run -p chronicle-daemon -- plan 3      # what a restore would do
cargo run -p chronicle-daemon -- restore 3 --go
cargo run -p chronicle-daemon -- forget 2    # delete the last two hours
```

`doctor` is the one to reach for when an enricher is not picking up an
application: it prints the raw window sample and exactly what would be stored.

The database is a single file at
`%APPDATA%\Chronicle\data\chronicle.db` (`chronicled path` prints it).
Deleting that file is a complete and permanent reset.

## Two processes

`chronicled run` records; `chronicle` is the window. They share one SQLite file
and never talk to each other — the window knows the recorder is alive because
its heartbeat is fresh, not because it asked.

```
cargo run -p chronicle-daemon -- run     # the recorder
cargo run -p chronicle-app               # the window
```

Keyboard: `↑`/`↓` sessions, `Enter` restore, `Ctrl+K` search, `F2` rename,
`Ctrl+P` pin, `Esc` clear.

## The window makes no network requests

The frontend uses the platform's own fonts and ships no external assets. The
content security policy in `tauri.conf.json` allows `'self'` and the Tauri IPC
channel only, so this is enforced by the webview rather than by good intentions.
The update check is the one piece of network in the product, it lives in the
recorder rather than the window, and it is off until switched on — see
[Updating](#updating).
The cost is that the app does not use the specification's typefaces; bundling
them locally is a follow-up, not a reason to fetch them at runtime.

## Starting with Windows

```
chronicled autostart on      # register
chronicled autostart off     # remove
chronicled autostart         # what is registered right now
```

Registration writes one value to the per-user `Run` key,
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Chronicle`, pointing at
whichever binary `autostart on` was run from, with `run --background`.

The Run key rather than a scheduled task, deliberately. A scheduled task starts
without a console flash and can run before login, but it does not appear in Task
Manager's Startup tab — and a recorder that cannot be found in the place people
look fails the transparency principle before it records anything. The switch
Windows puts next to the entry genuinely turns Chronicle off.

`--background` changes two things: the console the Run key hands a
console-subsystem binary is released with `FreeConsole` (otherwise it would sit
on the desktop all day), and logs go to `chronicled.log` beside the database
instead of stdout. The log is truncated past a megabyte rather than rotated; it
exists to answer "did the recorder come up at login?".

Because autostart makes a second recorder likely — the daemon comes up at login,
then a terminal runs `chronicled run` out of habit — `run` now takes a named
mutex and exits immediately if another recorder holds it. Two recorders on one
database would double every observation and leave two sessionisers disagreeing
about where a session ends.

Register a binary that lives outside the build directory. A path under
`target\release` is held open by the running recorder, so the next
`cargo build --release` fails, and `cargo clean` breaks the entry silently.

## How a terminal's directory is found

A terminal window's title is just "Windows PowerShell", and
`windowsterminal.exe` is not itself in any directory — the shell is a separate
process. So the directory is read from the shell's own memory: the PEB, via
`NtQueryInformationProcess`, then `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory`
with `ReadProcessMemory`. It needs `PROCESS_VM_READ`, which a user has over
their own processes without elevation. Nothing is written, nothing is injected,
and the command line — which sits a few fields further along in the same
structure — is deliberately not read.

Finding the shell means walking the process tree, and the two terminal shapes
run opposite ways round. Windows Terminal hosts each tab as a descendant, an
`OpenConsole.exe` with the shell beneath it. A classic console window belongs to
`conhost.exe`, which is a *child* of the shell. Both are checked.

Only recognised shells count. A `cargo build` running under the prompt has a
working directory too, and filing the session under whatever the compiler was
doing would be worse than filing nothing.

Directories are stored as `terminal://c:/work/proj`, not `file:///`. Artifact
URIs are unique and the owning application is fixed the first time a URI is
seen, so sharing the scheme with the Explorer enricher would mean whichever saw
a folder first decided, for everyone afterwards, whether restoring it opens a
file manager or a prompt.

Restore uses Windows Terminal's own tab syntax, `wt -d A ; new-tab -d B`, so
four directories come back as four tabs of one window rather than four windows.
Nothing typed at those prompts is replayed: Chronicle cannot tell a build from a
deploy.

## Updating

```
chronicled update                       # ask GitHub whether there is a newer one
chronicled update --install             # fetch it, verify it, restart the recorder
chronicled update --auto off            # the default
chronicled update --auto check          # look once a day, tell me, install nothing
chronicled update --auto install        # look once a day and apply it
```

This is the only part of Chronicle that uses the network, so the whole of that
contact is worth stating exactly: one `GET` to `api.github.com` for the latest
release, and one `GET` to each asset it names. No account, no cookie, no
identifier, no telemetry. Nothing about a session, a title, a path or a URL
ever leaves the machine. The request carries a `User-Agent` of
`Chronicle/<version>` because GitHub rejects requests without one.

It is **off by default**. A background recorder that quietly reaches the
internet on a schedule the user never agreed to is the shape of thing this
product exists not to be, so the automatic check is something the user turns on
rather than something they discover.

Every download is checked against the release's own `SHA256SUMS` before it is
allowed near the install directory, and a release that does not publish that
file is refused rather than trusted. The hash comes from the operating system's
own SHA-256 (`BCryptHash`), and the transport is WinHTTP rather than a bundled
HTTP client — which keeps the binary around 2.5 MB instead of 4, and means the
system proxy and any corporate certificate store work without Chronicle knowing
anything about either.

Windows will not delete a running executable but it will rename one, so an
update moves the old binary aside and puts the new one in its place; the next
start sweeps up what is no longer locked. The running recorder is then *asked*
to stop through a flag in the database rather than killed, so the session it was
in the middle of ends properly instead of turning up tomorrow marked
"interrupted".

Releases are built by `.github/workflows/release.yml` on a `v*` tag. The tag has
to match the version in `Cargo.toml` or the workflow fails — a binary that
reports the wrong version would either offer itself as an upgrade forever or
never again.

## Which platforms CI covers, and why

Chronicle is a Windows application. Not "Windows first" — the observer is
`GetForegroundWindow` and `IShellWindows` and a PEB read; `local_path` produces
backslash paths; the restore engine launches `.exe` files. On any other
platform `win.rs` is a stub that returns `None`, so the program starts, records
nothing, and restores nothing.

That splits CI in two, because two different questions are being asked.

* **windows-latest** runs `cargo test --workspace` and
  `cargo clippy --all-targets -- -D warnings`. This is the product, so this is
  where the tests mean something.
* **ubuntu-latest and macos-latest** run `cargo clippy --workspace` — the
  crates, deliberately *not* `--all-targets`. The question they answer is "do
  the non-Windows stubs still compile?", which is the only thing a future macOS
  port has to start from and which rots silently otherwise.

Running the full suite on Linux was tried and is worse than useless: it fails on
`PathBuf::from(r"c:\work\proj")`, where a backslash is one component rather
than three, and on `local_path`, which is Windows path handling by design. A
green tick there would have meant a Linux Chronicle works. There is no Linux
Chronicle.

Releases are **Windows only** for the same reason. Publishing a macOS binary of
a workspace recorder that cannot see a workspace is shipping a promise the
binary does not keep. The earlier workflow built `.dmg`, `.deb` and `.AppImage`
artifacts and — separately — never published `chronicled.exe` at all, so the
recorder, the part that actually runs in the background, could not be
downloaded. If macOS artifacts are wanted for development rather than for
users, the matrix is three lines; the reason they are gone is honesty about
what they do, not build time.

## Sources lists this machine, not the catalogue

Settings → Sources used to list everything Chronicle recognises — every
browser, every JetBrains IDE, every password manager — whether or not any of it
was installed. That is a list of Chronicle's opinions rather than of the user's
computer, and the one app they came to switch off was somewhere past Krita.
Worse, an installed app the catalogue had never heard of could not be
configured at all until Chronicle happened to record it, which is backwards for
a privacy control.

The list now comes from the machine:

* `App Paths` in the registry, which maps an executable name straight to its
  path — the same identity Chronicle files sessions under.
* The uninstall keys, which is what "Add or remove programs" reads.
* Chronicle's own `apps` table, which covers Store apps: `WindowsApps` is not
  readable, but an application the recorder has watched running is installed by
  definition.

None of it is stored. The scan runs when the panel opens, builds the list, and
is dropped — Chronicle keeps no inventory of installed software, and this never
touches the network.

The uninstall list is not a list of applications; it is a list of everything
that knows how to remove itself, so most of it is redistributables, drivers and
language packs that have no window and can never appear in a session. Those are
filtered by name, which is a heuristic and will occasionally be wrong in both
directions — hence **Show all known apps** in the panel, and
`chronicled sources --all`. On the machine this was written on the list went
from about 200 rows to 60.

Applications that are not installed still appear, at the bottom, when a shipped
rule denies them: "are password managers excluded?" deserves an answer that
does not depend on having one installed.

One row can cover several executables. Windows Terminal ships as both
`windowsterminal.exe` and `wt.exe`; showing two rows makes one app look like
two, and showing one row while writing the setting to one executable would
leave the other quietly recording. The row carries its aliases and the switch
writes to all of them.

## Known gaps

- **A terminal's tabs are not split by window.** Windows Terminal runs every
  window in one process, so the process tree cannot say which window a given
  shell belongs to — the same way Explorer needed a window handle to tell two
  folder views apart, and unlike Explorer there is no `IShellWindows` to ask.
  Two terminal windows of three tabs restore as one window of six. The
  directories are right; the window split is not.
- **The window cannot update Chronicle**, only the CLI can. Same reason as
  autostart: the code lives in the recorder's binary, which the Tauri app
  cannot import.
- **Some entries are named by their executable.** An application that
  registered an app path but no display name arrives as `mspaint` or `wsl`
  rather than "Paint" or "Windows Subsystem for Linux". The real name is in the
  binary's version resource, which nothing reads yet.
- **A web view host is recorded under its own name.** `msedgewebview2.exe`
  hosts whatever asked for it, so an app that ships as a web view arrives
  looking like a generic Microsoft component with the hosted app's title. The
  deny list keys on executables, and this one executable is several apps.
- **A 32-bit shell reports no directory.** The offsets used to read
  `RTL_USER_PROCESS_PARAMETERS` are the x64 ones, and a WOW64 process is
  declined rather than read with the wrong layout — a plausible wrong path is
  worse than none.
- **Browser tabs are behind by however long the browser has been open.** A
  running Chromium holds its current session file with an exclusive lock, so
  Chronicle reads the newest file it *can* open — the previous session. The UI
  says how old the data is. Live tabs need either UI Automation on the address
  bar (focused tab only) or the extension that was ruled out.
- **Browser tab groups and pinning are partly lost on restore.** Pinned state
  is read, but `--new-window` cannot replay it, and tab groups are not in the
  session file at all.
- **Undo Restore does not exist.** Spawned process ids are captured in the
  receipt, but nothing closes them yet.
- **The window cannot start or stop the recorder**, and cannot toggle
  autostart. It only reports whether a recorder is running. Autostart lives in
  the daemon binary, so sharing it with the Tauri app means lifting it into a
  crate both can depend on. A tray icon and a pause control are the next piece.
- **Observations that never form a session are never cleaned up.** The
  sessioniser only looks back 24 hours, so an observation that failed the
  minimum-duration or minimum-artifact rule is re-scanned every minute until it
  ages out of that window, then stays in the table unreferenced until retention
  removes it.
- **Session titles are not editable from the sidebar**, only from the detail
  pane with `F2`.

## Privacy posture, as implemented

Already enforced in code, not just in the design document:

- Password managers, credential prompts and the sign-in UI are on a permanent
  deny list that **no user setting can override** (`policy.rs`).
- Messaging and mail apps are off by default and must be switched on explicitly.
- Applications whose name suggests finance or health are denied automatically.
- The desktop, the taskbar, the Start menu and Alt+Tab are not recorded at all.
  They are `explorer.exe` like every folder window, so the filter is the window
  class (`CabinetWClass`), which Windows does not localise — a title match would
  miss on a non-English install.
- Window titles are matched against secret patterns and dropped before writing.
- URLs lose credentials, OAuth codes and tokens; a URL that cannot be made safe
  is not recorded at all.
- `forget_range` is a hard delete followed by `VACUUM` — no tombstones.
- No network code exists anywhere in the workspace.

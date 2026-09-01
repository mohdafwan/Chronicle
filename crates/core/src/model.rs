//! The vocabulary of Chronicle: what an observation is, what an artifact is,
//! and what a session is once the sessioniser has had its say.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type SessionId = i64;
pub type ArtifactId = i64;

/// What kind of thing a user was looking at. Determines the restore adapter
/// and the colour of the tile in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A bare application with no resolvable document.
    App,
    /// A code project or repository root.
    Project,
    /// A single file open in an editor or viewer.
    File,
    /// A folder open in Explorer / Finder.
    Directory,
    /// A web page.
    Url,
    /// A PDF or office document.
    Document,
    /// A design file (Figma, Sketch, PSD).
    Design,
    /// A shell working directory.
    Terminal,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Project => "project",
            Self::File => "file",
            Self::Directory => "directory",
            Self::Url => "url",
            Self::Document => "document",
            Self::Design => "design",
            Self::Terminal => "terminal",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "project" => Self::Project,
            "file" => Self::File,
            "directory" => Self::Directory,
            "url" => Self::Url,
            "document" => Self::Document,
            "design" => Self::Design,
            "terminal" => Self::Terminal,
            _ => Self::App,
        }
    }
}

/// The grouping shown in the session detail pane, and the tile colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Editor,
    Browser,
    Design,
    Terminal,
    Document,
    Folder,
    Comms,
    Other,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Browser => "browser",
            Self::Design => "design",
            Self::Terminal => "terminal",
            Self::Document => "document",
            Self::Folder => "folder",
            Self::Comms => "comms",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "editor" => Self::Editor,
            "browser" => Self::Browser,
            "design" => Self::Design,
            "terminal" => Self::Terminal,
            "document" => Self::Document,
            "folder" => Self::Folder,
            "comms" => Self::Comms,
            _ => Self::Other,
        }
    }

    /// Display label for the group header in the session detail pane.
    pub fn label(self) -> &'static str {
        match self {
            Self::Editor => "Editors",
            Self::Browser => "Browser",
            Self::Design => "Design",
            Self::Terminal => "Terminal",
            Self::Document => "Documents",
            Self::Folder => "Folders",
            Self::Comms => "Communication",
            Self::Other => "Other",
        }
    }

    /// Launch priority during restore. Editors and terminals are slowest to
    /// become ready so they go first; browsers steal focus so they go last.
    pub fn launch_order(self) -> u8 {
        match self {
            Self::Editor => 0,
            Self::Terminal => 1,
            Self::Design => 2,
            Self::Document => 3,
            Self::Folder => 4,
            Self::Other => 5,
            Self::Comms => 6,
            Self::Browser => 7,
        }
    }
}

/// Screen geometry of a window, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One resolved thing a user had in front of them, as seen by an enricher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactObs {
    pub kind: ArtifactKind,
    /// Canonical identity. `file:///c:/work/app`, `https://…`, `figma://file/KEY`.
    /// Two observations with the same uri are the same artifact, forever.
    pub uri: String,
    pub display_name: String,
    /// Repository or project root, when one could be resolved. This is the
    /// single strongest signal the sessioniser has.
    pub project_root: Option<String>,
    /// Restorable detail: branch, page number, line, cwd, node-id.
    pub state: Vec<(String, String)>,
}

impl ArtifactObs {
    pub fn new(kind: ArtifactKind, uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind,
            uri: uri.into(),
            display_name: name.into(),
            project_root: None,
            state: Vec::new(),
        }
    }

    pub fn with_root(mut self, root: impl Into<String>) -> Self {
        self.project_root = Some(root.into());
        self
    }

    pub fn with_state(mut self, k: &str, v: impl Into<String>) -> Self {
        self.state.push((k.to_string(), v.into()));
        self
    }
}

/// A single sample from the observer: what was focused at a point in time.
/// Titles and URLs here have already been through redaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub at: DateTime<Utc>,
    /// Stable app identity. On Windows this is the lowercased exe name.
    pub app_id: String,
    pub app_name: String,
    /// Full path to the executable, as observed. Restore relaunches exactly
    /// this binary rather than searching PATH and hoping.
    pub exe_path: Option<String>,
    pub category: Category,
    pub title: String,
    pub pid: u32,
    pub frame: Option<Frame>,
    pub display_id: Option<String>,
    /// Everything the enrichers could resolve from this window.
    pub artifacts: Vec<ArtifactObs>,
}

/// Why a session ended. Drives the "Interrupted" badge and the login prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// Twelve quiet minutes.
    Idle,
    /// Workstation locked or the machine slept.
    Locked,
    /// The focused window set turned over.
    ContextSwitch,
    /// No clean shutdown marker was written. This is the one that matters.
    Interrupted,
    /// Still going.
    Open,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Locked => "locked",
            Self::ContextSwitch => "context_switch",
            Self::Interrupted => "interrupted",
            Self::Open => "open",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "idle" => Self::Idle,
            "locked" => Self::Locked,
            "context_switch" => Self::ContextSwitch,
            "interrupted" => Self::Interrupted,
            _ => Self::Open,
        }
    }
}

/// Where a session title came from. Recorded so that automatic re-titling
/// never overwrites a name the user chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    ProjectRoot,
    Branch,
    DesignFile,
    Domain,
    Fallback,
    User,
}

impl TitleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectRoot => "project_root",
            Self::Branch => "branch",
            Self::DesignFile => "design_file",
            Self::Domain => "domain",
            Self::Fallback => "fallback",
            Self::User => "user",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "project_root" => Self::ProjectRoot,
            "branch" => Self::Branch,
            "design_file" => Self::DesignFile,
            "domain" => Self::Domain,
            "user" => Self::User,
            _ => Self::Fallback,
        }
    }
}

/// A session as stored. The unit the user sees, searches, pins and restores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub title_source: TitleSource,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Time with a focused window, excluding the gaps. Always <= wall time.
    pub active_seconds: i64,
    pub end_reason: EndReason,
    pub pinned: bool,
    pub device_id: String,
}

/// An artifact as it appeared inside one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionArtifact {
    pub artifact_id: ArtifactId,
    pub kind: ArtifactKind,
    pub category: Category,
    pub uri: String,
    pub display_name: String,
    pub app_id: String,
    pub app_name: String,
    pub app_exe: Option<String>,
    pub project_root: Option<String>,
    pub focus_seconds: i64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub state: Vec<(String, String)>,
}

impl SessionArtifact {
    /// Look up one piece of restorable state.
    pub fn state_of(&self, key: &str) -> Option<&str> {
        self.state
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A session plus everything attached to it, ready for the detail pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session: Session,
    pub artifacts: Vec<SessionArtifact>,
}

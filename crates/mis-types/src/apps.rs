//! Apps the orchestrator hosts: static bundles kept as numbered releases.

use serde::{Deserialize, Serialize};

/// Most characters in an app's title.
pub const MAX_APP_TITLE_CHARS: usize = 80;

/// What [`valid_slug`] wants, for error messages.
pub const SLUG_RULE: &str =
    "an app's name is a-z, 0-9 and -, up to 32, starting with a letter or digit";

/// An app's name and path segment: `[a-z0-9][a-z0-9-]{0,31}`, e.g. `pool`, `chores-2`.
pub fn valid_slug(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=32).contains(&s.len()) && s.starts_with(ok) && s.chars().all(|c| ok(c) || c == '-')
}

/// `POST /api/apps/tasks`: a task that builds a new app, or changes one. It runs in the
/// settings' apps repo with the apps guide (`guides/apps.md`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppTask {
    /// What the user wants, in their words.
    pub request: String,
    /// The app; a new one's may be left to the agent.
    #[serde(default)]
    pub slug: Option<String>,
    /// Change app `slug` rather than build one.
    #[serde(default)]
    pub change: bool,
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    /// Only this worker runs it; None: any running `harness`.
    #[serde(default)]
    pub worker: Option<uuid::Uuid>,
}

/// An app as `GET /api/apps` lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppView {
    /// `[a-z0-9][a-z0-9-]{0,31}`; its path is `/a/<slug>/`.
    pub slug: String,
    /// From the live release's `app.json`, else the newest one's.
    pub title: String,
    /// Where it is served, e.g. `https://home.example.com/a/pool/`. None: nowhere.
    pub url: Option<String>,
    /// The live release. None: published, never activated.
    pub active: Option<u32>,
    /// Oldest first.
    pub releases: Vec<AppRelease>,
    /// Who it is shared with, by email. Direct loopback requests are its owner too.
    #[serde(default)]
    pub members: Vec<AppMember>,
    /// The staged release an agent proposes, to try and then publish or discard.
    #[serde(default)]
    pub draft: Option<AppDraft>,
    /// The asker's role in it: only an owner publishes or discards its draft. None: not a member.
    #[serde(default)]
    pub role: Option<AppRole>,
    /// Feedback waits for the owner to send it to an agent, rather than going at once.
    #[serde(default)]
    pub hold_feedback: bool,
    /// Feedback waiting, oldest first.
    #[serde(default)]
    pub feedback: Vec<HeldFeedback>,
}

/// `POST /a/{slug}/api/v1/feedback`: an owner's or editor's note on what they see, from the
/// app's feedback button. No screenshot: the agent reproduces the view with Playwright.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppFeedback {
    /// Where in the app: its `#hash` route, else the path after `/a/<slug>/`.
    pub route: String,
    /// A CSS selector for the element marked (the one tapped, or under the circle's middle).
    #[serde(default)]
    pub selector: String,
    /// What was marked, in CSS pixels of the page (scroll included).
    pub rect: FeedbackRect,
    pub viewport: FeedbackSize,
    pub note: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct FeedbackRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct FeedbackSize {
    pub w: u32,
    pub h: u32,
}

/// Feedback as the orchestrator has it: who sent it, about which release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeldFeedback {
    pub id: uuid::Uuid,
    /// Unix millis it came in.
    pub at: u64,
    /// Their email, or `local`.
    pub by: String,
    /// The release they saw; None: none was live.
    pub release: Option<u32>,
    /// They saw the draft (`release` is it).
    pub draft: bool,
    /// The app had a draft then: the task that proposed it (None: from no task), whose
    /// thread the feedback continues rather than starting work the draft would replace.
    #[serde(default)]
    pub draft_task: Option<Option<uuid::Uuid>>,
    pub feedback: AppFeedback,
    /// Why it wasn't sent to an agent when it came in (held, or the agent was busy).
    #[serde(default)]
    pub waiting: Option<String>,
    /// The task it went to. None: it waits.
    #[serde(default)]
    pub task: Option<uuid::Uuid>,
}

/// What became of feedback someone sent, as their app's menu shows it (`/me`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeedbackStatus {
    pub id: uuid::Uuid,
    /// Unix millis they sent it.
    pub at: u64,
    pub note: String,
    #[serde(flatten)]
    pub state: FeedbackState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FeedbackState {
    /// Not with an agent yet: held for the owner, or its work was busy.
    Waiting { why: String },
    /// An agent is on it.
    Working,
    /// A draft came after it: try it.
    DraftReady { release: u32 },
}

/// Longest feedback note.
pub const MAX_FEEDBACK_CHARS: usize = 2000;
/// Most feedback an app keeps waiting; more is refused (429) until some is sent or dismissed.
pub const MAX_HELD_FEEDBACK: usize = 50;
/// Most open tasks feedback starts for one app; past it, feedback waits for the owner.
pub const MAX_FEEDBACK_TASKS: usize = 3;

/// An app's draft: a staged release and the PR it was built from (`mis apps propose`). One
/// per app; a newer proposal replaces it. Publishing merges the PR, then makes it live.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppDraft {
    /// This proposal: a decision names it, so it never lands on one proposed since.
    pub id: uuid::Uuid,
    pub release: u32,
    /// `https://github.com/<owner>/<repo>/pull/<n>`.
    pub pr: String,
    /// The commit the release was built from: the PR merges only while that is its head.
    pub head: String,
    #[serde(default)]
    pub note: Option<String>,
    /// Screenshots kept with it (`GET /api/apps/{slug}/draft/before|after`).
    #[serde(default)]
    pub before: bool,
    #[serde(default)]
    pub after: bool,
    /// The task that proposed it, whose thread a change request replies in.
    #[serde(default)]
    pub task: Option<uuid::Uuid>,
    /// Unix millis it was proposed.
    pub at: u64,
    /// Why the last publish didn't happen, e.g. the PR couldn't merge.
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /api/apps/{slug}/draft`: propose release `release` as the app's draft.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposeDraft {
    pub release: u32,
    pub pr: String,
    /// The full sha of the commit the release was built from.
    pub head: String,
    #[serde(default)]
    pub note: Option<String>,
    /// Base64 PNG, JPEG or GIF, up to [`MAX_DRAFT_IMAGE_BYTES`].
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
    /// With the full token, the task it belongs to; an app task's token names its own.
    #[serde(default)]
    pub task: Option<uuid::Uuid>,
}

/// Largest before or after screenshot on a draft.
pub const MAX_DRAFT_IMAGE_BYTES: usize = 2 << 20;
/// Most characters in a draft's note.
pub const MAX_DRAFT_NOTE_CHARS: usize = 2000;

/// What a member may do with an app: viewers read, editors also write, owners also admin.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppRole {
    Owner,
    Editor,
    Viewer,
}

impl AppRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppMember {
    /// Lower-case.
    pub email: String,
    pub role: AppRole,
}

/// `POST /api/apps/{slug}/members`: give `email` `role`; `role: null` removes them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetMember {
    pub email: String,
    pub role: Option<AppRole>,
}

/// `GET /a/{slug}/api/v1/me` on the apps listener: who is asking and their role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppMe {
    /// Their email, or `local` for a direct loopback request.
    pub user: String,
    pub role: AppRole,
    /// The app's slug.
    pub app: String,
    /// The draft release they are trying (on a copy of the records). None: the live app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<u32>,
    /// The app's current draft, which owners and editors may try. None: none, or a viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drafted: Option<u32>,
    /// Their latest feedback on the app and what became of it, newest first (owners, editors).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<FeedbackStatus>,
}

/// One published bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppRelease {
    pub n: u32,
    /// Unix millis it was published.
    pub at: u64,
    /// Hex SHA-256 of its files (paths and contents, in path order).
    pub sha256: String,
    /// Bytes of its files.
    pub size: u64,
}

/// One snapshot of an app's records, as `GET /api/apps/{slug}/backups` lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppBackup {
    /// E.g. `daily-20261108T031500.000Z`, `pre-v3-…`: what `restore` takes.
    pub name: String,
    pub kind: BackupKind,
    /// Unix millis it was taken.
    pub at: u64,
    /// Bytes of its database.
    pub size: u64,
}

/// Why a snapshot was taken; each kind keeps its own newest few.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    Daily,
    PreActivation,
    PreRestore,
    Manual,
}

impl BackupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::PreActivation => "pre-activation",
            Self::PreRestore => "pre-restore",
            Self::Manual => "manual",
        }
    }
}

/// `POST /api/apps/{slug}/restore`: replace the app's records with snapshot `backup`, and its
/// members with that moment's too if `with_policy`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestoreBackup {
    pub backup: String,
    #[serde(default)]
    pub with_policy: bool,
}

/// What a restore replies: the snapshot taken just before it (restore it to undo). None: the
/// app had no records yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Restored {
    pub pre_restore: Option<AppBackup>,
}

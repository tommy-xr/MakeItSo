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
}

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

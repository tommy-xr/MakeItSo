//! The notes repo: publishing captures to it (`GET /api/notes/sync`) and reading it (`GET
//! /api/notes/tree`, `GET /api/projects`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    /// No notes remote configured.
    Off,
    /// Every capture is in the repo.
    Idle,
    /// Captures wait for the next run, or the last run failed locally (see `last_error`).
    Pending,
    /// The remote couldn't be reached; retried next run.
    Offline,
    /// Rebasing onto the remote conflicted; a human resolves it in the clone.
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotesSync {
    pub state: SyncState,
    /// Captures not yet in the repo.
    pub pending: usize,
    /// Unix ms of the last push since the orchestrator started.
    pub last_push: Option<u64>,
    pub last_error: Option<String>,
}

impl NotesSync {
    pub const OFF: Self = Self {
        state: SyncState::Off,
        pending: 0,
        last_push: None,
        last_error: None,
    };
}

/// What a path in the notes repo is, as the Notes view shows it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    Dir,
    Md,
    /// Named like a JPEG, PNG, WebP or GIF; served only if its bytes are one.
    Image,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteEntry {
    /// Repo-relative, `/`-separated, e.g. `inbox/2026-11-08-foo.md`.
    pub path: String,
    pub kind: NoteKind,
    /// Bytes; 0 for a dir.
    pub size: u64,
}

/// `GET /api/notes/tree`: the clone's shown folders, every entry under them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotesTree {
    /// `off`: no notes remote configured.
    pub state: SyncState,
    pub entries: Vec<NoteEntry>,
}

/// A project record in the notes repo: `projects/<id>/README.md`, or `archive/<id>/README.md`
/// once archived.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    /// Its folder's name, e.g. `make-it-so`.
    pub id: String,
    pub title: String,
    /// Other names it goes by, e.g. `mis`; no two projects share a name, ignoring case.
    pub aliases: Vec<String>,
    /// As written, e.g. `active`.
    pub status: String,
    /// GitHub `owner/repo`s.
    pub repos: Vec<String>,
    /// Paths on the orchestrator's machine, as written; not served.
    pub notes: Vec<String>,
    /// Under `archive/`.
    pub archived: bool,
    /// The README's Markdown after its frontmatter.
    pub description: String,
}

/// A record that isn't one, or a name it can't have, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectError {
    /// The record, e.g. `projects/x/README.md`.
    pub path: String,
    pub error: String,
}

/// `GET /api/projects`: the notes clone's project records, those under `projects/` first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Projects {
    /// `off`: no notes remote configured.
    pub state: SyncState,
    pub projects: Vec<Project>,
    pub errors: Vec<ProjectError>,
}

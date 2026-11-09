//! The notes repo: publishing captures to it (`GET /api/notes/sync`) and reading it (`GET
//! /api/notes/tree`).

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

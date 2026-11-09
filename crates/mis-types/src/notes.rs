//! Publishing captures to the notes repo, as `GET /api/notes/sync` reports it.

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

//! Reviews: a PR waiting on the owner. Approve means "merge when it can": the orchestrator
//! waits for checks, asks the task that made it to fix failures, and merges at the reviewed
//! head through the worker's code-checked path; then the kind's step runs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::TaskId;

/// Most times a failing or conflicted PR goes back to its task before it needs the owner.
pub const MAX_FIX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Review {
    pub id: Uuid,
    pub pr: PrRef,
    /// The commit under review: approving merges only this.
    pub head: String,
    /// The task that made it, whose thread fixes go to.
    pub task: Option<TaskId>,
    pub note: Option<String>,
    pub kind: ReviewKind,
    pub state: ReviewState,
    /// Unix millis it was opened.
    pub at: u64,
    /// The latest word on it, e.g. `checks running`, `asked its task to fix checks (1/3)`.
    #[serde(default)]
    pub status: Option<String>,
    /// The head its task was last asked to fix: the same failure there waits for that fix.
    #[serde(default)]
    pub fixing: Option<String>,
    /// A checkout of its repo on the worker (`~/code/mis-notes`): the PR must be its origin's.
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrRef {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub url: String,
}

/// What a merged review goes on to do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewKind {
    /// Code: nothing after the merge.
    Code,
    /// An app's draft: release `release` of `slug` goes live once merged.
    AppDraft { slug: String, release: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewState {
    /// Waiting for the owner.
    Open,
    /// Approved: merging when it can. `fixes`: times it went back to its task.
    Approved {
        fixes: u32,
    },
    /// Its task pushed a fix after approval: the new head waits for the owner's nod.
    FixReady {
        approved: String,
        fixes: u32,
    },
    /// It couldn't merge and its task can't (or may no longer) fix it.
    NeedsYou {
        why: String,
    },
    Merged,
    Rejected,
}

impl ReviewState {
    /// Merged or rejected: it never changes again.
    pub fn finished(&self) -> bool {
        matches!(self, Self::Merged | Self::Rejected)
    }
}

/// What a worker found when it looked at (and, if it could, merged) a review's PR.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Assessment {
    Merged,
    /// Not yet: checks running, GitHub still working out mergeability.
    Waiting {
        why: String,
    },
    /// Checks failed: their names, and the tail of the first one's log.
    Failing {
        checks: Vec<String>,
        log: String,
    },
    /// It conflicts with its base.
    Conflict,
    /// Its head isn't the reviewed one any more.
    Moved {
        head: String,
    },
    /// Closed without merging, not in the apps repo, or gh couldn't read it.
    Refused {
        why: String,
    },
}

//! Questions the orchestrator asks one worker on demand (`OrchMsg::Query`), and
//! their answers (`WorkerMsg::Answer`): Claude and Codex sessions on its disk.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Query {
    /// The newest sessions, answered with `Answer::Sessions`; headless ones only if asked.
    ListSessions {
        #[serde(default)]
        headless: bool,
    },
    /// The last `limit` turns of one session, answered with `Answer::Transcript`
    /// or `Answer::NotFound`.
    ReadSession {
        harness: String,
        id: Uuid,
        limit: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Sessions {
        sessions: Vec<Session>,
    },
    Transcript {
        /// Oldest first.
        turns: Vec<Turn>,
        /// Older turns exist than those sent.
        more: bool,
        /// As in `Session`, so a viewer knows whether to keep refreshing.
        updated_ms: u64,
        active: bool,
    },
    NotFound,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// A tool call (`Bash: ls`) or its result (`→ first line`), one line each.
    Tool,
}

/// Most turns `Query::ReadSession` returns.
pub const MAX_TURNS: usize = 200;

/// One entry of a session's transcript, clipped for a phone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// When it was written, as the harness recorded it (RFC 3339).
    pub at: Option<String>,
}

/// A Claude or Codex session found in the harness's own transcript files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Session {
    /// "claude" or "codex".
    pub harness: String,
    /// What `claude --resume` / `codex exec resume` take.
    pub id: String,
    pub cwd: Option<String>,
    /// Its title if it has one, else the first thing the user asked; one line.
    pub title: Option<String>,
    pub model: Option<String>,
    /// Unix millis of its last write.
    pub updated_ms: u64,
    /// Written in the last couple of minutes: likely still running.
    pub active: bool,
    /// Started from a terminal UI, not headless (`claude -p`, `codex exec`).
    pub interactive: bool,
}

//! Questions the orchestrator asks one worker on demand (`OrchMsg::Query`), and
//! their answers (`WorkerMsg::Answer`): Claude and Codex sessions on its disk.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Query {
    /// The newest sessions, answered with `Answer::Sessions`.
    ListSessions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Sessions { sessions: Vec<Session> },
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

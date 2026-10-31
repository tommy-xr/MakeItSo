//! What a worker lets its agents do without asking: worker.toml `[permissions]`,
//! reported in `WorkerInfo` so the Workers view can show it; and the approvals
//! asked of the user for anything else in `ask` mode.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Write only in the task's cwd and `folders`; use only the listed MCP
    /// servers and command prefixes. Anything else is denied.
    #[default]
    Folders,
    /// No limits (`--dangerously-skip-permissions` and codex's bypass).
    Full,
    /// Folders, and claude asks the user (via the page) for anything else
    /// instead of being denied. Codex has no hook for this: it runs as folders.
    Ask,
}

/// Longest `Approval::detail`.
pub const MAX_APPROVAL_DETAIL: usize = 4000;

/// A tool call the agent wants that the rules don't allow, waiting for the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    pub id: Uuid,
    /// The agent's tool, e.g. "Write", "Bash", "mcp__blender__execute_blender_code".
    pub tool: String,
    /// Short and human-readable, e.g. "Write ~/assets/crate.py", "Bash: python3 make.py".
    pub summary: String,
    /// The whole call, for a closer look: a command as is, else the input as
    /// JSON; clipped to `MAX_APPROVAL_DETAIL` chars, saying so.
    #[serde(default)]
    pub detail: String,
    /// What "always" would save, e.g. "writes in ~/assets"; None when it can't.
    #[serde(default)]
    pub always: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    /// Allow, and save the rule `Approval::always` names in worker.toml.
    Always,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Permissions {
    pub mode: PermissionMode,
    /// Extra writable directories, absolute and canonical once resolved.
    pub folders: Vec<String>,
    /// MCP servers whose tools are allowed, by name, e.g. "blender".
    pub mcp: Vec<String>,
    /// Shell command prefixes allowed, e.g. "python3", "git log".
    pub commands: Vec<String>,
}

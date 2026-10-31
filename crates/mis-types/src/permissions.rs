//! What a worker lets its agents do without asking: worker.toml `[permissions]`,
//! reported in `WorkerInfo` so the Workers view can show it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Write only in the task's cwd and `folders`; use only the listed MCP
    /// servers and command prefixes. Anything else is denied.
    #[default]
    Folders,
    /// No limits (`--dangerously-skip-permissions` and codex's bypass).
    Full,
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

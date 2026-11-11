//! The core's builds: what `mis-up --watch` has built and runs (`Builds`, its report), and what
//! the owner sees and decides from it (`CoreUpdate`). A new build waits for the owner's Update.

use serde::{Deserialize, Serialize};

use crate::Checks;

/// How the running build started.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Started {
    /// When `mis-up` started.
    #[default]
    Launch,
    /// The owner's Update (or `--auto-update`).
    Update,
    /// The owner's Roll back.
    RollBack,
}

/// One commit between two builds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    /// Its subject; shown without a trailing `(#N)`, which becomes `pr`.
    pub subject: String,
    #[serde(default)]
    pub pr: Option<u64>,
    /// Main's CI run on it, if the watcher could ask.
    #[serde(default)]
    pub ci: Option<Checks>,
}

/// `mis-up --watch`'s report, sent every few seconds to `POST /api/update/builds`.
/// Shas are full; commits newest first.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Builds {
    pub running: String,
    #[serde(default)]
    pub started: Started,
    /// Built from main and not started: it waits for the owner.
    #[serde(default)]
    pub ready: Option<String>,
    /// The build that ran before this one, kept for Roll back.
    #[serde(default)]
    pub previous: Option<String>,
    /// `running..ready`: what Update brings.
    #[serde(default)]
    pub incoming: Vec<Commit>,
    /// `previous..running`: what Roll back takes out.
    #[serde(default)]
    pub outgoing: Vec<Commit>,
    /// The checkout's origin URL, for PR links.
    #[serde(default)]
    pub origin: String,
    /// `mis-up --auto-update`: a ready build starts once no task runs, without the owner.
    #[serde(default)]
    pub auto: bool,
}

/// A build the owner may start, and the commits that start changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    pub sha: String,
    pub commits: Vec<Commit>,
}

/// What the page shows about the core's builds (`ServerMsg::Status`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreUpdate {
    pub running: String,
    pub started: Started,
    /// `owner/name` on GitHub, for PR links.
    #[serde(default)]
    pub repo: Option<String>,
    /// Update starts it.
    #[serde(default)]
    pub ready: Option<Change>,
    /// Roll back starts it.
    #[serde(default)]
    pub back: Option<Change>,
    /// The build the owner chose; it starts once no task runs.
    #[serde(default)]
    pub asked: Option<String>,
    /// The owner kept the running build: its Updated card is done.
    #[serde(default)]
    pub kept: bool,
    #[serde(default)]
    pub auto: bool,
}

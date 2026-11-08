//! Apps the orchestrator hosts: static bundles kept as numbered releases.

use serde::{Deserialize, Serialize};

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

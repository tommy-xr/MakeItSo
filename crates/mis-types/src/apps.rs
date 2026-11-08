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
    /// Who it is shared with, by email. Direct loopback requests are its owner too.
    #[serde(default)]
    pub members: Vec<AppMember>,
}

/// What a member may do with an app: viewers read, editors also write, owners also admin.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppRole {
    Owner,
    Editor,
    Viewer,
}

impl AppRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppMember {
    /// Lower-case.
    pub email: String,
    pub role: AppRole,
}

/// `POST /api/apps/{slug}/members`: give `email` `role`; `role: null` removes them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetMember {
    pub email: String,
    pub role: Option<AppRole>,
}

/// `GET /a/{slug}/api/v1/me` on the apps listener: who is asking and their role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppMe {
    /// Their email, or `local` for a direct loopback request.
    pub user: String,
    pub role: AppRole,
    /// The app's slug.
    pub app: String,
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

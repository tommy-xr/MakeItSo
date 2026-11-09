use serde::{Deserialize, Serialize};

/// The orchestrator's settings (`GET`/`POST /api/settings`, the Settings page). A missing key
/// takes its default, so a file saved by an older version still reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub apps: AppSettings,
}

/// How tasks that build or change apps run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub harness: String,
    /// A CLI alias (`opus`) follows the newest model.
    pub model: String,
    /// The apps repo's checkout on the worker: the tasks' folder. `~/` is the worker's home.
    pub repo: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            harness: "claude".into(),
            model: "opus".into(),
            repo: "~/code/mis-notes".into(),
        }
    }
}

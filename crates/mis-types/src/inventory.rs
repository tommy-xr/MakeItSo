//! What a worker machine has beyond its harnesses (MCP servers, apps, hardware),
//! so the planner can send e.g. 3D asset work to the machine with Blender.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Inventory {
    /// MCP server names per harness, e.g. `{"claude": ["blender"]}`. Names only:
    /// server configs often hold API keys, so commands, args, env and URLs never leave the machine.
    pub mcp: BTreeMap<String, Vec<String>>,
    /// Known apps and tools found on the machine, by name.
    pub apps: Vec<InstalledApp>,
    pub machine: Machine,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstalledApp {
    /// e.g. "Blender", "ffmpeg".
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Machine {
    /// e.g. "macOS 15.3".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    pub cpus: u32,
    pub mem_gb: f32,
    /// e.g. "Apple M2 Max".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
}

impl Inventory {
    /// One line for prompts and terminals, e.g. "apps: Blender 4.2.3, ffmpeg;
    /// mcp claude: blender; machine: macOS 15.3, 12 cpus, 32 GB, Apple M2".
    pub fn brief(&self) -> String {
        let mut parts = vec![];
        if !self.apps.is_empty() {
            let apps: Vec<String> = self
                .apps
                .iter()
                .map(|a| match &a.version {
                    Some(v) => format!("{} {v}", a.name),
                    None => a.name.clone(),
                })
                .collect();
            parts.push(format!("apps: {}", apps.join(", ")));
        }
        for (harness, names) in &self.mcp {
            parts.push(format!("mcp {harness}: {}", names.join(", ")));
        }
        let m = &self.machine;
        let mut spec: Vec<String> = m.os_version.iter().cloned().collect();
        if m.cpus > 0 {
            spec.push(format!("{} cpus", m.cpus));
        }
        if m.mem_gb > 0.0 {
            spec.push(format!("{} GB", m.mem_gb.round()));
        }
        spec.extend(m.gpu.iter().cloned());
        if !spec.is_empty() {
            parts.push(format!("machine: {}", spec.join(", ")));
        }
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_lists_what_is_known() {
        assert_eq!(Inventory::default().brief(), "");
        let inv = Inventory {
            mcp: [("claude".into(), vec!["blender".into(), "playwright".into()])].into(),
            apps: vec![
                InstalledApp {
                    name: "Blender".into(),
                    version: Some("4.2.3".into()),
                },
                InstalledApp {
                    name: "ffmpeg".into(),
                    version: None,
                },
            ],
            machine: Machine {
                os_version: Some("macOS 15.3".into()),
                cpus: 12,
                mem_gb: 34.4,
                gpu: Some("Apple M2 Max".into()),
            },
        };
        assert_eq!(
            inv.brief(),
            "apps: Blender 4.2.3, ffmpeg; mcp claude: blender, playwright; \
             machine: macOS 15.3, 12 cpus, 34 GB, Apple M2 Max"
        );
    }
}

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
    /// Apps, then MCP server names, in `self` but not in `old`: "new: Houdini, houdini-mcp".
    pub fn added_since(&self, old: &Inventory) -> Vec<String> {
        let apps = (self.apps.iter())
            .filter(|a| !old.apps.iter().any(|o| o.name == a.name))
            .map(|a| a.name.clone());
        let had: Vec<&String> = old.mcp.values().flatten().collect();
        let mut mcp: Vec<String> = (self.mcp.values().flatten())
            .filter(|n| !had.contains(n))
            .cloned()
            .collect();
        mcp.sort();
        mcp.dedup();
        apps.chain(mcp).collect()
    }

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
    fn added_since_names_new_apps_then_new_mcp_servers() {
        let app = |n: &str| InstalledApp {
            name: n.into(),
            version: None,
        };
        let old = Inventory {
            apps: vec![app("Blender")],
            mcp: [("claude".into(), vec!["blender".into()])].into(),
            ..Default::default()
        };
        let new = Inventory {
            apps: vec![app("Blender"), app("Houdini")],
            mcp: [
                (
                    "claude".into(),
                    vec!["blender".into(), "houdini-mcp".into()],
                ),
                ("codex".into(), vec!["blender".into(), "houdini-mcp".into()]),
            ]
            .into(),
            ..Default::default()
        };
        assert_eq!(new.added_since(&old), ["Houdini", "houdini-mcp"]);
        assert!(old.added_since(&new).is_empty());
    }

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

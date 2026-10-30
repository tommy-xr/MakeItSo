//! Capability inventory sent in Hello: MCP server names per harness, known
//! apps, machine specs. Parsers are pure; `collect` reads the files.
//!
//! MCP configs often hold API keys in env, args, headers or URLs, so parsers
//! return server names only, and parse errors are logged without their text.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mis_types::{InstalledApp, Inventory, Machine};
use serde_json::Value;

/// A version probe that takes longer is dropped.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// An app or tool worth telling the planner about.
struct Known {
    name: &'static str,
    /// Bundle name under /Applications or ~/Applications (macOS).
    mac_app: Option<&'static str>,
    /// Executable on PATH, and the args that print its version.
    bin: Option<(&'static str, &'static [&'static str])>,
}

const V: &[&str] = &["--version"];
const KNOWN: &[Known] = &[
    Known {
        name: "Blender",
        mac_app: Some("Blender.app"),
        bin: Some(("blender", V)),
    },
    Known {
        name: "Godot",
        mac_app: Some("Godot.app"),
        bin: Some(("godot", V)),
    },
    Known {
        name: "Unity Hub",
        mac_app: Some("Unity Hub.app"),
        bin: None,
    },
    Known {
        name: "Xcode",
        mac_app: Some("Xcode.app"),
        bin: None,
    },
    Known {
        name: "Docker",
        mac_app: Some("Docker.app"),
        bin: Some(("docker", V)),
    },
    Known {
        name: "ffmpeg",
        mac_app: None,
        bin: Some(("ffmpeg", &["-version"])),
    },
    Known {
        name: "node",
        mac_app: None,
        bin: Some(("node", V)),
    },
    Known {
        name: "python3",
        mac_app: None,
        bin: Some(("python3", V)),
    },
    Known {
        name: "cargo",
        mac_app: None,
        bin: Some(("cargo", V)),
    },
    Known {
        name: "gh",
        mac_app: None,
        bin: Some(("gh", V)),
    },
];

/// Keys of `v[key]` when it is an object, sorted.
fn keys(v: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = v
        .get(key)
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Claude's `.claude.json`: user servers, plus those of projects under a root.
pub fn claude_mcp(json: &str, roots: &[PathBuf]) -> Result<Vec<String>, ()> {
    let v: Value = serde_json::from_str(json).map_err(|_| ())?;
    let mut names = keys(&v, "mcpServers");
    if let Some(projects) = v.get("projects").and_then(Value::as_object) {
        for (dir, p) in projects {
            if roots.iter().any(|r| Path::new(dir).starts_with(r)) {
                names.extend(keys(p, "mcpServers"));
            }
        }
    }
    Ok(names)
}

/// A project's `.mcp.json`.
pub fn mcp_json(json: &str) -> Result<Vec<String>, ()> {
    let v: Value = serde_json::from_str(json).map_err(|_| ())?;
    Ok(keys(&v, "mcpServers"))
}

/// Codex `config.toml`: `[mcp_servers.<name>]` tables not set `enabled = false`.
pub fn codex_mcp(toml: &str) -> Result<Vec<String>, ()> {
    let t: toml::Table = toml.parse().map_err(|_| ())?;
    let Some(servers) = t.get("mcp_servers").and_then(|s| s.as_table()) else {
        return Ok(vec![]);
    };
    Ok(servers
        .iter()
        .filter(|(_, s)| s.get("enabled").and_then(|e| e.as_bool()) != Some(false))
        .map(|(name, _)| name.clone())
        .collect())
}

/// First version-looking token: "Python 3.12.1" -> "3.12.1", "v22.3.0" ->
/// "22.3.0", "4.3.stable.official" -> "4.3".
pub fn parse_version(out: &str) -> Option<String> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    line.split(|c: char| c.is_whitespace() || c == ',')
        .map(|w| w.trim_start_matches('v'))
        .map(|w| {
            let end = w
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(w.len());
            w[..end].trim_end_matches('.')
        })
        .find(|w| w.contains('.') && w.starts_with(|c: char| c.is_ascii_digit()))
        .map(String::from)
}

/// GPU names from `system_profiler SPDisplaysDataType` ("Chipset Model: ...")
/// or `nvidia-smi --query-gpu=name --format=csv,noheader` (one per line).
pub fn parse_gpu(out: &str) -> Option<String> {
    let chipsets: Vec<&str> = out
        .lines()
        .filter_map(|l| l.trim().strip_prefix("Chipset Model:"))
        .map(str::trim)
        .collect();
    let names = match chipsets.is_empty() {
        false => chipsets,
        true => out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect(),
    };
    (!names.is_empty()).then(|| names.join(", "))
}

/// Merge MCP names into sorted, deduped lists; harnesses with none are left out.
fn add_mcp(inv: &mut Inventory, harness: &str, names: Vec<String>) {
    if names.is_empty() {
        return;
    }
    let list = inv.mcp.entry(harness.into()).or_default();
    list.extend(names);
    list.sort();
    list.dedup();
}

/// Parse `path` with `parse`; missing is empty, unreadable is logged by path only.
fn read_names(path: &Path, parse: impl FnOnce(&str) -> Result<Vec<String>, ()>) -> Vec<String> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return vec![];
    };
    parse(&s).unwrap_or_else(|()| {
        tracing::warn!(
            "could not parse {}; skipping its MCP servers",
            path.display()
        );
        vec![]
    })
}

/// Which known apps are installed: in an app dir or on PATH. `version` reads
/// one from a found bundle path or executable path.
fn find_apps(
    app_dirs: &[PathBuf],
    path_dirs: &[PathBuf],
    version: impl Fn(Found) -> Option<String> + Sync,
) -> Vec<InstalledApp> {
    let found: Vec<(&Known, Found)> = KNOWN
        .iter()
        .filter_map(|k| {
            let bundle = k
                .mac_app
                .and_then(|a| app_dirs.iter().map(|d| d.join(a)).find(|p| p.is_dir()));
            let exe = k.bin.and_then(|(b, args)| {
                let file = format!("{b}{}", std::env::consts::EXE_SUFFIX);
                let p = path_dirs
                    .iter()
                    .map(|d| d.join(&file))
                    .find(|p| p.is_file())?;
                Some((p, args))
            });
            let f = match (bundle, exe) {
                (Some(b), _) => Found::Bundle(b),
                (None, Some((p, args))) => Found::Exe(p, args),
                (None, None) => return None,
            };
            Some((k, f))
        })
        .collect();
    // Probes run side by side so startup waits for the slowest, not the sum.
    std::thread::scope(|s| {
        let handles: Vec<_> = found
            .into_iter()
            .map(|(k, f)| (k, s.spawn(|| version(f))))
            .collect();
        handles
            .into_iter()
            .map(|(k, h)| InstalledApp {
                name: k.name.into(),
                version: h.join().ok().flatten(),
            })
            .collect()
    })
}

/// Where a known app was found.
#[derive(Debug, PartialEq)]
enum Found {
    Bundle(PathBuf),
    Exe(PathBuf, &'static [&'static str]),
}

/// Version via the bundle's Info.plist or the executable's version flag.
fn probe_version(f: Found) -> Option<String> {
    let out = match f {
        Found::Bundle(b) => {
            let plist = b.join("Contents/Info.plist");
            let mut cmd = Command::new("/usr/bin/plutil");
            cmd.args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
                .arg(plist);
            output_within(cmd)?
        }
        Found::Exe(p, args) => {
            let mut cmd = Command::new(p);
            cmd.args(args);
            output_within(cmd)?
        }
    };
    parse_version(&out)
}

/// stdout of `cmd` if it exits ok within `PROBE_TIMEOUT`; killed otherwise.
fn output_within(mut cmd: Command) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if start.elapsed() > PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut s = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut s).ok()?;
    Some(s)
}

fn machine() -> Machine {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    // Absolute: /usr/sbin is often missing from a service's PATH.
    let gpu = if cfg!(target_os = "macos") {
        let mut cmd = Command::new("/usr/sbin/system_profiler");
        cmd.arg("SPDisplaysDataType");
        output_within(cmd)
    } else {
        let mut cmd = Command::new("nvidia-smi");
        cmd.args(["--query-gpu=name", "--format=csv,noheader"]);
        output_within(cmd)
    };
    Machine {
        os_version: sysinfo::System::long_os_version(),
        cpus: std::thread::available_parallelism().map_or(0, |n| n.get() as u32),
        mem_gb: (sys.total_memory() as f64 / 1e9 * 10.0).round() as f32 / 10.0,
        gpu: gpu.as_deref().and_then(parse_gpu),
    }
}

/// Dirs that may hold a project `.mcp.json`: each root and its direct children.
fn project_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .flat_map(|r| {
            let kids = std::fs::read_dir(r).into_iter().flatten().flatten();
            std::iter::once(r.clone()).chain(kids.map(|e| e.path()).filter(|p| p.is_dir()))
        })
        .collect()
}

/// Read this machine's inventory. Blocks for up to `PROBE_TIMEOUT` on probes.
pub fn collect(roots: &[PathBuf]) -> Inventory {
    let home = std::env::home_dir().unwrap_or_default();
    let mut inv = Inventory::default();
    // Same lookups as the harnesses: CLAUDE_CONFIG_DIR, CODEX_HOME.
    let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map_or(home.clone(), PathBuf::from);
    let claude = read_names(&claude_dir.join(".claude.json"), |s| claude_mcp(s, roots));
    add_mcp(&mut inv, "claude", claude);
    for dir in project_dirs(roots) {
        add_mcp(
            &mut inv,
            "claude",
            read_names(&dir.join(".mcp.json"), mcp_json),
        );
    }
    let codex_dir = std::env::var_os("CODEX_HOME").map_or(home.join(".codex"), PathBuf::from);
    add_mcp(
        &mut inv,
        "codex",
        read_names(&codex_dir.join("config.toml"), codex_mcp),
    );

    let app_dirs = match cfg!(target_os = "macos") {
        true => vec!["/Applications".into(), home.join("Applications")],
        false => vec![],
    };
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let (apps, machine) = std::thread::scope(|s| {
        let m = s.spawn(machine);
        let apps = find_apps(&app_dirs, &path_dirs, probe_version);
        (apps, m.join().unwrap_or_default())
    });
    inv.apps = apps;
    inv.machine = machine;
    inv
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRETS: &[&str] = &[
        "sk-live-123",
        "--api-key",
        "secret.example",
        "Bearer",
        "npx",
    ];

    const CLAUDE_JSON: &str = r#"{
        "numStartups": 4,
        "mcpServers": {
            "blender": {"command": "uvx", "args": ["blender-mcp", "--api-key", "sk-live-123"],
                        "env": {"BLENDER_KEY": "sk-live-123"}}
        },
        "projects": {
            "/code/game": {"mcpServers": {
                "assets": {"type": "http", "url": "https://secret.example/mcp",
                           "headers": {"Authorization": "Bearer sk-live-123"}}}},
            "/elsewhere/app": {"mcpServers": {"private": {"command": "npx"}}},
            "/code/empty": {}
        }
    }"#;

    const CODEX_TOML: &str = r#"
        model = "gpt-5"
        [mcp_servers.houdini]
        command = "npx"
        args = ["houdini-mcp", "--api-key", "sk-live-123"]
        [mcp_servers.houdini.env]
        HOUDINI_TOKEN = "sk-live-123"
        [mcp_servers.off]
        command = "x"
        enabled = false
        [mcp_servers.remote]
        url = "https://secret.example/mcp"
        bearer_token_env_var = "TOKEN"
    "#;

    #[test]
    fn claude_names_user_and_rooted_project_servers_only() {
        let roots = [PathBuf::from("/code")];
        assert_eq!(
            claude_mcp(CLAUDE_JSON, &roots).unwrap(),
            ["blender", "assets"]
        );
        assert_eq!(claude_mcp(CLAUDE_JSON, &[]).unwrap(), ["blender"]);
        assert_eq!(claude_mcp("{}", &roots).unwrap(), Vec::<String>::new());
        assert!(claude_mcp("not json", &roots).is_err());
        let project = r#"{"mcpServers": {"b": {"env": {"K": "sk-live-123"}}, "a": {}}}"#;
        assert_eq!(mcp_json(project).unwrap(), ["a", "b"]);
    }

    #[test]
    fn codex_names_enabled_servers() {
        assert_eq!(codex_mcp(CODEX_TOML).unwrap(), ["houdini", "remote"]);
        assert_eq!(codex_mcp("model = \"x\"").unwrap(), Vec::<String>::new());
        assert!(codex_mcp("[[[").is_err());
    }

    /// The whole inventory, as sent and as saved, carries no command, args, env or URL.
    #[test]
    fn secrets_never_leave() {
        let mut inv = Inventory::default();
        add_mcp(
            &mut inv,
            "claude",
            claude_mcp(CLAUDE_JSON, &["/code".into()]).unwrap(),
        );
        add_mcp(&mut inv, "codex", codex_mcp(CODEX_TOML).unwrap());
        let json = serde_json::to_string(&inv).unwrap();
        let toml = toml::to_string(&inv).unwrap();
        for s in SECRETS {
            assert!(!json.contains(s) && !toml.contains(s), "{s} leaked: {json}");
        }
        assert_eq!(inv.mcp["claude"], ["assets", "blender"]);
        assert_eq!(inv.mcp["codex"], ["houdini", "remote"]);
    }

    #[test]
    fn versions_from_common_outputs() {
        let cases = [
            ("Python 3.12.1\n", Some("3.12.1")),
            ("v22.3.0\n", Some("22.3.0")),
            ("cargo 1.90.0 (840b83a10 2025-07-30)", Some("1.90.0")),
            (
                "gh version 2.60.1 (2024-10-16)\nhttps://github.com/cli",
                Some("2.60.1"),
            ),
            ("Docker version 27.3.1, build ce12230", Some("27.3.1")),
            ("ffmpeg version 7.1 Copyright (c) 2000-2024", Some("7.1")),
            ("4.3.stable.official.77dcf97d8\n", Some("4.3")),
            ("Blender 4.2.3 LTS\n\tbuild date: x", Some("4.2.3")),
            ("4.5\n", Some("4.5")),
            ("no version here", None),
            ("", None),
        ];
        for (out, want) in cases {
            assert_eq!(parse_version(out).as_deref(), want, "{out:?}");
        }
    }

    #[test]
    fn gpu_from_system_profiler_or_nvidia_smi() {
        let sp = "Graphics/Displays:\n\n    Apple M2 Max:\n\n      Chipset Model: Apple M2 Max\n      Type: GPU\n";
        assert_eq!(parse_gpu(sp).as_deref(), Some("Apple M2 Max"));
        let smi = "NVIDIA GeForce RTX 4090\nNVIDIA GeForce RTX 3060\n";
        assert_eq!(
            parse_gpu(smi).as_deref(),
            Some("NVIDIA GeForce RTX 4090, NVIDIA GeForce RTX 3060")
        );
        assert_eq!(parse_gpu("\n"), None);
    }

    #[test]
    fn apps_from_bundles_and_path() {
        let dir = std::env::temp_dir().join(format!("mis-inv-{}", uuid::Uuid::new_v4()));
        let (apps, bin) = (dir.join("Applications"), dir.join("bin"));
        std::fs::create_dir_all(apps.join("Blender.app")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for b in ["blender", "ffmpeg"] {
            std::fs::write(bin.join(b), "").unwrap();
        }
        let got = find_apps(
            std::slice::from_ref(&apps),
            std::slice::from_ref(&bin),
            |f| match f {
                Found::Bundle(p) => Some(p.file_name()?.to_string_lossy().into()),
                Found::Exe(_, args) => Some(args.join(" ")),
            },
        );
        let app = |n: &str, v: &str| InstalledApp {
            name: n.into(),
            version: Some(v.into()),
        };
        // The bundle wins over a same-named executable.
        assert_eq!(
            got,
            [app("Blender", "Blender.app"), app("ffmpeg", "-version")]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn slow_probe_is_dropped() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let t = Instant::now();
        assert_eq!(output_within(cmd), None);
        assert!(t.elapsed() < Duration::from_secs(4));
    }
}

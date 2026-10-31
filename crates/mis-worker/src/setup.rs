//! `mis-worker setup`: ask for connection settings, test them, save worker.toml.

use std::io::{BufRead, IsTerminal, Write};
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{self, http::Uri, Message};

use crate::config::{self, Config};
use crate::{inventory, permissions, roots};

/// Each flag answers (and skips) one question.
#[derive(clap::Args)]
pub struct SetupArgs {
    /// Orchestrator URL, e.g. wss://mis.example.com.
    #[arg(long)]
    url: Option<String>,
    /// Bearer token; empty for none.
    #[arg(long)]
    token: Option<String>,
    /// Cloudflare Access service-token id. The secret comes from
    /// CF_ACCESS_CLIENT_SECRET, or a hidden prompt; never a flag.
    #[arg(long)]
    cf_access_client_id: Option<String>,
    /// Not behind Cloudflare Access; skips that question.
    #[arg(long, conflicts_with = "cf_access_client_id")]
    no_cf_access: bool,
    /// Directory tasks may run in (repeatable, comma-separated ok). Default: the
    /// current directory.
    #[arg(long = "root")]
    roots: Vec<String>,
    /// What this machine is for, e.g. "Blender + blender-mcp; 3D modelling,
    /// glTF export". The planner routes matching tasks here. Empty clears it.
    #[arg(long)]
    purpose: Option<String>,
    /// What agents may do: "ask" (folders, and claude asks you on the page for
    /// anything else), "folders" (write only in the task's dir and --folder
    /// dirs; use only --mcp servers and --command prefixes; the rest is denied)
    /// or "full" (anything). Default: ask.
    #[arg(long, value_parser = MODES.map(|(n, _)| n))]
    permissions: Option<String>,
    /// Folder agents may write in (repeatable, comma-separated ok). Default: the roots.
    #[arg(long = "folder")]
    folders: Vec<String>,
    /// MCP server whose tools agents may use (repeatable, comma-separated ok;
    /// "" for none). Default: every server found.
    #[arg(long)]
    mcp: Vec<String>,
    /// Shell command prefix agents may run, e.g. python3 or "git log"
    /// (repeatable, comma-separated ok; "" for none). Default: none.
    #[arg(long = "command")]
    commands: Vec<String>,
}

pub async fn run(a: SetupArgs) -> Result<()> {
    let path = crate::state_dir()?.join("worker.toml");
    let saved = config::load(&path)?;
    let tty = std::io::stdin().is_terminal();
    let cwd = std::env::current_dir()?;

    let url = match a.url {
        Some(u) => u,
        None if tty => {
            let default = match saved.url.or_else(|| std::env::var("MIS_URL").ok()) {
                Some(u) => Some(u),
                None => probe().await,
            };
            ask("Orchestrator URL", default.as_deref())?
        }
        None => bail!("--url is required when stdin is not a terminal"),
    };
    let mut token = match a.token {
        Some(t) => Some(t).filter(|t| !t.is_empty()),
        None if tty => ask_token()?,
        None => None,
    };
    let cf_id = match a.cf_access_client_id {
        Some(id) => Some(id),
        None if tty
            && !a.no_cf_access
            && may_need_access(&url)
            && confirm("Behind Cloudflare Access? [y/N]: ")? =>
        {
            Some(ask("Access client id", None)?)
        }
        None => None,
    };
    let cf_secret = match (&cf_id, crate::access_secret_env()) {
        (None, _) => None,
        (Some(_), Some(s)) => Some(s),
        (Some(_), None) if tty => Some(rpassword::prompt_password(
            "Access client secret (hidden): ",
        )?),
        (Some(_), None) => None,
    };
    access_headers(cf_id.as_deref(), cf_secret.as_deref()).map_err(anyhow::Error::msg)?;
    let answer = match a.roots.is_empty() {
        false => a.roots.join(","),
        true if tty => line(&format!("Roots [{}]: ", cwd.display()))?,
        true => String::new(),
    };
    let mut roots = roots::split_roots(&answer);
    if roots.is_empty() {
        roots.push(cwd.clone());
    }
    let roots = roots::canonical_roots(&roots)?;

    // Locking the id means setup refuses to run beside a live worker, whose
    // ledger entry this test connection would otherwise replace.
    let (id, _lock) = crate::worker_id()?;
    let agents = mis_agent::available();
    let harnesses: Vec<String> = agents.iter().map(|a| a.name().to_string()).collect();
    let info = WorkerInfo {
        hostname: crate::hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        // Advertise no harnesses so the orchestrator assigns nothing to this probe.
        harnesses: vec![],
        roots: roots.iter().map(|r| r.display().to_string()).collect(),
        ..Default::default()
    };
    let mut tries = 1;
    let cfg = loop {
        let cfg = Config {
            url: Some(url.clone()),
            token: token.clone(),
            cf_access_client_id: cf_id.clone(),
            cf_access_client_secret: cf_secret.clone(),
            roots: roots.clone(),
            models: saved.models.clone(),
            ..Default::default()
        };
        let resolved = config::merge(cfg.clone(), Config::default(), cwd.clone());
        match test_connect(&resolved, id, &info).await {
            Ok(()) => break cfg,
            Err(e) if tty && tries < 3 && denied(&e) == Some(401) => {
                eprintln!("{e:#}; check the token");
                token = ask_token()?;
                tries += 1;
            }
            // Cloudflare Access answers 403 for a bad service token.
            Err(e) if denied(&e) == Some(403) => {
                return Err(e.context("check the Cloudflare Access credentials"))
            }
            Err(e) => return Err(e),
        }
    };
    println!(
        "connected to {url} as worker {id} ({}, {harnesses:?})",
        info.hostname
    );
    if harnesses.is_empty() {
        eprintln!("warning: no `claude` or `codex` on PATH; mis-worker needs one to run");
    }
    let models = crate::advertised(&agents, &saved.models);
    for h in &harnesses {
        let from = if saved.models.contains_key(h) {
            "worker.toml"
        } else {
            "discovered"
        };
        let list = models
            .get(h)
            .map_or("none (runs its default)".into(), |m| m.join(", "));
        println!("{h} models ({from}): {list}");
    }
    let scan = roots.clone();
    let inventory = tokio::task::spawn_blocking(move || inventory::collect(&scan)).await?;
    print_inventory(&inventory);
    let purpose = match a.purpose {
        Some(p) => p,
        None if tty => {
            eprintln!("Purpose: what this machine is for, so the planner sends it matching tasks.");
            let answer = match &saved.purpose {
                Some(p) => line(&format!("Purpose [{p}] (- clears): "))?,
                None => line("Purpose (blank for none): ")?,
            };
            match answer.as_str() {
                "" => saved.purpose.clone().unwrap_or_default(),
                "-" => String::new(),
                _ => answer,
            }
        }
        None => saved.purpose.clone().unwrap_or_default(),
    };
    let flags = PermissionFlags {
        mode: a.permissions,
        folders: a.folders,
        mcp: a.mcp,
        commands: a.commands,
    };
    let perms = ask_permissions(flags, tty, saved.permissions.as_ref(), &roots, &inventory)?;
    println!("{}", describe(&perms));
    let cfg = Config {
        purpose: Some(purpose.trim().to_string()).filter(|p| !p.is_empty()),
        inventory: Some(inventory),
        permissions: Some(perms),
        ..cfg
    };
    config::write(&path, &cfg)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Answers to the permission questions, from flags or none.
struct PermissionFlags {
    mode: Option<String>,
    folders: Vec<String>,
    mcp: Vec<String>,
    commands: Vec<String>,
}

/// Setup's names for the modes, recommended first.
const MODES: [(&str, PermissionMode); 3] = [
    ("ask", PermissionMode::Ask),
    ("folders", PermissionMode::Folders),
    ("full", PermissionMode::Full),
];

fn mode_named(name: &str) -> Option<PermissionMode> {
    MODES.iter().find(|(n, _)| *n == name).map(|(_, m)| *m)
}

/// The `[permissions]` to save: each flag answers one question; on a terminal
/// the rest are asked, defaulting to the saved value, else ask mode with
/// the roots, every MCP server found, and no commands.
fn ask_permissions(
    f: PermissionFlags,
    tty: bool,
    saved: Option<&Permissions>,
    roots: &[std::path::PathBuf],
    inv: &Inventory,
) -> Result<Permissions> {
    let home = std::env::home_dir();
    let saved_mode = saved.map_or(PermissionMode::Ask, |p| p.mode);
    let mode = match f.mode.as_deref() {
        Some(name) => mode_named(name).unwrap_or(saved_mode),
        None if tty => {
            eprintln!(
                "Permissions: folders = agents write only in the task's dir and chosen folders, \
                 and claude runs only the MCP servers and commands you list, plus read-only \
                 ones (codex runs any command, sandboxed); ask = folders, and claude asks you \
                 on the page for anything else (recommended); full = anything, anywhere."
            );
            let default = MODES
                .iter()
                .find(|(_, m)| *m == saved_mode)
                .map_or("ask", |(n, _)| n);
            loop {
                match mode_named(&ask("Mode (ask/folders/full)", Some(default))?) {
                    Some(m) => break m,
                    None => eprintln!("answer ask, folders or full"),
                }
            }
        }
        None => saved_mode,
    };
    // Names a rule can't hold (e.g. "my.server") are left for the user to see in the inventory.
    let found_mcp: std::collections::BTreeSet<&String> = (inv.mcp.values().flatten())
        .filter(|n| {
            let one = Permissions {
                mcp: vec![n.to_string()],
                ..Default::default()
            };
            mis_agent::check(&one).is_ok()
        })
        .collect();
    // Full keeps any saved lists, so switching back finds them again.
    let mut p = saved.cloned().unwrap_or_else(|| Permissions {
        folders: roots.iter().map(|r| r.display().to_string()).collect(),
        mcp: found_mcp.into_iter().cloned().collect(),
        ..Default::default()
    });
    p.mode = mode;
    if mode == PermissionMode::Full {
        return Ok(p);
    }
    // Each list is checked on its own, so a bad answer is asked again.
    let check = |q: Permissions| permissions::resolve(q, home.as_deref());
    let folders = ask_list(
        f.folders,
        tty,
        "Folders agents may write",
        &p.folders,
        |l| {
            Ok(check(Permissions {
                folders: l,
                ..Default::default()
            })?
            .folders)
        },
    )?;
    let mcp = ask_list(f.mcp, tty, "MCP servers agents may use", &p.mcp, |l| {
        Ok(check(Permissions {
            mcp: l,
            ..Default::default()
        })?
        .mcp)
    })?;
    let found = inventory::commands(inv, mis_agent::which);
    let label = match found.is_empty() {
        true => "Commands agents may run, e.g. python3".to_string(),
        false => format!("Commands agents may run (found: {})", found.join(", ")),
    };
    let commands = ask_list(f.commands, tty, &label, &p.commands, |l| {
        Ok(check(Permissions {
            commands: l,
            ..Default::default()
        })?
        .commands)
    })?;
    Ok(Permissions {
        mode,
        folders,
        mcp,
        commands,
    })
}

/// A comma-separated list: from `flag` when given (one empty value clears),
/// else asked on a terminal (blank keeps `current`, `-` clears), else
/// `current`. `check` normalises it; a bad typed answer is asked again.
fn ask_list(
    flag: Vec<String>,
    tty: bool,
    label: &str,
    current: &[String],
    check: impl Fn(Vec<String>) -> Result<Vec<String>>,
) -> Result<Vec<String>> {
    let split = |s: &str| -> Vec<String> {
        (s.split(','))
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    };
    if !flag.is_empty() {
        return check(flag.iter().flat_map(|f| split(f)).collect());
    }
    if !tty {
        return check(current.to_vec());
    }
    let shown = match current.is_empty() {
        true => "none".to_string(),
        false => current.join(", "),
    };
    loop {
        let list = match line(&format!(
            "{label}, comma-separated [{shown}] (- for none): "
        ))?
        .as_str()
        {
            "" => current.to_vec(),
            "-" => vec![],
            answer => split(answer),
        };
        match check(list) {
            Ok(l) => return Ok(l),
            Err(e) => eprintln!("{e:#}"),
        }
    }
}

/// e.g. "permissions: folders /a, /b; mcp blender; commands none".
fn describe(p: &Permissions) -> String {
    let list = |v: &[String]| match v.is_empty() {
        true => "none".to_string(),
        false => v.join(", "),
    };
    match p.mode {
        PermissionMode::Full => "permissions: full (agents may do anything)".into(),
        PermissionMode::Folders | PermissionMode::Ask => format!(
            "permissions: {} {} (plus each task's dir); mcp {}; commands {}",
            if p.mode == PermissionMode::Ask {
                "ask, folders"
            } else {
                "folders"
            },
            list(&p.folders),
            list(&p.mcp),
            list(&p.commands)
        ),
    }
}

/// One line per part of `Inventory::brief`, e.g. "apps: Blender 4.2.3".
fn print_inventory(inv: &Inventory) {
    match inv.brief().as_str() {
        "" => println!("inventory: nothing found"),
        brief => brief.split("; ").for_each(|part| println!("{part}")),
    }
}

/// Open the worker socket, say Hello, and wait for Welcome.
async fn test_connect(cfg: &config::Resolved, id: WorkerId, info: &WorkerInfo) -> Result<()> {
    let req = crate::worker_request(cfg, &crate::access_values(cfg)?)?;
    let handshake = async {
        let (mut ws, _) = connect_async(req)
            .await
            .with_context(|| format!("connecting to {}", cfg.url))?;
        let hello = WorkerMsg::Hello {
            id,
            info: info.clone(),
            running: vec![],
        };
        ws.send(crate::text(&hello)).await?;
        while let Some(msg) = ws.next().await {
            if let Message::Text(t) = msg? {
                if let Ok(OrchMsg::Welcome { .. }) = serde_json::from_str(&t) {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
            }
        }
        bail!("orchestrator closed the connection before Welcome")
    };
    tokio::time::timeout(Duration::from_secs(5), handshake)
        .await
        .with_context(|| format!("no Welcome from {} within 5s", cfg.url))?
}

/// 401 (bad token) or 403 (refused by Cloudflare Access) from the handshake.
fn denied(e: &anyhow::Error) -> Option<u16> {
    match e.downcast_ref::<tungstenite::Error>() {
        Some(tungstenite::Error::Http(r)) => {
            Some(r.status().as_u16()).filter(|s| matches!(s, 401 | 403))
        }
        _ => None,
    }
}

/// A local orchestrator on the default address, if one answers quickly.
async fn probe() -> Option<String> {
    let url = config::DEFAULT_URL;
    let attempt = connect_async(format!("{url}/ws/client"));
    match tokio::time::timeout(Duration::from_millis(500), attempt).await {
        // A 401 still means an orchestrator is listening.
        Ok(Ok(_)) | Ok(Err(tungstenite::Error::Http(_))) => Some(url.into()),
        _ => None,
    }
}

/// Only a public `wss://` host can sit behind Cloudflare Access.
fn may_need_access(url: &str) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let Some(host) = uri.host().filter(|_| uri.scheme_str() == Some("wss")) else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => !(ip.is_loopback() || ip.is_private() || ip.is_link_local()),
        Ok(IpAddr::V6(ip)) => {
            !(ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local())
        }
        Err(_) => !host.eq_ignore_ascii_case("localhost"),
    }
}

/// Print `prompt` to stderr and read one trimmed line from stdin.
fn line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut s = String::new();
    if std::io::stdin().lock().read_line(&mut s)? == 0 {
        bail!("stdin closed");
    }
    Ok(s.trim().to_string())
}

/// `label [default]:`; blank takes the default, or asks again if there is none.
fn ask(label: &str, default: Option<&str>) -> Result<String> {
    loop {
        let answer = match default {
            Some(d) => line(&format!("{label} [{d}]: "))?,
            None => line(&format!("{label}: "))?,
        };
        match (answer.is_empty(), default) {
            (false, _) => return Ok(answer),
            (true, Some(d)) => return Ok(d.into()),
            (true, None) => continue,
        }
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    Ok(matches!(line(prompt)?.to_lowercase().as_str(), "y" | "yes"))
}

fn ask_token() -> Result<Option<String>> {
    let t = rpassword::prompt_password("Token (hidden, empty for none): ")?;
    Ok(Some(t).filter(|t| !t.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_question_only_for_public_wss() {
        assert!(may_need_access("wss://mis.example.com"));
        assert!(may_need_access("wss://8.8.8.8:443/"));
        assert!(!may_need_access("ws://mis.example.com"));
        assert!(!may_need_access("wss://localhost:7474"));
        assert!(!may_need_access("wss://127.0.0.1"));
        assert!(!may_need_access("wss://192.168.1.5:7474"));
        assert!(!may_need_access("wss://10.0.0.2"));
        assert!(!may_need_access("wss://[::1]:7474"));
        assert!(!may_need_access("wss://[fd00::1]"));
        assert!(!may_need_access("not a url"));
    }

    #[test]
    fn denied_reports_401_and_403_only() {
        let http = |code: u16| {
            let r = tungstenite::http::Response::builder()
                .status(code)
                .body(None)
                .unwrap();
            anyhow::Error::from(tungstenite::Error::Http(r)).context("connecting")
        };
        assert_eq!(denied(&http(401)), Some(401));
        assert_eq!(denied(&http(403)), Some(403));
        assert_eq!(denied(&http(500)), None);
        assert_eq!(denied(&anyhow::anyhow!("timeout")), None);
    }
}

//! `mis-orchestrator doctor`: can this machine host the orchestrator with these settings?
//! [`probe`] runs commands and reads files; [`checks`] judges what it found.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{self, nonempty, Args, Level};

/// What the shell found; each `None` is a probe that didn't apply here.
#[derive(Debug, Default)]
pub struct Probe {
    /// `claude auth status --json` stdout; None: no `claude` on PATH.
    pub claude: Option<String>,
    /// `git ls-remote` of the notes remote; None: unset.
    pub notes: Option<Result<(), String>>,
    pub state: Option<StateDir>,
    pub mac: Option<Mac>,
    /// `cloudflared tunnel ingress rule <public url>` stdout; None: not checked.
    pub tunnel: Option<String>,
}

#[derive(Debug)]
pub struct StateDir {
    pub path: PathBuf,
    /// Permission bits; None: not created yet.
    pub mode: Option<u32>,
    /// Ok(true): free; Ok(false): an orchestrator holds it; Err: can't tell.
    pub lock: Result<bool, String>,
}

#[derive(Debug, Default)]
pub struct Mac {
    /// `pmset -g custom` (per power source) and `pmset -g` (holds `SleepDisabled`).
    pub pmset_custom: String,
    pub pmset: String,
    pub auto_login: bool,
    /// `fdesetup status`.
    pub filevault: String,
    /// Run over SSH (`SSH_CONNECTION`).
    pub ssh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Check {
    pub check: &'static str,
    pub level: Level,
    pub detail: String,
}

fn check(check: &'static str, level: Level, detail: impl Into<String>) -> Check {
    let detail = detail.into();
    Check {
        check,
        level,
        detail,
    }
}

/// Every check, in order: the config rules serve applies, then the machine.
pub fn checks(args: &Args, state_dir: Option<&Path>, p: &Probe) -> Vec<Check> {
    use Level::*;
    let mut out: Vec<Check> = (config::validate(args, state_dir).into_iter())
        .map(|f| check("config", f.level, f.msg))
        .collect();
    if out.is_empty() {
        out.push(check("config", Ok, "flags and env agree"));
    }
    let signed_in = |json: &str| {
        serde_json::from_str::<serde_json::Value>(json)
            .is_ok_and(|v| v["loggedIn"] == serde_json::Value::Bool(true))
    };
    out.push(match &p.claude {
        _ if args.no_planner && args.no_summary => check("claude", Ok, "not used"),
        None => check("claude", Warn, "no `claude` on PATH: planner, summaries and drafts off"),
        Some(json) if signed_in(json) => check("claude", Ok, "signed in"),
        Some(_) => check(
            "claude",
            Warn,
            "not signed in (`claude auth login`; headless: `claude setup-token`): planner, summaries and drafts fail",
        ),
    });
    out.push(match &p.notes {
        None => check("notes", Ok, "no MIS_NOTES_REMOTE: captures stay here"),
        Some(Result::Ok(())) => check("notes", Ok, "remote reachable"),
        Some(Err(e)) => check("notes", Fail, format!("git ls-remote: {e}")),
    });
    out.push(match &p.state {
        None => check("state dir", Warn, "none: task history is lost on restart"),
        Some(s) => state_check(s),
    });
    if let Some(mac) = &p.mac {
        out.extend(mac_checks(mac));
    }
    if let (Some(rule), Some(url)) = (&p.tunnel, nonempty(&args.public_url)) {
        out.push(tunnel_check(rule, &url, &args.bind));
    }
    out
}

fn state_check(s: &StateDir) -> Check {
    let dir = s.path.display();
    match (s.mode, &s.lock) {
        (None, _) => check("state dir", Level::Ok, format!("{dir}: created on start")),
        (Some(m), _) if m & 0o077 != 0 => check(
            "state dir",
            Level::Fail,
            format!("{dir} is readable by others (mode {m:o}): chmod 700 {dir}"),
        ),
        (_, Err(e)) => check("state dir", Level::Fail, format!("{dir}: locking: {e}")),
        (_, Ok(false)) => check(
            "state dir",
            Level::Warn,
            format!("{dir}: an orchestrator is running on it"),
        ),
        (_, Ok(true)) => check("state dir", Level::Ok, format!("{dir}: private, free")),
    }
}

/// `key`'s value in `pmset -g custom`'s `AC Power:` block: `sleep 0 (…)` -> "0".
fn ac_value<'a>(custom: &'a str, key: &str) -> Option<&'a str> {
    let block = custom
        .lines()
        .skip_while(|l| l.trim() != "AC Power:")
        .skip(1);
    let block = block.take_while(|l| l.starts_with(char::is_whitespace));
    block
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|t| t.first() == Some(&key))
        .and_then(|t| t.get(1).copied())
}

fn mac_checks(m: &Mac) -> Vec<Check> {
    use Level::*;
    let mut out = Vec::new();
    let sleep_disabled = (m.pmset.lines()).any(|l| l.split_whitespace().eq(["SleepDisabled", "1"]));
    match ac_value(&m.pmset_custom, "sleep") {
        Some(n) if n != "0" && !sleep_disabled => out.push(check(
            "sleep",
            Warn,
            format!("sleeps after {n} min on AC: sudo pmset -c sleep 0"),
        )),
        Some(_) => out.push(check("sleep", Ok, "never on AC")),
        None => {}
    }
    let autorestart = ac_value(&m.pmset_custom, "autorestart");
    out.push(match autorestart {
        Some("0") => check(
            "power loss",
            Warn,
            "stays off after a power cut: sudo pmset -c autorestart 1",
        ),
        Some(_) => check("power loss", Ok, "restarts after a power cut"),
        None => check("power loss", Ok, "n/a (no autorestart here)"),
    });
    out.push(match m.auto_login {
        true => check("login", Ok, "auto-login on"),
        false => check(
            "login",
            Warn,
            "no auto-login: after a restart nothing runs until someone logs in",
        ),
    });
    if m.filevault.contains("FileVault is On") && autorestart.is_some_and(|v| v != "0") {
        out.push(check(
            "filevault",
            Warn,
            "on: after a power cut the Mac waits at the unlock screen",
        ));
    }
    if m.ssh {
        out.push(check(
            "ssh",
            Warn,
            "over SSH: Keychain-backed claude/gh sign-ins may differ from the GUI session the service runs in",
        ));
    }
    out
}

/// Does the tunnel's ingress rule for `public_url` send it to `bind`'s port?
fn tunnel_check(rule: &str, public_url: &str, bind: &str) -> Check {
    let service = (rule.lines()).find_map(|l| l.trim().strip_prefix("service:").map(str::trim));
    let port = |s: &str| {
        let uri: axum::http::Uri = s.parse().ok()?;
        let default = match uri.scheme_str()? {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        Some(uri.port_u16().unwrap_or(default))
    };
    let bind_port = bind
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok());
    match service {
        Some(s) if port(s).is_some() && port(s) == bind_port => {
            check("tunnel", Level::Ok, format!("{public_url} -> {s}"))
        }
        Some(s) if port(s).is_some() => check(
            "tunnel",
            Level::Warn,
            format!("{public_url} -> {s}, not --bind {bind}"),
        ),
        _ => check(
            "tunnel",
            Level::Warn,
            format!("no cloudflared ingress rule sends {public_url} to this orchestrator"),
        ),
    }
}

/// Human-readable: one line per check, `FAIL` loud.
pub fn render(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        let level = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        s += &format!("{level}  {:<10}  {}\n", c.check, c.detail);
    }
    s
}

/// Run every probe that applies to `args` on this machine.
pub fn probe(args: &Args, state_dir: Option<&Path>) -> Probe {
    let stdout = |cmd: &str, a: &[&str]| -> Option<(bool, String)> {
        let out = Command::new(cmd)
            .args(a)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        Some((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into(),
        ))
    };
    let claude = (mis_agent::which("claude"))
        .then(|| stdout("claude", &["auth", "status", "--json"]).map(|(_, s)| s))
        .map(Option::unwrap_or_default);
    let notes = nonempty(&args.notes_remote).map(|r| crate::notes::reachable(&r));
    let state = state_dir.map(|dir| {
        let mode = std::fs::metadata(dir).ok().map(|m| mode_bits(&m));
        let lock = match mode {
            None => Ok(true),
            Some(_) => match crate::store::lock(dir) {
                Ok(_released) => Ok(true),
                Err(e) if e.is::<crate::store::Locked>() => Ok(false),
                Err(e) => Err(format!("{e:#}")),
            },
        };
        StateDir {
            path: dir.to_path_buf(),
            mode,
            lock,
        }
    });
    let text = |cmd, a| stdout(cmd, a).map(|(_, s)| s).unwrap_or_default();
    let mac = cfg!(target_os = "macos").then(|| Mac {
        pmset_custom: text("pmset", &["-g", "custom"]),
        pmset: text("pmset", &["-g"]),
        auto_login: stdout(
            "defaults",
            &[
                "read",
                "/Library/Preferences/com.apple.loginwindow",
                "autoLoginUser",
            ],
        )
        .is_some_and(|(ok, _)| ok),
        filevault: text("fdesetup", &["status"]),
        ssh: std::env::var_os("SSH_CONNECTION").is_some(),
    });
    let tunnel = nonempty(&args.public_url)
        .filter(|_| mis_agent::which("cloudflared"))
        .and_then(|u| {
            let url = mis_types::http_url(&u);
            stdout("cloudflared", &["tunnel", "ingress", "rule", &url])
        })
        .and_then(|(ok, s)| ok.then_some(s));
    Probe {
        claude,
        notes,
        state,
        mac,
        tunnel,
    }
}

#[cfg(unix)]
fn mode_bits(m: &std::fs::Metadata) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777
}

/// No group/world bits to check elsewhere.
#[cfg(not(unix))]
fn mode_bits(_: &std::fs::Metadata) -> u32 {
    0o700
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_args;
    use Level::*;

    /// A laptop's `pmset -g custom`: a battery block, then AC.
    const PMSET: &str = "Battery Power:\n sleep                5\n autorestart          0\n\
        AC Power:\n Sleep On Power Button 1\n displaysleep         180\n sleep                {s}{r}\n";

    fn pmset(sleep: &str, autorestart: Option<&str>) -> String {
        let r = autorestart.map_or(String::new(), |r| format!("\n autorestart          {r}"));
        PMSET.replace("{s}", sleep).replace("{r}", &r)
    }

    fn mac(custom: String) -> Mac {
        Mac {
            pmset_custom: custom,
            auto_login: true,
            ..Default::default()
        }
    }

    fn by_name<'a>(checks: &'a [Check], name: &str) -> Vec<(Level, &'a str)> {
        let c = checks.iter().filter(|c| c.check == name);
        c.map(|c| (c.level, c.detail.as_str())).collect()
    }

    #[test]
    fn ac_sleep_and_power_loss() {
        let level = |m: Mac, name| by_name(&mac_checks(&m), name)[0].0;
        assert_eq!(
            level(mac(pmset("0 (sleep prevented by powerd)", None)), "sleep"),
            Ok
        );
        assert_eq!(level(mac(pmset("10", None)), "sleep"), Warn);
        let disabled = Mac {
            pmset: "System-wide power settings:\n SleepDisabled\t\t1\n".into(),
            ..mac(pmset("10", None))
        };
        assert_eq!(level(disabled, "sleep"), Ok);
        // Battery's `autorestart 0` is not AC's; missing on AC is n/a.
        assert_eq!(
            by_name(&mac_checks(&mac(pmset("0", None))), "power loss"),
            [(Ok, "n/a (no autorestart here)")]
        );
        assert_eq!(level(mac(pmset("0", Some("0"))), "power loss"), Warn);
        assert_eq!(level(mac(pmset("0", Some("1"))), "power loss"), Ok);
        assert!(by_name(&mac_checks(&mac(String::new())), "sleep").is_empty());
    }

    #[test]
    fn login_filevault_and_ssh() {
        let m = Mac {
            auto_login: false,
            filevault: "FileVault is On.\n".into(),
            ssh: true,
            ..mac(pmset("0", Some("1")))
        };
        let c = mac_checks(&m);
        assert_eq!(by_name(&c, "login")[0].0, Warn);
        assert_eq!(by_name(&c, "filevault")[0].0, Warn);
        assert_eq!(by_name(&c, "ssh")[0].0, Warn);
        let off = Mac {
            filevault: "FileVault is Off.\n".into(),
            ..mac(pmset("0", Some("1")))
        };
        let c = mac_checks(&off);
        assert!(by_name(&c, "filevault").is_empty() && by_name(&c, "ssh").is_empty());
        assert_eq!(by_name(&c, "login")[0].0, Ok);
    }

    #[test]
    fn claude_sign_in() {
        let level = |claude: Option<&str>, argv: &[&str]| {
            let p = Probe {
                claude: claude.map(Into::into),
                ..Default::default()
            };
            by_name(&checks(&test_args(argv), None, &p), "claude")[0].0
        };
        assert_eq!(level(Some(r#"{"loggedIn":true}"#), &[]), Ok);
        assert_eq!(level(Some(r#"{"loggedIn":false}"#), &[]), Warn);
        assert_eq!(level(Some(""), &[]), Warn);
        assert_eq!(level(None, &[]), Warn);
        assert_eq!(level(None, &["--no-planner", "--no-summary"]), Ok);
    }

    #[test]
    fn state_dir_and_notes() {
        let dir = |mode, lock| StateDir {
            path: "/s".into(),
            mode,
            lock,
        };
        let level = |s: StateDir| state_check(&s).level;
        assert_eq!(level(dir(None, Result::Ok(true))), Ok);
        assert_eq!(level(dir(Some(0o700), Result::Ok(true))), Ok);
        assert_eq!(level(dir(Some(0o700), Result::Ok(false))), Warn);
        assert_eq!(level(dir(Some(0o750), Result::Ok(true))), Fail);
        assert_eq!(level(dir(Some(0o700), Err("denied".into()))), Fail);
        let p = Probe {
            notes: Some(Err("<remote>: not found".into())),
            ..Default::default()
        };
        let c = checks(&test_args(&[]), Some(Path::new("/s")), &p);
        assert_eq!(
            by_name(&c, "notes"),
            [(Fail, "git ls-remote: <remote>: not found")]
        );
        assert_eq!(
            by_name(&c, "state dir")[0].0,
            Warn,
            "no probe: no state dir"
        );
    }

    #[test]
    fn config_findings_lead() {
        let c = checks(
            &test_args(&["--bind", "0.0.0.0:7474"]),
            None,
            &Probe::default(),
        );
        assert_eq!(c[0].check, "config");
        assert_eq!(c[0].level, Fail);
        assert!(render(&c).starts_with("FAIL  config      refusing to bind"));
    }

    #[test]
    fn tunnel_rule_matches_bind_port() {
        let rule = |s: &str| {
            format!("Using rules from /x\nMatched rule #0\n\thostname: h\n\tservice: {s}\n")
        };
        let level = |s: &str, bind| tunnel_check(&rule(s), "https://h", bind).level;
        assert_eq!(level("http://localhost:7474", "127.0.0.1:7474"), Ok);
        assert_eq!(level("http://127.0.0.1:7474/", "0.0.0.0:7474"), Ok);
        assert_eq!(level("http://localhost:8080", "127.0.0.1:7474"), Warn);
        assert_eq!(level("http_status:404", "127.0.0.1:7474"), Warn);
        let p = Probe {
            tunnel: Some(rule("http://localhost:1")),
            ..Default::default()
        };
        let url = ["--public-url", "wss://h"];
        assert_eq!(
            by_name(&checks(&test_args(&url), None, &p), "tunnel")[0].0,
            Warn
        );
        assert!(by_name(&checks(&test_args(&[]), None, &p), "tunnel").is_empty());
    }
}

//! The orchestrator's flags (each also an env var) and the rules they must meet together:
//! `validate` is pure; serve refuses to start on a [`Level::Fail`], `doctor` lists them all.

use std::path::Path;

/// How bad a finding is. `doctor` exits 1 on any `Fail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub msg: String,
}

#[derive(clap::Args)]
pub struct Args {
    #[arg(long, env = "MIS_BIND", default_value = "127.0.0.1:7474")]
    pub bind: String,
    /// Shared secret clients and workers must send as a bearer token.
    #[arg(long, env = "MIS_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// Read-only token: sees tasks, workers and transcripts but cannot submit (repeatable, or
    /// comma-separated `MIS_READ_TOKENS`, preferred: argv is visible to other users). Needs --token.
    #[arg(
        long = "read-token",
        env = "MIS_READ_TOKENS",
        value_delimiter = ',',
        hide_env_values = true
    )]
    pub read_tokens: Vec<String>,
    /// Capture-only token (a phone's share sheet): may `POST /api/capture` and nothing else
    /// (repeatable, or comma-separated `MIS_CAPTURE_TOKENS`, preferred). Needs --token.
    #[arg(
        long = "capture-token",
        env = "MIS_CAPTURE_TOKENS",
        value_delimiter = ',',
        hide_env_values = true
    )]
    pub capture_tokens: Vec<String>,
    /// Externally reachable URL (e.g. a tunnel hostname), shown in connection hints.
    #[arg(long, env = "MIS_PUBLIC_URL")]
    pub public_url: Option<String>,
    /// Serve published apps at `/a/<slug>/` on this address, e.g. `127.0.0.1:7475`. Unset: not
    /// served. Each app to its members only (`mis apps share`), signed in through the Access
    /// app of --apps-cf-access-aud; direct loopback requests are every app's owner.
    #[arg(long, env = "MIS_APPS_BIND")]
    pub apps_bind: Option<String>,
    /// Where the apps listener is reached from outside, e.g. `https://home.example.com`; app
    /// links point there, and its host is the only non-loopback Host the listener answers.
    /// Default: the --apps-bind address.
    #[arg(long, env = "MIS_APPS_PUBLIC_URL")]
    pub apps_public_url: Option<String>,
    /// App development: a direct loopback request may act as a viewer or editor by sending
    /// `X-Mis-Dev-Role`, to try an app's other roles locally. Never for a served household.
    #[arg(long, env = "MIS_APPS_DEV", value_parser = clap::builder::BoolishValueParser::new())]
    pub apps_dev: bool,
    /// Browser origin allowed to connect, e.g. `https://mis.example.com` (repeatable).
    #[arg(
        long = "allowed-origin",
        env = "MIS_ALLOWED_ORIGINS",
        value_delimiter = ','
    )]
    pub allowed_origins: Vec<String>,
    /// Don't plan with the local `claude` CLI; tasks without --harness run as-is on the default
    /// harness (Settings; claude unset).
    #[arg(long)]
    pub no_planner: bool,
    /// Model the planner itself runs on.
    #[arg(long, default_value = "sonnet", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    pub planner_model: String,
    /// Don't summarize finished tasks (they keep the agent's own last message) or suggest
    /// where inbox notes go.
    #[arg(long)]
    pub no_summary: bool,
    /// Model that summarizes finished tasks and suggests where inbox notes go.
    #[arg(long, default_value = "haiku", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    pub summary_model: String,
    /// Cloudflare Access team: the `<team>` in `<team>.cloudflareaccess.com`. With
    /// --cf-access-aud, non-local requests need a valid Access JWT.
    #[arg(long, env = "MIS_CF_ACCESS_TEAM")]
    pub cf_access_team: Option<String>,
    /// Application Audience (AUD) tag of the Access application.
    #[arg(long, env = "MIS_CF_ACCESS_AUD")]
    pub cf_access_aud: Option<String>,
    /// Only these Access identities get in: emails, or service token client ids (repeatable,
    /// or comma-separated `MIS_ACCESS_SUBJECTS`). Unset: anyone the Access application admits.
    #[arg(
        long = "access-subject",
        env = "MIS_ACCESS_SUBJECTS",
        value_delimiter = ','
    )]
    pub access_subjects: Vec<String>,
    /// AUD tag of a second Access app on path `/api/capture` (Service Auth for the phone's
    /// capture-only service token). Its JWT is accepted there only, and only to capture.
    #[arg(long, env = "MIS_CF_ACCESS_CAPTURE_AUD")]
    pub cf_access_capture_aud: Option<String>,
    /// AUD tag of the Access app in front of the apps listener (its own hostname and
    /// policy, same team). Its JWTs are accepted there only; the MIS app's are refused there.
    #[arg(long, env = "MIS_APPS_CF_ACCESS_AUD")]
    pub apps_cf_access_aud: Option<String>,
    /// Fetch Access keys from here instead of the team's certs URL (testing).
    #[arg(long, hide = true)]
    pub cf_access_certs_url: Option<String>,
    /// Holds task history (`ledger.json`, `transcripts/`; tasks are kept 30 days
    /// after they end), the push notification key and subscriptions, and uploaded
    /// images (`images/`, deleted after a week), evidence files agents attach
    /// (`evidence/`, deleted after 30 days), captures (`captures/inbox/`, kept), the notes
    /// repo clone (`notes/`) and published apps (`apps/`, every release kept). Default: ~/.local/state/mis
    /// (%LOCALAPPDATA%\mis on Windows).
    #[arg(long, env = "MIS_STATE_DIR")]
    pub state_dir: Option<std::path::PathBuf>,
    /// Routine files: one Markdown file per periodic task, re-read every 30s.
    /// Default: ~/.config/mis/routines.
    #[arg(long, env = "MIS_ROUTINES_DIR")]
    pub routines_dir: Option<std::path::PathBuf>,
    /// Git remote of the notes repo, e.g. `https://github.com/you/mis-notes.git`. Captures are
    /// published to its `inbox/` on `main`, through a clone at `<state-dir>/notes`, with the
    /// user's git credentials (e.g. `gh auth setup-git`). Unset: not published.
    #[arg(long, env = "MIS_NOTES_REMOTE")]
    pub notes_remote: Option<String>,
    /// VAPID contact push services may use about this sender (`mailto:` or `https:`).
    #[arg(
        long,
        env = "MIS_VAPID_SUBJECT",
        default_value = "https://github.com/tommy-xr/MakeItSo"
    )]
    pub vapid_subject: String,
    /// Release channel this build follows: `stable` (tagged releases), `edge` (every merge to
    /// main) or `off`. It checks every 5 min and, once no task runs, restarts on a newer release
    /// that passes `--self-test`; workers switch to this build from it. Default: stable for CI
    /// release builds, else off (`mis-up` sets off).
    #[arg(long, env = "MIS_UPDATE_CHANNEL", value_enum)]
    pub update_channel: Option<UpdateChannel>,
    /// Seconds between release checks (testing).
    #[arg(long, hide = true, default_value_t = 300)]
    pub update_every: u64,
    /// Check this build can run here (reads the state dir without locking it, binds a port),
    /// then exit. The updater runs it on a new build before switching.
    #[arg(long)]
    pub self_test: bool,
    /// File of `KEY=value` lines for the env vars above, e.g. `~/.config/mis/orchestrator.env`,
    /// read before anything else; variables already set win. Only read when given.
    #[arg(long, env = "MIS_ENV_FILE")]
    pub env_file: Option<std::path::PathBuf>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum UpdateChannel {
    Stable,
    Edge,
    Off,
}

/// `v`, unless unset or empty: `MIS_..=` means unset.
pub fn nonempty(v: &Option<String>) -> Option<String> {
    v.clone().filter(|v| !v.is_empty())
}

/// A repeatable flag's values, trimmed, blanks dropped: `MIS_READ_TOKENS=` lists none.
pub fn trimmed(v: &[String]) -> Vec<String> {
    (v.iter().map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Every problem with `a` (raw, as parsed) that serve would refuse or warn about at startup.
/// `state_dir`: the one in use, if any.
pub fn validate(a: &Args, state_dir: Option<&Path>) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut fail = |msg: &str| {
        out.push(Finding {
            level: Level::Fail,
            msg: msg.into(),
        })
    };
    let token = nonempty(&a.token);
    let (read, capture) = (trimmed(&a.read_tokens), trimmed(&a.capture_tokens));
    if !read.is_empty() && token.is_none() {
        fail("--read-token / MIS_READ_TOKENS needs --token / MIS_TOKEN");
    }
    if !capture.is_empty() && token.is_none() {
        fail("--capture-token / MIS_CAPTURE_TOKENS needs --token / MIS_TOKEN");
    }
    if (capture.iter()).any(|t| token.as_ref() == Some(t) || read.contains(t)) {
        fail("capture tokens must differ from the full and read tokens");
    }
    match a.bind.parse::<std::net::SocketAddr>() {
        Err(e) => fail(&format!("--bind {:?}: {e}", a.bind)),
        Ok(addr) if !addr.ip().is_loopback() && token.is_none() => fail(&format!(
            "refusing to bind {addr} without --token / MIS_TOKEN"
        )),
        Ok(_) => {}
    }
    let (team, aud) = (nonempty(&a.cf_access_team), nonempty(&a.cf_access_aud));
    let capture_aud = nonempty(&a.cf_access_capture_aud);
    let gate = match (&team, &aud) {
        (Some(team), Some(aud)) => {
            if capture_aud.as_ref() == Some(aud) {
                fail("--cf-access-capture-aud must be another Access app's AUD");
            }
            if !team.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                fail("--cf-access-team is the <team> in <team>.cloudflareaccess.com");
            }
            true
        }
        (None, None) => {
            if capture_aud.is_some() {
                fail("--cf-access-capture-aud needs --cf-access-team/-aud");
            }
            false
        }
        _ => {
            fail("--cf-access-team and --cf-access-aud go together");
            false
        }
    };
    if let Some(apps_aud) = nonempty(&a.apps_cf_access_aud) {
        if team.is_none() {
            fail("--apps-cf-access-aud needs --cf-access-team/-aud");
        }
        if nonempty(&a.apps_bind).is_none() || nonempty(&a.apps_public_url).is_none() {
            fail("--apps-cf-access-aud needs --apps-bind and --apps-public-url");
        }
        if [&aud, &capture_aud].contains(&&Some(apps_aud)) {
            fail("--apps-cf-access-aud must be the home Access app's own AUD");
        }
    }
    // `MIS_ACCESS_SUBJECTS=` means unset; a list of only blanks (`,`) is a mistake, not "anyone".
    let subjects = trimmed(&a.access_subjects);
    let given = a.access_subjects.iter().any(|s| !s.is_empty()) || a.access_subjects.len() > 1;
    if given && subjects.is_empty() {
        fail("--access-subject / MIS_ACCESS_SUBJECTS lists no one");
    }
    if !gate && !subjects.is_empty() {
        fail("--access-subject / MIS_ACCESS_SUBJECTS needs Cloudflare Access");
    }
    if state_dir.is_none() && nonempty(&a.apps_bind).is_some() {
        fail("--apps-bind needs a state dir");
    }
    if state_dir.is_none() && nonempty(&a.notes_remote).is_some() {
        fail("--notes-remote / MIS_NOTES_REMOTE needs a state dir");
    }
    if let Some(Err(e)) =
        nonempty(&a.apps_public_url).map(|u| crate::app_access::public_authority(&u))
    {
        fail(&format!("--apps-public-url {e}"));
    }
    let mut warn = |msg: &str| {
        out.push(Finding {
            level: Level::Warn,
            msg: msg.into(),
        })
    };
    if gate && subjects.is_empty() {
        warn("Cloudflare Access: anyone its application admits gets full access; list who with --access-subject");
    }
    if gate && token.is_none() {
        warn("no --token: local requests without Cloudflare headers need nothing");
    }
    // Tokens travel in headers, URLs and env files, where other characters need escaping.
    let odd = |t: &String| {
        !t.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
    };
    if token.iter().chain(&read).chain(&capture).any(odd) {
        warn("tokens should use only A-Z a-z 0-9 - . _");
    }
    out
}

/// `argv` parsed as the orchestrator's flags, ignoring the env (tests run inside MIS sessions).
#[cfg(test)]
pub(crate) fn test_args(argv: &[&str]) -> Args {
    use clap::{Args as _, FromArgMatches};
    let cmd = Args::augment_args(clap::Command::new("t")).mut_args(|a| a.env(None::<&str>));
    let m = cmd.try_get_matches_from([&["t"], argv].concat()).unwrap();
    Args::from_arg_matches(&m).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(level, msg)` of each finding for `argv`, with a state dir.
    fn found(argv: &[&str]) -> Vec<(Level, String)> {
        let dir = Some(Path::new("/state"));
        let f = validate(&test_args(argv), dir).into_iter();
        f.map(|f| (f.level, f.msg)).collect()
    }

    fn fails(argv: &[&str]) -> Vec<String> {
        let f = found(argv).into_iter();
        f.filter(|(l, _)| *l == Level::Fail)
            .map(|(_, m)| m)
            .collect()
    }

    #[test]
    fn defaults_and_a_served_setup_pass() {
        assert_eq!(found(&[]), []);
        let served = [
            "--bind",
            "0.0.0.0:7474",
            "--token",
            "t-1",
            "--read-token",
            "r",
        ];
        assert_eq!(found(&served), []);
    }

    #[test]
    fn startup_refusals() {
        let cases: &[(&[&str], &str)] = &[
            (&["--bind", "0.0.0.0:7474"], "refusing to bind 0.0.0.0:7474"),
            (
                &["--bind", "0.0.0.0:7474", "--token", ""],
                "refusing to bind",
            ),
            (&["--bind", "nowhere"], "--bind \"nowhere\""),
            (
                &["--read-token", "r"],
                "--read-token / MIS_READ_TOKENS needs",
            ),
            (
                &["--capture-token", "c"],
                "--capture-token / MIS_CAPTURE_TOKENS needs",
            ),
            (
                &["--token", "t", "--capture-token", "t"],
                "capture tokens must differ",
            ),
            (&["--cf-access-team", "x"], "go together"),
            (
                &["--cf-access-capture-aud", "c"],
                "--cf-access-capture-aud needs",
            ),
            (&["--access-subject", ","], "lists no one"),
            (&["--access-subject", "a@b.c"], "needs Cloudflare Access"),
            (
                &["--apps-cf-access-aud", "p"],
                "--apps-cf-access-aud needs --cf-access-team",
            ),
            (&["--apps-public-url", "ftp://x"], "--apps-public-url"),
        ];
        for (argv, want) in cases {
            let got = fails(argv);
            assert!(got.iter().any(|m| m.contains(want)), "{argv:?}: {got:?}");
        }
        let access = ["--cf-access-team", "t", "--cf-access-aud", "a"];
        let fail = |extra: &[&str]| fails(&[&access[..], extra].concat());
        assert!(fail(&["--cf-access-capture-aud", "a"])[0].contains("another Access app"));
        assert!(fail(&["--apps-cf-access-aud", "a"])[0].contains("and --apps-public-url"));
        let team = ["--cf-access-team", "t.x", "--cf-access-aud", "a"];
        assert!(fails(&team)[0].contains("<team>.cloudflareaccess.com"));
    }

    #[test]
    fn state_dir_needed_for_apps_and_notes() {
        let a = test_args(&["--apps-bind", "127.0.0.1:7475", "--notes-remote", "r"]);
        assert_eq!(validate(&a, None).len(), 2);
        assert_eq!(validate(&a, Some(Path::new("/s"))), []);
    }

    #[test]
    fn warnings() {
        let access = ["--cf-access-team", "t", "--cf-access-aud", "a"];
        let got = found(&access);
        assert!(got.iter().all(|(l, _)| *l == Level::Warn), "{got:?}");
        assert!(got[0].1.contains("anyone its application admits"));
        assert!(got[1].1.starts_with("no --token"));
        let odd = found(&["--token", "a+b/c="]);
        assert_eq!(
            odd,
            [(
                Level::Warn,
                "tokens should use only A-Z a-z 0-9 - . _".into()
            )]
        );
        assert_eq!(found(&["--token", "aZ0-._"]), []);
    }
}

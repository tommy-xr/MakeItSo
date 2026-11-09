use clap::Parser;
use mis_orchestrator::access_gate::AccessGate;
use mis_orchestrator::capture::Inbox;
use mis_orchestrator::evidence;
use mis_orchestrator::images::Images;
use mis_orchestrator::push::Push;
use mis_orchestrator::{access, normalize_origins, App};

#[derive(Parser)]
#[command(about = "Make It So orchestrator", version = mis_orchestrator::VERSION)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7474")]
    bind: String,
    /// Shared secret clients and workers must send as a bearer token.
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    /// Read-only token: sees tasks, workers and transcripts but cannot submit (repeatable, or
    /// comma-separated `MIS_READ_TOKENS`, preferred: argv is visible to other users). Needs --token.
    #[arg(
        long = "read-token",
        env = "MIS_READ_TOKENS",
        value_delimiter = ',',
        hide_env_values = true
    )]
    read_tokens: Vec<String>,
    /// Capture-only token (a phone's share sheet): may `POST /api/capture` and nothing else
    /// (repeatable, or comma-separated `MIS_CAPTURE_TOKENS`, preferred). Needs --token.
    #[arg(
        long = "capture-token",
        env = "MIS_CAPTURE_TOKENS",
        value_delimiter = ',',
        hide_env_values = true
    )]
    capture_tokens: Vec<String>,
    /// Externally reachable URL (e.g. a tunnel hostname), shown in connection hints.
    #[arg(long, env = "MIS_PUBLIC_URL")]
    public_url: Option<String>,
    /// Serve published apps at `/a/<slug>/` on this address, e.g. `127.0.0.1:7475`. Unset: not
    /// served. Each app to its members only (`mis apps share`), signed in through the Access
    /// app of --apps-cf-access-aud; direct loopback requests are every app's owner.
    #[arg(long, env = "MIS_APPS_BIND")]
    apps_bind: Option<String>,
    /// Where the apps listener is reached from outside, e.g. `https://home.example.com`; app
    /// links point there, and its host is the only non-loopback Host the listener answers.
    /// Default: the --apps-bind address.
    #[arg(long, env = "MIS_APPS_PUBLIC_URL")]
    apps_public_url: Option<String>,
    /// App development: a direct loopback request may act as a viewer or editor by sending
    /// `X-Mis-Dev-Role`, to try an app's other roles locally. Never for a served household.
    #[arg(long, env = "MIS_APPS_DEV", value_parser = clap::builder::BoolishValueParser::new())]
    apps_dev: bool,
    /// Browser origin allowed to connect, e.g. `https://mis.example.com` (repeatable).
    #[arg(
        long = "allowed-origin",
        env = "MIS_ALLOWED_ORIGINS",
        value_delimiter = ','
    )]
    allowed_origins: Vec<String>,
    /// Don't plan with the local `claude` CLI; tasks without --harness run on claude as-is.
    #[arg(long)]
    no_planner: bool,
    /// Model the planner itself runs on.
    #[arg(long, default_value = "sonnet", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    planner_model: String,
    /// Don't summarize finished tasks; they keep the agent's own last message.
    #[arg(long)]
    no_summary: bool,
    /// Model that summarizes finished tasks.
    #[arg(long, default_value = "haiku", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    summary_model: String,
    /// Cloudflare Access team: the `<team>` in `<team>.cloudflareaccess.com`. With
    /// --cf-access-aud, non-local requests need a valid Access JWT.
    #[arg(long, env = "MIS_CF_ACCESS_TEAM")]
    cf_access_team: Option<String>,
    /// Application Audience (AUD) tag of the Access application.
    #[arg(long, env = "MIS_CF_ACCESS_AUD")]
    cf_access_aud: Option<String>,
    /// Only these Access identities get in: emails, or service token client ids (repeatable,
    /// or comma-separated `MIS_ACCESS_SUBJECTS`). Unset: anyone the Access application admits.
    #[arg(
        long = "access-subject",
        env = "MIS_ACCESS_SUBJECTS",
        value_delimiter = ','
    )]
    access_subjects: Vec<String>,
    /// AUD tag of a second Access app on path `/api/capture` (Service Auth for the phone's
    /// capture-only service token). Its JWT is accepted there only, and only to capture.
    #[arg(long, env = "MIS_CF_ACCESS_CAPTURE_AUD")]
    cf_access_capture_aud: Option<String>,
    /// AUD tag of the Access app in front of the apps listener (its own hostname and
    /// policy, same team). Its JWTs are accepted there only; the MIS app's are refused there.
    #[arg(long, env = "MIS_APPS_CF_ACCESS_AUD")]
    apps_cf_access_aud: Option<String>,
    /// Fetch Access keys from here instead of the team's certs URL (testing).
    #[arg(long, hide = true)]
    cf_access_certs_url: Option<String>,
    /// Holds task history (`ledger.json`, `transcripts/`; tasks are kept 30 days
    /// after they end), the push notification key and subscriptions, and uploaded
    /// images (`images/`, deleted after a week), evidence files agents attach
    /// (`evidence/`, deleted after 30 days), captures (`captures/inbox/`, kept), the notes
    /// repo clone (`notes/`) and published apps (`apps/`, every release kept). Default: ~/.local/state/mis
    /// (%LOCALAPPDATA%\mis on Windows).
    #[arg(long, env = "MIS_STATE_DIR")]
    state_dir: Option<std::path::PathBuf>,
    /// Routine files: one Markdown file per periodic task, re-read every 30s.
    /// Default: ~/.config/mis/routines.
    #[arg(long, env = "MIS_ROUTINES_DIR")]
    routines_dir: Option<std::path::PathBuf>,
    /// Git remote of the notes repo, e.g. `https://github.com/you/mis-notes.git`. Captures are
    /// published to its `inbox/` on `main`, through a clone at `<state-dir>/notes`, with the
    /// user's git credentials (e.g. `gh auth setup-git`). Unset: not published.
    #[arg(long, env = "MIS_NOTES_REMOTE")]
    notes_remote: Option<String>,
    /// VAPID contact push services may use about this sender (`mailto:` or `https:`).
    #[arg(
        long,
        env = "MIS_VAPID_SUBJECT",
        default_value = "https://github.com/tommy-xr/MakeItSo"
    )]
    vapid_subject: String,
    /// Release channel this build follows: `stable` (tagged releases), `edge` (every merge to
    /// main) or `off`. It checks every 5 min and, once no task runs, restarts on a newer release
    /// that passes `--self-test`; workers switch to this build from it. Default: stable for CI
    /// release builds, else off (`mis-up` sets off).
    #[arg(long, env = "MIS_UPDATE_CHANNEL", value_enum)]
    update_channel: Option<UpdateChannel>,
    /// Seconds between release checks (testing).
    #[arg(long, hide = true, default_value_t = 300)]
    update_every: u64,
    /// Check this build can run here (reads the state dir without locking it, binds a port),
    /// then exit. The updater runs it on a new build before switching.
    #[arg(long)]
    self_test: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum UpdateChannel {
    Stable,
    Edge,
    Off,
}

/// The channel to publish to workers; see `--update-channel`.
fn channel(arg: Option<UpdateChannel>, ci_build: bool) -> Option<mis_types::Channel> {
    match arg {
        Some(UpdateChannel::Stable) => Some(mis_types::Channel::Stable),
        Some(UpdateChannel::Edge) => Some(mis_types::Channel::Edge),
        Some(UpdateChannel::Off) => None,
        None => ci_build.then_some(mis_types::Channel::Stable),
    }
}

/// Env var `k` as a path, if set and not empty.
fn var(k: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(k)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// `$XDG_STATE_HOME/mis`, else `~/.local/state/mis`; `%LOCALAPPDATA%\mis` on Windows.
fn default_state_dir() -> Option<std::path::PathBuf> {
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")?
    } else {
        var("XDG_STATE_HOME").or_else(|| Some(var("HOME")?.join(".local/state")))?
    };
    Some(base.join("mis"))
}

/// `~/.config/mis/routines`, beside the worker's `worker.toml` (home is `%USERPROFILE%` on Windows).
fn default_routines_dir() -> Option<std::path::PathBuf> {
    let home = var("HOME").or_else(|| var("USERPROFILE"))?;
    Some(home.join(".config").join("mis").join("routines"))
}

/// Local `claude` for one tool-less turn on `model`: no user/project settings
/// (hooks, CLAUDE.md), MCP servers, or tools. (`--bare` would also drop the login.)
fn one_shot_claude(model: &str, extra: &[&str]) -> std::sync::Arc<mis_agent::claude::ClaudeAgent> {
    let base = [
        &format!("--model={model}"),
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--tools",
        "",
        "--max-turns",
        "1",
    ];
    std::sync::Arc::new(mis_agent::claude::ClaudeAgent {
        extra_args: base.iter().chain(extra).map(|s| s.to_string()).collect(),
    })
}

/// `v`, unless unset or empty: `MIS_..=` means unset.
fn nonempty(v: &Option<String>) -> Option<String> {
    v.clone().filter(|v| !v.is_empty())
}

fn certs_url(args: &Args, team: &str) -> String {
    nonempty(&args.cf_access_certs_url).unwrap_or_else(|| access::certs_url(team))
}

/// `--apps-cf-access-aud`: a gate of its own, so a JWT for one Access app never opens the
/// other. Needs the MIS app's team (checked by [`access_gate`]) and a public URL.
async fn apps_access_gate(args: &Args) -> anyhow::Result<Option<AccessGate>> {
    let Some(aud) = nonempty(&args.apps_cf_access_aud) else {
        return Ok(None);
    };
    let Some(team) = nonempty(&args.cf_access_team) else {
        anyhow::bail!("--apps-cf-access-aud needs --cf-access-team/-aud");
    };
    if nonempty(&args.apps_bind).is_none() || nonempty(&args.apps_public_url).is_none() {
        anyhow::bail!("--apps-cf-access-aud needs --apps-bind and --apps-public-url");
    }
    let others = [&args.cf_access_aud, &args.cf_access_capture_aud];
    if others.iter().any(|a| nonempty(a).as_ref() == Some(&aud)) {
        anyhow::bail!("--apps-cf-access-aud must be the home Access app's own AUD");
    }
    let gate = AccessGate::remote(team.clone(), aud, certs_url(args, &team));
    if let Err(e) = gate.refresh().await {
        tracing::warn!("fetching Cloudflare Access keys (apps): {e:#}; retrying on first request");
    }
    tracing::info!("Cloudflare Access: apps need the home Access app's JWT");
    Ok(Some(gate))
}

/// `--cf-access-team` / `--cf-access-aud`: both or neither; `MIS_..=` means unset.
async fn access_gate(args: &Args) -> anyhow::Result<Option<AccessGate>> {
    let capture_aud = nonempty(&args.cf_access_capture_aud);
    let (team, aud) = match (
        nonempty(&args.cf_access_team),
        nonempty(&args.cf_access_aud),
    ) {
        (Some(team), Some(aud)) => (team, aud),
        (None, None) if capture_aud.is_none() => return Ok(None),
        (None, None) => anyhow::bail!("--cf-access-capture-aud needs --cf-access-team/-aud"),
        _ => anyhow::bail!("--cf-access-team and --cf-access-aud go together"),
    };
    if capture_aud.as_ref() == Some(&aud) {
        anyhow::bail!("--cf-access-capture-aud must be another Access app's AUD");
    }
    if !team.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("--cf-access-team is the <team> in <team>.cloudflareaccess.com");
    }
    let url = certs_url(args, &team);
    tracing::info!(%team, "Cloudflare Access: non-local requests need its JWT");
    let mut gate = AccessGate::remote(team, aud, url);
    if let Some(aud) = capture_aud {
        tracing::info!("Cloudflare Access: capture app's JWT accepted on /api/capture");
        gate = gate.with_capture_aud(aud);
    }
    // Unreachable now is not fatal: the first request with a JWT refetches.
    if let Err(e) = gate.refresh().await {
        tracing::warn!("fetching Cloudflare Access keys: {e:#}; retrying on first request");
    }
    Ok(Some(gate))
}

/// Print ready-to-paste worker/CLI commands for every way to reach this orchestrator.
fn print_connect_hints(
    addr: std::net::SocketAddr,
    token: Option<&str>,
    public_url: Option<&str>,
    origins: &[String],
) {
    let mut urls = Vec::new();
    if let Some(u) = public_url {
        urls.push(u.trim_end_matches('/').to_string());
    }
    if addr.ip().is_loopback() {
        urls.push(format!("ws://{addr}"));
    } else {
        let lan = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.is_loopback() && i.ip().is_ipv4())
            .filter(|i| addr.ip().is_unspecified() || i.ip() == addr.ip());
        urls.extend(lan.map(|i| format!("ws://{}:{}", i.ip(), addr.port())));
    }
    let token = token
        .map(|t| format!(" MIS_TOKEN={}", sh_quote(t)))
        .unwrap_or_default();
    eprintln!("\nmis-orchestrator {}", mis_orchestrator::VERSION);
    eprintln!("Connect workers with one of (same MIS_URL/MIS_TOKEN work for `mis`):");
    for u in &urls {
        eprintln!("  MIS_URL={}{token} mis-worker", sh_quote(u));
    }
    if let Some(u) = urls.first() {
        eprintln!("Open in a browser: {}/", mis_types::http_url(u));
    }
    if !origins.is_empty() {
        eprintln!("Browser origins allowed: {}", origins.join(", "));
    }
    eprintln!();
}

/// Single-quote for POSIX shells so tokens like `a$b` or `x y` paste intact.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = Args::parse();
    args.token = args.token.filter(|t| !t.is_empty()); // `MIS_TOKEN=` means no token
    args.read_tokens = (args.read_tokens.iter().map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
        .collect();
    if !args.read_tokens.is_empty() && args.token.is_none() {
        anyhow::bail!("--read-token / MIS_READ_TOKENS needs --token / MIS_TOKEN");
    }
    args.capture_tokens = (args.capture_tokens.iter().map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
        .collect();
    if !args.capture_tokens.is_empty() && args.token.is_none() {
        anyhow::bail!("--capture-token / MIS_CAPTURE_TOKENS needs --token / MIS_TOKEN");
    }
    let shared = |t: &String| args.token.as_ref() == Some(t) || args.read_tokens.contains(t);
    if args.capture_tokens.iter().any(shared) {
        anyhow::bail!("capture tokens must differ from the full and read tokens");
    }
    let state_dir = args.state_dir.clone().or_else(default_state_dir);
    if args.self_test {
        mis_orchestrator::self_test(state_dir.as_deref())?;
        println!("self-test ok");
        return Ok(());
    }
    let addr: std::net::SocketAddr = args.bind.parse()?;
    if !addr.ip().is_loopback() && args.token.is_none() {
        anyhow::bail!("refusing to bind {addr} without --token / MIS_TOKEN");
    }
    let gate = access_gate(&args).await?;
    let apps_gate = apps_access_gate(&args).await?;
    // `MIS_ACCESS_SUBJECTS=` means unset; a list of only blanks (`,`) is a mistake, not "anyone".
    let given =
        args.access_subjects.iter().any(|s| !s.is_empty()) || args.access_subjects.len() > 1;
    args.access_subjects = (args.access_subjects.iter().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if given && args.access_subjects.is_empty() {
        anyhow::bail!("--access-subject / MIS_ACCESS_SUBJECTS lists no one");
    }
    match (&gate, args.access_subjects.is_empty()) {
        (None, false) => anyhow::bail!("--access-subject / MIS_ACCESS_SUBJECTS needs Cloudflare Access"),
        (Some(_), true) => tracing::warn!(
            "Cloudflare Access: anyone its application admits gets full access; list who with --access-subject"
        ),
        _ => {}
    }
    if gate.is_some() && args.token.is_none() {
        tracing::warn!("no --token: local requests without Cloudflare headers need nothing");
    }
    let apps_listener = match nonempty(&args.apps_bind) {
        Some(_) if state_dir.is_none() => anyhow::bail!("--apps-bind needs a state dir"),
        Some(bind) => Some(tokio::net::TcpListener::bind(&bind).await?),
        None => None,
    };
    let apps_addr = apps_listener.as_ref().map(|l| l.local_addr()).transpose()?;
    if let Some(addr) = apps_addr {
        tracing::info!("serving apps on http://{addr}/a/");
        if apps_gate.is_none() {
            tracing::info!("no --apps-cf-access-aud: apps answer direct loopback requests only");
        }
        if args.apps_dev {
            tracing::warn!("--apps-dev: direct loopback requests may act as a viewer or editor");
        }
        if !addr.ip().is_loopback() && nonempty(&args.apps_public_url).is_none() {
            tracing::warn!("no --apps-public-url: app links are left out (only loopback names and its host are answered)");
        }
    }
    let apps_public_host = nonempty(&args.apps_public_url)
        .map(|u| mis_orchestrator::app_access::public_authority(&u))
        .transpose()
        .map_err(|e| anyhow::anyhow!("--apps-public-url {e}"))?;
    let apps = state_dir.as_ref().map(|dir| {
        let base =
            mis_orchestrator::apps::base_url(nonempty(&args.apps_public_url).as_deref(), apps_addr);
        std::sync::Arc::new(mis_orchestrator::apps::Apps::new(dir.join("apps"), base))
    });
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    let local = listener.local_addr()?;
    tracing::info!("listening on {local}");
    let origins = normalize_origins(args.allowed_origins);
    print_connect_hints(
        local,
        args.token.as_deref(),
        args.public_url.as_deref(),
        &origins,
    );
    let channel = channel(args.update_channel, mis_update::CI_BUILD);
    // Resolved before any swap: on Linux a replaced executable's path reads "(deleted)".
    let exe = std::env::current_exe()?.canonicalize()?;
    mis_update::cleanup(&exe);
    let source = mis_update::Source::from_env()?;
    let blocked = mis_update::blocked(
        channel.is_none(),
        mis_orchestrator::VERSION,
        mis_update::CI_BUILD,
        source.custom,
    );
    let updater = match (channel, blocked) {
        (Some(channel), None) => {
            tracing::info!(?channel, "self-update on ({})", source.base);
            Some(mis_orchestrator::self_update::Updater {
                channel,
                source,
                exe: exe.clone(),
                state_dir: state_dir.clone(),
                every: std::time::Duration::from_secs(args.update_every),
            })
        }
        (_, why) => {
            tracing::info!(?channel, "self-update off: {}", why.unwrap_or("dev build"));
            None
        }
    };
    let mut app = App::new(args.token)
        .with_channel(channel)
        .with_read_tokens(args.read_tokens)
        .with_access_subjects(args.access_subjects)
        .with_capture_tokens(args.capture_tokens)
        .with_allowed_origins(origins);
    if let Some(gate) = gate {
        app = app.with_access(gate);
    }
    if let Some(apps) = &apps {
        app = app.with_apps(apps.clone());
        tokio::spawn(mis_orchestrator::backups::run_daily(apps.clone()));
    }
    if let (Some(listener), Some(apps)) = (apps_listener, apps) {
        tokio::spawn(async move {
            let auth = mis_orchestrator::apps_http::Auth {
                gate: apps_gate,
                public_host: apps_public_host,
                dev: args.apps_dev,
            };
            if let Err(e) = mis_orchestrator::serve_apps(listener, apps, auth).await {
                tracing::error!("apps listener stopped: {e:#}");
            }
        });
    }
    match args.routines_dir.clone().or_else(default_routines_dir) {
        Some(dir) => app = app.with_routines(dir, mis_orchestrator::ROUTINE_EVERY),
        None => tracing::warn!("no --routines-dir and no home dir: routines off"),
    }
    match state_dir {
        Some(dir) => {
            app = app.with_ledger_dir(dir.clone());
            match Push::open(dir.clone(), args.vapid_subject.clone()) {
                Ok(push) => {
                    tracing::info!(dir = %dir.display(), "push notifications on");
                    app = app.with_push(push);
                }
                Err(e) => tracing::warn!("push notifications off: {e:#}"),
            }
            match Images::open(dir.join("images")) {
                Ok(images) => app = app.with_images(images),
                Err(e) => tracing::warn!("image attachments off: {e:#}"),
            }
            match evidence::Store::open(dir.join("evidence")) {
                Ok(store) => app = app.with_evidence(store),
                Err(e) => tracing::warn!("evidence files off: {e:#}"),
            }
            match Inbox::open(dir.join("captures").join("inbox")) {
                Ok(inbox) => app = app.with_captures(inbox),
                Err(e) => tracing::warn!("captures off: {e:#}"),
            }
            if let Some(remote) = nonempty(&args.notes_remote) {
                // Not the URL: it may carry a token.
                tracing::info!("publishing captures to the notes repo");
                app = app.with_notes(mis_orchestrator::notes::Notes::new(remote, &dir));
            }
        }
        None if nonempty(&args.notes_remote).is_some() => {
            anyhow::bail!("--notes-remote / MIS_NOTES_REMOTE needs a state dir")
        }
        None => tracing::warn!(
            "no --state-dir and no home dir: task history is lost on restart; \
             push notifications, image attachments, evidence files and captures off"
        ),
    }
    if args.no_planner {
        tracing::info!("planner off; tasks without --harness run on claude");
    } else if mis_agent::which("claude") {
        tracing::info!(model = %args.planner_model, "planner: local claude (tool-less)");
        app = app.with_planner(one_shot_claude(
            &args.planner_model,
            &["--json-schema", mis_orchestrator::plan::SCHEMA],
        ));
        app = app.with_drafter(one_shot_claude(&args.planner_model, &[]));
    } else {
        tracing::warn!("no `claude` on PATH: planner off; tasks without --harness run on claude");
    }
    if !args.no_summary && mis_agent::which("claude") {
        tracing::info!(model = %args.summary_model, "summaries: local claude (tool-less)");
        app = app.with_summarizer(one_shot_claude(&args.summary_model, &[]));
    }
    // Shut down on a signal, or once the updater has swapped in a new build: then restart on it.
    let restarting = std::cell::Cell::new(false);
    let watched = app.clone();
    let updated = async move {
        match updater {
            Some(u) => u.run(watched).await,
            None => std::future::pending().await,
        }
    };
    let stop = async {
        tokio::select! {
            _ = shutdown_signal() => {}
            _ = updated => restarting.set(true),
        }
    };
    mis_orchestrator::serve_until(listener, app, stop).await?;
    if restarting.get() {
        tracing::info!("restarting on the new build");
        return Err(mis_update::restart(&exe));
    }
    Ok(())
}

/// Ctrl-C, or SIGTERM (`kill`, service managers) on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    #[test]
    fn channel_defaults_to_stable_only_for_ci_builds() {
        use super::{channel, UpdateChannel};
        use mis_types::Channel;
        assert_eq!(channel(None, true), Some(Channel::Stable));
        assert_eq!(channel(None, false), None);
        assert_eq!(
            channel(Some(UpdateChannel::Edge), false),
            Some(Channel::Edge)
        );
        assert_eq!(channel(Some(UpdateChannel::Off), true), None);
    }

    #[test]
    fn quotes_shell_metacharacters() {
        assert_eq!(super::sh_quote("abc"), "'abc'");
        assert_eq!(super::sh_quote("a$b c"), "'a$b c'");
        assert_eq!(super::sh_quote("it's"), "'it'\\''s'");
    }
}

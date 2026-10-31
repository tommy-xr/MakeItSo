use clap::Parser;
use mis_orchestrator::access_gate::AccessGate;
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
    /// Externally reachable URL (e.g. a tunnel hostname), shown in connection hints.
    #[arg(long, env = "MIS_PUBLIC_URL")]
    public_url: Option<String>,
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
    /// Fetch Access keys from here instead of the team's certs URL (testing).
    #[arg(long, hide = true)]
    cf_access_certs_url: Option<String>,
    /// Holds task history (`ledger.json`, `transcripts/`; tasks are kept 30 days
    /// after they end), the push notification key and subscriptions, and uploaded
    /// images (`images/`, deleted after a week). Default: ~/.local/state/mis
    /// (%LOCALAPPDATA%\mis on Windows).
    #[arg(long, env = "MIS_STATE_DIR")]
    state_dir: Option<std::path::PathBuf>,
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

/// `$XDG_STATE_HOME/mis`, else `~/.local/state/mis`; `%LOCALAPPDATA%\mis` on Windows.
fn default_state_dir() -> Option<std::path::PathBuf> {
    let var = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    };
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")?
    } else {
        var("XDG_STATE_HOME").or_else(|| Some(var("HOME")?.join(".local/state")))?
    };
    Some(base.join("mis"))
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

/// `--cf-access-team` / `--cf-access-aud`: both or neither; `MIS_..=` means unset.
async fn access_gate(args: &Args) -> anyhow::Result<Option<AccessGate>> {
    let nonempty = |v: &Option<String>| v.clone().filter(|v| !v.is_empty());
    let (team, aud) = match (
        nonempty(&args.cf_access_team),
        nonempty(&args.cf_access_aud),
    ) {
        (Some(team), Some(aud)) => (team, aud),
        (None, None) => return Ok(None),
        _ => anyhow::bail!("--cf-access-team and --cf-access-aud go together"),
    };
    if !team.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("--cf-access-team is the <team> in <team>.cloudflareaccess.com");
    }
    let url = nonempty(&args.cf_access_certs_url).unwrap_or_else(|| access::certs_url(&team));
    tracing::info!(%team, "Cloudflare Access: non-local requests need its JWT");
    let gate = AccessGate::remote(team, aud, url);
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
    if gate.is_some() && args.token.is_none() {
        tracing::warn!("no --token: local requests without Cloudflare headers need nothing");
    }
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
        .with_allowed_origins(origins);
    if let Some(gate) = gate {
        app = app.with_access(gate);
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
        }
        None => tracing::warn!(
            "no --state-dir and no home dir: task history is lost on restart; \
             push notifications and image attachments off"
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

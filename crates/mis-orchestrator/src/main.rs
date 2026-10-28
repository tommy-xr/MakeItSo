use clap::Parser;
use mis_orchestrator::access_gate::AccessGate;
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
        skip_permissions: false,
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
    let mut app = App::new(args.token)
        .with_read_tokens(args.read_tokens)
        .with_allowed_origins(origins);
    if let Some(gate) = gate {
        app = app.with_access(gate);
    }
    if args.no_planner {
        tracing::info!("planner off; tasks without --harness run on claude");
    } else if mis_agent::which("claude") {
        tracing::info!(model = %args.planner_model, "planner: local claude (tool-less)");
        app = app.with_planner(one_shot_claude(
            &args.planner_model,
            &["--json-schema", mis_orchestrator::plan::SCHEMA],
        ));
    } else {
        tracing::warn!("no `claude` on PATH: planner off; tasks without --harness run on claude");
    }
    if !args.no_summary && mis_agent::which("claude") {
        tracing::info!(model = %args.summary_model, "summaries: local claude (tool-less)");
        app = app.with_summarizer(one_shot_claude(&args.summary_model, &[]));
    }
    mis_orchestrator::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn quotes_shell_metacharacters() {
        assert_eq!(super::sh_quote("abc"), "'abc'");
        assert_eq!(super::sh_quote("a$b c"), "'a$b c'");
        assert_eq!(super::sh_quote("it's"), "'it'\\''s'");
    }
}

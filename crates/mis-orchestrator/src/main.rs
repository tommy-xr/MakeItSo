use clap::Parser;
use mis_orchestrator::App;

#[derive(Parser)]
#[command(about = "Make It So orchestrator")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7474")]
    bind: String,
    /// Shared secret clients and workers must send as a bearer token.
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    /// Externally reachable URL (e.g. a tunnel hostname), shown in connection hints.
    #[arg(long, env = "MIS_PUBLIC_URL")]
    public_url: Option<String>,
    /// Don't plan with the local `claude` CLI; tasks without --harness run on claude as-is.
    #[arg(long)]
    no_planner: bool,
}

/// Print ready-to-paste worker/CLI commands for every way to reach this orchestrator.
fn print_connect_hints(addr: std::net::SocketAddr, token: Option<&str>, public_url: Option<&str>) {
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
    eprintln!("\nConnect workers with one of (same MIS_URL/MIS_TOKEN work for `mis`):");
    for u in urls {
        eprintln!("  MIS_URL={}{token} mis-worker", sh_quote(&u));
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
    let addr: std::net::SocketAddr = args.bind.parse()?;
    if !addr.ip().is_loopback() && args.token.is_none() {
        anyhow::bail!("refusing to bind {addr} without --token / MIS_TOKEN");
    }
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    let local = listener.local_addr()?;
    tracing::info!("listening on {local}");
    print_connect_hints(local, args.token.as_deref(), args.public_url.as_deref());
    let mut app = App::new(args.token);
    if args.no_planner {
        tracing::info!("planner off; tasks without --harness run on claude");
    } else if mis_agent::which("claude") {
        tracing::info!("planner: local claude (tool-less)");
        app = app.with_planner(std::sync::Arc::new(mis_agent::claude::ClaudeAgent {
            skip_permissions: false,
            // No user/project settings (hooks, CLAUDE.md), MCP servers, or tools; one
            // turn. (`--bare` would also drop the login.)
            extra_args: [
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--tools",
                "",
                "--max-turns",
                "1",
            ]
            .map(String::from)
            .to_vec(),
        }));
    } else {
        tracing::warn!("no `claude` on PATH: planner off; tasks without --harness run on claude");
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

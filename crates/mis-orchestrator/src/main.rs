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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    ).init();
    let mut args = Args::parse();
    args.token = args.token.filter(|t| !t.is_empty()); // `MIS_TOKEN=` means no token
    let addr: std::net::SocketAddr = args.bind.parse()?;
    if !addr.ip().is_loopback() && args.token.is_none() {
        anyhow::bail!("refusing to bind {addr} without --token / MIS_TOKEN");
    }
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!("listening on {}", listener.local_addr()?);
    mis_orchestrator::serve(listener, App::new(args.token)).await
}

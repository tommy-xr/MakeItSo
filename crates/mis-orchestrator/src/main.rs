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
    let args = Args::parse();
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!("listening on {}", listener.local_addr()?);
    mis_orchestrator::serve(listener, App::new(args.token)).await
}

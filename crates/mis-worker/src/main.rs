//! Worker: connects to the orchestrator, advertises its harnesses, runs
//! assigned tasks through `mis_agent` and streams events back.

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use mis_agent::Agent;
use mis_types::*;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(about = "Make It So worker")]
struct Args {
    #[arg(long, env = "MIS_URL", default_value = "ws://127.0.0.1:7474")]
    url: String,
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    /// Let agents run tools without permission prompts (required for real work).
    #[arg(long)]
    skip_permissions: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    ).init();
    let args = Args::parse();

    let mut agents = mis_agent::available();
    for a in agents.iter_mut() {
        if a.name() == "claude" {
            *a = Box::new(mis_agent::claude::ClaudeAgent { skip_permissions: args.skip_permissions });
        }
    }
    let info = WorkerInfo {
        hostname: hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        harnesses: agents.iter().map(|a| a.name().to_string()).collect(),
    };
    tracing::info!(?info.harnesses, "connecting to {}", args.url);

    let mut req = format!("{}/ws/worker", args.url).into_client_request()?;
    if let Some(t) = &args.token {
        req.headers_mut().insert("authorization", format!("Bearer {t}").parse()?);
    }
    let (ws, _) = connect_async(req).await.with_context(|| format!("connecting to {}", args.url))?;
    let (mut sink, mut stream) = ws.split();
    sink.send(text(&WorkerMsg::Hello { info })).await?;

    // Agent events from the running task are forwarded on this channel.
    let (ev_tx, mut ev_rx) = mpsc::channel::<(TaskId, AgentEvent)>(64);
    let agents = std::sync::Arc::new(agents);
    loop {
        tokio::select! {
            Some((task, event)) = ev_rx.recv() => {
                sink.send(text(&WorkerMsg::Event { task, event })).await?;
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<OrchMsg>(&t)? {
                    OrchMsg::Welcome { id } => tracing::info!(%id, "registered"),
                    OrchMsg::Assign { task } => {
                        tracing::info!(id = %task.id, harness = %task.harness, "assigned: {}", task.prompt);
                        let agents = agents.clone();
                        let tx = ev_tx.clone();
                        tokio::spawn(async move { run_task(&agents, task, tx).await });
                    }
                },
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => { tracing::info!("orchestrator closed connection"); return Ok(()); }
            }
        }
    }
}

async fn run_task(agents: &[Box<dyn Agent>], task: Task, tx: mpsc::Sender<(TaskId, AgentEvent)>) {
    let id = task.id;
    let Some(agent) = agents.iter().find(|a| a.name() == task.harness) else {
        let _ = tx.send((id, AgentEvent::Finished { ok: false, summary: Some(format!("no harness {}", task.harness)) })).await;
        return;
    };
    let (atx, mut arx) = mpsc::channel(64);
    let fwd = async {
        while let Some(ev) = arx.recv().await {
            if tx.send((id, ev)).await.is_err() { break; }
        }
    };
    let (res, _) = tokio::join!(agent.run(&task.prompt, task.cwd.as_deref(), atx), fwd);
    if let Err(e) = res {
        tracing::error!(%id, "agent failed: {e:#}");
        let _ = tx.send((id, AgentEvent::Finished { ok: false, summary: Some(e.to_string()) })).await;
    }
}

fn text<T: serde::Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap().into())
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

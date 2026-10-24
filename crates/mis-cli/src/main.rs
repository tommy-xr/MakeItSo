//! Text client for the orchestrator. Stands in for the Voice app.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[derive(Parser)]
#[command(name = "mis", about = "Make It So CLI")]
struct Args {
    #[arg(long, env = "MIS_URL", default_value = "ws://127.0.0.1:7474")]
    url: String,
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List workers and tasks.
    Status,
    /// Submit a task.
    Task {
        prompt: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        /// Stream this task's events until it finishes.
        #[arg(long, short)]
        follow: bool,
    },
    /// Stream all task events.
    Tail,
}

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(args: &Args) -> Result<Ws> {
    let mut req = format!("{}/ws/client", args.url).into_client_request()?;
    if let Some(t) = &args.token {
        req.headers_mut().insert("authorization", format!("Bearer {t}").parse()?);
    }
    let (ws, _) = connect_async(req).await.with_context(|| format!("connecting to {}", args.url))?;
    Ok(ws)
}

async fn send(ws: &mut Ws, m: &ClientMsg) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(m)?.into())).await?;
    Ok(())
}

async fn recv(ws: &mut Ws) -> Result<ServerMsg> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => bail!("connection closed"),
        }
    }
}

fn print_event(task: TaskId, event: &AgentEvent) {
    let short = &task.to_string()[..8];
    match event {
        AgentEvent::Started { .. } => println!("[{short}] started"),
        AgentEvent::Text { text } => println!("[{short}] {text}"),
        AgentEvent::ToolUse { name, input } => println!("[{short}] > {name} {input}"),
        AgentEvent::ToolResult { content } => {
            let first = content.lines().next().unwrap_or("");
            println!("[{short}] < {first}");
        }
        AgentEvent::Finished { ok, summary } => {
            println!("[{short}] {} {}", if *ok { "done" } else { "FAILED" }, summary.as_deref().unwrap_or(""))
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut ws = connect(&args).await?;
    match &args.cmd {
        Cmd::Status => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status { workers, tasks } = recv(&mut ws).await? else { bail!("unexpected reply") };
            println!("workers ({}):", workers.len());
            for w in workers {
                println!("  {} {} {} {:?}{}", w.id, w.info.hostname, w.info.os, w.info.harnesses, if w.busy { " [busy]" } else { "" });
            }
            println!("tasks ({}):", tasks.len());
            for t in tasks {
                println!("  {} {:<8} {}", &t.id.to_string()[..8], format!("{:?}", t.status).to_lowercase(), t.prompt);
            }
        }
        Cmd::Task { prompt, cwd, harness, follow } => {
            if *follow {
                send(&mut ws, &ClientMsg::Subscribe).await?;
            }
            send(&mut ws, &ClientMsg::Submit { prompt: prompt.clone(), cwd: cwd.clone(), harness: harness.clone() }).await?;
            let ServerMsg::Submitted { task } = recv(&mut ws).await? else { bail!("unexpected reply") };
            println!("submitted {task}");
            if *follow {
                loop {
                    if let ServerMsg::Event { task: t, event, .. } = recv(&mut ws).await? {
                        if t != task { continue; }
                        print_event(t, &event);
                        if matches!(event, AgentEvent::Finished { .. }) { break; }
                    }
                }
            }
        }
        Cmd::Tail => {
            send(&mut ws, &ClientMsg::Subscribe).await?;
            loop {
                if let ServerMsg::Event { task, event, .. } = recv(&mut ws).await? {
                    print_event(task, &event);
                }
            }
        }
    }
    Ok(())
}

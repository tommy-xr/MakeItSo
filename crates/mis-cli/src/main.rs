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
    /// Run an orchestrator and a worker on this machine for quick testing.
    Dev {
        /// Let agents run tools without permission prompts.
        #[arg(long)]
        skip_permissions: bool,
    },
}

/// Spawns the sibling `mis-orchestrator` and `mis-worker` binaries, binding the
/// orchestrator to the host in `url`. MIS_TOKEN is inherited by both. Both are
/// killed when either exits or this process gets Ctrl-C / SIGTERM.
async fn dev(url: &str, skip_permissions: bool) -> Result<()> {
    let bin = |name: &str| -> Result<std::path::PathBuf> {
        let exe = std::env::current_exe()?;
        Ok(exe.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
    };
    let bind = url
        .trim_start_matches("ws://")
        .trim_end_matches('/')
        .to_string();
    let mut orch = tokio::process::Command::new(bin("mis-orchestrator")?)
        .args(["--bind", &bind])
        .kill_on_drop(true)
        .spawn()
        .context("starting mis-orchestrator")?;
    let mut worker = tokio::process::Command::new(bin("mis-worker")?);
    worker.args(["--url", url]).kill_on_drop(true);
    if skip_permissions {
        worker.arg("--skip-permissions");
    }
    let mut worker = worker.spawn().context("starting mis-worker")?;
    tokio::select! {
        s = orch.wait() => bail!("mis-orchestrator exited: {}", s?),
        s = worker.wait() => bail!("mis-worker exited: {}", s?),
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate() => Ok(()),
    }
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{signal, SignalKind};
    let _ = signal(SignalKind::terminate()).unwrap().recv().await;
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending().await
}

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(args: &Args) -> Result<Ws> {
    let mut req = format!("{}/ws/client", args.url).into_client_request()?;
    if let Some(t) = &args.token {
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse()?);
    }
    let (ws, _) = connect_async(req)
        .await
        .with_context(|| format!("connecting to {}", args.url))?;
    Ok(ws)
}

async fn send(ws: &mut Ws, m: &ClientMsg) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(m)?.into()))
        .await?;
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
            println!(
                "[{short}] {} {}",
                if *ok { "done" } else { "FAILED" },
                summary.as_deref().unwrap_or("")
            )
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Cmd::Dev { skip_permissions } = args.cmd {
        return dev(&args.url, skip_permissions).await;
    }
    let mut ws = connect(&args).await?;
    match &args.cmd {
        Cmd::Status => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status { workers, tasks } = recv(&mut ws).await? else {
                bail!("unexpected reply")
            };
            println!("workers ({}):", workers.len());
            for w in workers {
                println!(
                    "  {} {} {} {:?}{}",
                    w.id,
                    w.info.hostname,
                    w.info.os,
                    w.info.harnesses,
                    if !w.online {
                        " [offline]"
                    } else if w.busy {
                        " [busy]"
                    } else {
                        ""
                    }
                );
            }
            println!("tasks ({}):", tasks.len());
            for t in tasks {
                println!(
                    "  {} {:<8} {}",
                    &t.id.to_string()[..8],
                    format!("{:?}", t.status).to_lowercase(),
                    t.prompt
                );
            }
        }
        Cmd::Task {
            prompt,
            cwd,
            harness,
            follow,
        } => {
            if *follow {
                send(&mut ws, &ClientMsg::Subscribe).await?;
            }
            send(
                &mut ws,
                &ClientMsg::Submit {
                    prompt: prompt.clone(),
                    cwd: cwd.clone(),
                    harness: harness.clone(),
                },
            )
            .await?;
            // Other tasks' events may interleave once subscribed; skip them until Submitted.
            let task = loop {
                match recv(&mut ws).await? {
                    ServerMsg::Submitted { task } => break task,
                    ServerMsg::Event { .. } => continue,
                    other => bail!("unexpected reply: {other:?}"),
                }
            };
            println!("submitted {task}");
            if *follow {
                loop {
                    match recv(&mut ws).await? {
                        ServerMsg::Event { task: t, event, .. } if t == task => {
                            print_event(t, &event);
                            if let AgentEvent::Finished { ok, .. } = event {
                                if !ok {
                                    std::process::exit(1);
                                }
                                break;
                            }
                        }
                        ServerMsg::Error { message } => eprintln!("warning: {message}"),
                        _ => {}
                    }
                }
            }
        }
        Cmd::Tail => {
            send(&mut ws, &ClientMsg::Subscribe).await?;
            loop {
                match recv(&mut ws).await? {
                    ServerMsg::Event { task, event, .. } => print_event(task, &event),
                    ServerMsg::Error { message } => eprintln!("warning: {message}"),
                    _ => {}
                }
            }
        }
        Cmd::Dev { .. } => unreachable!(),
    }
    Ok(())
}

//! Worker: connects to the orchestrator, advertises its harnesses, runs
//! assigned tasks through `mis_agent` and streams events back.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use mis_agent::Agent;
use mis_types::*;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(about = "Make It So worker")]
struct Args {
    #[arg(long, env = "MIS_URL", default_value = "ws://127.0.0.1:7474")]
    url: String,
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    /// Cloudflare Access service-token id; needs --cf-access-client-secret.
    #[arg(long, env = "CF_ACCESS_CLIENT_ID")]
    cf_access_client_id: Option<String>,
    #[arg(long, env = "CF_ACCESS_CLIENT_SECRET", hide_env_values = true)]
    cf_access_client_secret: Option<String>,
    /// Let agents run tools without permission prompts (required for real work).
    #[arg(long)]
    skip_permissions: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    // Validate before the reconnect loop so bad credentials fail fast.
    let access = access_headers(
        args.cf_access_client_id.as_deref(),
        args.cf_access_client_secret.as_deref(),
    )
    .map_err(anyhow::Error::msg)?
    .into_iter()
    .map(|(k, v)| Ok((k, v.parse().with_context(|| format!("invalid {k}"))?)))
    .collect::<Result<Vec<(&str, HeaderValue)>>>()?;

    let (id, _id_lock) = worker_id()?;
    let agents = mis_agent::available(args.skip_permissions);
    if agents.is_empty() {
        anyhow::bail!("no agent harness found on PATH (need `claude` or `codex`)");
    }
    let info = WorkerInfo {
        hostname: hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        harnesses: agents.iter().map(|a| a.name().to_string()).collect(),
    };
    tracing::info!(?info.harnesses, "connecting to {}", args.url);
    let agents = std::sync::Arc::new(agents);
    // Events from in-flight tasks queue here while disconnected. Delivery is best
    // effort: an event already written to a socket that then dies is not resent.
    let (ev_tx, ev_rx) = mpsc::channel::<(TaskId, AgentEvent)>(1024);
    let mut conn = Conn {
        ev_rx,
        pending: None,
        running: HashSet::new(),
        backoff: Duration::from_secs(1),
    };

    loop {
        match session(&args, &access, id, &info, &agents, &ev_tx, &mut conn).await {
            Ok(()) => tracing::info!("orchestrator closed connection"),
            Err(e) => tracing::warn!("connection lost: {e:#}"),
        }
        tracing::info!("reconnecting in {:?}", conn.backoff);
        tokio::time::sleep(conn.backoff).await;
        conn.backoff = (conn.backoff * 2).min(Duration::from_secs(30));
    }
}

/// State that outlives a single connection.
struct Conn {
    ev_rx: mpsc::Receiver<(TaskId, AgentEvent)>,
    /// Event taken from `ev_rx` whose send failed; goes first on the next session.
    pending: Option<(TaskId, AgentEvent)>,
    /// Tasks assigned and not yet reported Finished; sent in Hello on reconnect.
    running: HashSet<TaskId>,
    backoff: Duration,
}

/// One connection's lifetime: handshake, then forward events and run assignments
/// until the socket drops. Returns Ok on a clean close from the orchestrator.
async fn session(
    args: &Args,
    access: &[(&'static str, HeaderValue)],
    id: WorkerId,
    info: &WorkerInfo,
    agents: &std::sync::Arc<Vec<Box<dyn Agent>>>,
    ev_tx: &mpsc::Sender<(TaskId, AgentEvent)>,
    c: &mut Conn,
) -> Result<()> {
    let mut req = format!("{}/ws/worker", args.url).into_client_request()?;
    if let Some(t) = &args.token {
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse()?);
    }
    for (k, v) in access {
        req.headers_mut().insert(*k, v.clone());
    }
    let (ws, _) = connect_async(req)
        .await
        .with_context(|| format!("connecting to {}", args.url))?;
    let (mut sink, mut stream) = ws.split();
    let running: Vec<TaskId> = c.running.iter().copied().collect();
    sink.send(text(&WorkerMsg::Hello {
        id,
        info: info.clone(),
        running,
    }))
    .await?;

    loop {
        // Resend a stranded event before pulling a new one.
        let next = match c.pending.take() {
            Some(ev) => Some(ev),
            None => tokio::select! {
                ev = c.ev_rx.recv() => ev,
                msg = stream.next() => {
                    handle_incoming(msg, agents, ev_tx, &mut c.running, &mut c.backoff)?;
                    continue;
                }
            },
        };
        let Some((task, event)) = next else {
            return Ok(());
        };
        let finished = matches!(event, AgentEvent::Finished { .. });
        if let Err(e) = sink
            .send(text(&WorkerMsg::Event {
                task,
                event: event.clone(),
            }))
            .await
        {
            c.pending = Some((task, event));
            return Err(e.into());
        }
        if finished {
            c.running.remove(&task);
        }
    }
}

type Incoming = Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>;

/// Returns Ok on a message handled or ignored; Err when the socket is gone.
fn handle_incoming(
    msg: Incoming,
    agents: &std::sync::Arc<Vec<Box<dyn Agent>>>,
    ev_tx: &mpsc::Sender<(TaskId, AgentEvent)>,
    running: &mut HashSet<TaskId>,
    backoff: &mut Duration,
) -> Result<()> {
    match msg {
        Some(Ok(Message::Text(t))) => match serde_json::from_str::<OrchMsg>(&t) {
            Ok(OrchMsg::Welcome { id }) => {
                tracing::info!(%id, "registered");
                *backoff = Duration::from_secs(1);
            }
            Ok(OrchMsg::Assign { task }) => {
                if !running.insert(task.id) {
                    tracing::warn!(id = %task.id, "already running; ignoring duplicate assign");
                    return Ok(());
                }
                tracing::info!(id = %task.id, "assigned to {}: {}", task.harness, task.prompt);
                let agents = agents.clone();
                let tx = ev_tx.clone();
                tokio::spawn(async move { run_task(&agents, task, tx).await });
            }
            Err(e) => tracing::warn!("bad message from orchestrator: {e}"),
        },
        Some(Ok(_)) => {}
        Some(Err(e)) => return Err(e.into()),
        None => anyhow::bail!("orchestrator closed connection"),
    }
    Ok(())
}

async fn run_task(agents: &[Box<dyn Agent>], task: Task, tx: mpsc::Sender<(TaskId, AgentEvent)>) {
    let id = task.id;
    let fail = |summary: String| {
        (
            id,
            AgentEvent::Finished {
                ok: false,
                summary: Some(summary),
            },
        )
    };
    let Some(agent) = agents.iter().find(|a| a.name() == task.harness) else {
        let _ = tx.send(fail(format!("no harness {}", task.harness))).await;
        return;
    };
    let (atx, mut arx) = mpsc::channel(64);
    let fwd = async {
        while let Some(ev) = arx.recv().await {
            if tx.send((id, ev)).await.is_err() {
                break;
            }
        }
    };
    // A panic inside the agent must still produce a Finished, or the task stays
    // "running" in every future Hello.
    let run = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(agent.run(
        &task.prompt,
        task.cwd.as_deref(),
        atx,
    )));
    let (res, _) = tokio::join!(run, fwd);
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(%id, "agent failed: {e:#}");
            let _ = tx.send(fail(e.to_string())).await;
        }
        Err(_) => {
            tracing::error!(%id, "agent panicked");
            let _ = tx.send(fail("worker panicked".into())).await;
        }
    }
}

fn text<T: serde::Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap().into())
}

/// Stable identity across restarts, persisted in `$MIS_STATE_DIR/worker-id`
/// (default `~/.config/mis`). Holds an exclusive lock on the file for the life of
/// the process so two workers can't share one id.
fn worker_id() -> Result<(WorkerId, std::fs::File)> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let dir = std::env::var_os("MIS_STATE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| home.map(|h| std::path::PathBuf::from(h).join(".config/mis")))
        .context("neither MIS_STATE_DIR nor HOME is set")?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("worker-id");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "another worker is using {}; set MIS_STATE_DIR to run a second one",
            path.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    let mut s = String::new();
    std::io::Read::read_to_string(&mut file, &mut s)?;
    let id = if s.trim().is_empty() {
        let id = WorkerId::new_v4();
        std::io::Write::write_all(&mut file, id.to_string().as_bytes())?;
        file.sync_all()?;
        tracing::info!(%id, "created worker id at {}", path.display());
        id
    } else {
        s.trim()
            .parse()
            .with_context(|| format!("bad worker id in {}", path.display()))?
    };
    Ok((id, file))
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

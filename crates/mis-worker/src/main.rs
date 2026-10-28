//! Worker: connects to the orchestrator, advertises its harnesses, runs
//! assigned tasks through `mis_agent` and streams events back.

mod config;
mod mcp;
mod roots;
mod setup;
mod stats;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use mis_agent::Agent;
use mis_types::*;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(
    about = "Make It So worker",
    args_conflicts_with_subcommands = true,
    after_help = "Unset flags fall back to $MIS_STATE_DIR/worker.toml (default ~/.config/mis)."
)]
struct Args {
    /// Default: ws://127.0.0.1:7474.
    #[arg(long, env = "MIS_URL")]
    url: Option<String>,
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    /// Cloudflare Access service-token id; needs --cf-access-client-secret.
    #[arg(long, env = "CF_ACCESS_CLIENT_ID")]
    cf_access_client_id: Option<String>,
    #[arg(long, env = "CF_ACCESS_CLIENT_SECRET", hide_env_values = true)]
    cf_access_client_secret: Option<String>,
    /// Directory tasks may run in (repeatable; MIS_ROOTS is comma/`:`-separated).
    /// Default: the current directory.
    #[arg(long = "root", env = "MIS_ROOTS")]
    roots: Vec<String>,
    /// Let agents run tools without permission prompts (required for real work).
    #[arg(long)]
    skip_permissions: bool,
    /// None runs the worker.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ask for connection settings, test them, and save worker.toml.
    Setup(setup::SetupArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    if let Some(Cmd::Setup(a)) = args.cmd {
        return setup::run(a).await;
    }
    let path = state_dir()?.join("worker.toml");
    let cli = config::Config {
        url: args.url,
        token: args.token,
        cf_access_client_id: args.cf_access_client_id,
        cf_access_client_secret: args.cf_access_client_secret,
        roots: args
            .roots
            .iter()
            .flat_map(|r| roots::split_roots(r))
            .collect(),
        models: Default::default(),
    };
    let cfg = config::merge(cli, config::load(&path)?, std::env::current_dir()?);
    // Validate before the reconnect loop so bad credentials fail fast.
    let access = access_values(&cfg)?;
    let roots = roots::canonical_roots(&cfg.roots)?;

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
        roots: roots.iter().map(|r| r.display().to_string()).collect(),
        models: advertised(&agents, &cfg.models),
    };
    tracing::info!(?info.harnesses, ?info.models, ?info.roots, "connecting to {}", cfg.url);
    let ctx = Ctx {
        agents: Arc::new(agents),
        roots: Arc::new(roots),
        limits: Default::default(),
    };
    // Events from in-flight tasks queue here while disconnected. Delivery is best
    // effort: an event already written to a socket that then dies is not resent.
    let (ev_tx, ev_rx) = mpsc::channel::<(TaskId, AgentEvent)>(1024);
    let mut conn = Conn {
        ev_rx,
        pending: None,
        running: HashSet::new(),
        backoff: Duration::from_secs(1),
        sampler: stats::Sampler::new(ctx.roots.first().cloned().unwrap_or_else(|| "/".into())),
    };

    loop {
        match session(&cfg, &access, id, &info, &ctx, &ev_tx, &mut conn).await {
            Ok(()) => tracing::info!("orchestrator closed connection"),
            Err(e) => tracing::warn!("connection lost: {e:#}"),
        }
        tracing::info!("reconnecting in {:?}", conn.backoff);
        tokio::time::sleep(conn.backoff).await;
        conn.backoff = (conn.backoff * 2).min(Duration::from_secs(30));
    }
}

/// Each agent's models, from discovery unless worker.toml's `[models]` sets
/// them; harnesses with none are left out (they run their default).
fn advertised(
    agents: &[Box<dyn Agent>],
    overrides: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    agents
        .iter()
        .map(|a| {
            let models = overrides.get(a.name()).cloned();
            (a.name().to_string(), models.unwrap_or_else(|| a.models()))
        })
        .filter(|(_, m)| !m.is_empty())
        .collect()
}

/// What a task needs from the worker: harnesses and allowed directories, and
/// where its runs leave usage-limit readings for the stats beat.
#[derive(Clone)]
struct Ctx {
    agents: Arc<Vec<Box<dyn Agent>>>,
    roots: Arc<Vec<PathBuf>>,
    /// Latest limit windows per harness, from runs and polling.
    limits: Arc<Mutex<BTreeMap<String, Vec<LimitWindow>>>>,
}

impl Ctx {
    fn add_limits(&self, harness: &str, windows: Vec<LimitWindow>) {
        let mut all = self.limits.lock().unwrap();
        merge_limits(all.entry(harness.to_string()).or_default(), windows);
    }

    /// Poll the harnesses that keep their own readings, then every harness's windows.
    fn poll_limits(&self) -> BTreeMap<String, Vec<LimitWindow>> {
        for a in self.agents.iter() {
            if let Some(w) = a.limits() {
                self.add_limits(a.name(), w);
            }
        }
        self.limits.lock().unwrap().clone()
    }
}

fn unix_ms() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.map_or(0, |d| d.as_millis() as u64)
}

/// State that outlives a single connection.
struct Conn {
    ev_rx: mpsc::Receiver<(TaskId, AgentEvent)>,
    /// Event taken from `ev_rx` whose send failed; goes first on the next session.
    pending: Option<(TaskId, AgentEvent)>,
    /// Tasks assigned and not yet reported Finished; sent in Hello on reconnect.
    running: HashSet<TaskId>,
    backoff: Duration,
    sampler: stats::Sampler,
}

/// One connection's lifetime: handshake, then forward events and run assignments
/// until the socket drops. Returns Ok on a clean close from the orchestrator.
async fn session(
    cfg: &config::Resolved,
    access: &[(&'static str, HeaderValue)],
    id: WorkerId,
    info: &WorkerInfo,
    ctx: &Ctx,
    ev_tx: &mpsc::Sender<(TaskId, AgentEvent)>,
    c: &mut Conn,
) -> Result<()> {
    let (ws, _) = connect_async(worker_request(cfg, access)?)
        .await
        .with_context(|| format!("connecting to {}", cfg.url))?;
    let (mut sink, mut stream) = ws.split();
    let running: Vec<TaskId> = c.running.iter().copied().collect();
    sink.send(text(&WorkerMsg::Hello {
        id,
        info: info.clone(),
        running,
    }))
    .await?;
    // Heartbeat with stats. The first tick fires at once, so the orchestrator
    // has numbers right after Hello. Sampling (~20ms) runs inline; at one per
    // 15s that stall is negligible.
    let mut beat = tokio::time::interval(STATS_INTERVAL);
    // After a stall (e.g. laptop sleep), send one beat, not a burst.
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // Resend a stranded event before pulling a new one.
        let next = match c.pending.take() {
            Some(ev) => Some(ev),
            None => tokio::select! {
                ev = c.ev_rx.recv() => ev,
                msg = stream.next() => {
                    handle_incoming(msg, ctx, ev_tx, &mut c.running, &mut c.backoff)?;
                    continue;
                }
                _ = beat.tick() => {
                    let t = std::time::Instant::now();
                    let mut s = c.sampler.sample();
                    // File reads: off the runtime's threads.
                    let poll = ctx.clone();
                    s.limits = tokio::task::spawn_blocking(move || poll.poll_limits())
                        .await
                        .unwrap_or_default();
                    tracing::debug!(?s, took = ?t.elapsed(), "sending stats");
                    sink.send(text(&WorkerMsg::Stats(s))).await?;
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
    ctx: &Ctx,
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
                let ctx = ctx.clone();
                let tx = ev_tx.clone();
                tokio::spawn(async move { run_task(&ctx, task, tx).await });
            }
            Err(e) => tracing::warn!("bad message from orchestrator: {e}"),
        },
        Some(Ok(_)) => {}
        Some(Err(e)) => return Err(e.into()),
        None => anyhow::bail!("orchestrator closed connection"),
    }
    Ok(())
}

async fn run_task(ctx: &Ctx, task: Task, tx: mpsc::Sender<(TaskId, AgentEvent)>) {
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
    let Some(agent) = ctx.agents.iter().find(|a| a.name() == task.harness) else {
        let _ = tx.send(fail(format!("no harness {}", task.harness))).await;
        return;
    };
    let cwd = match roots::resolve_cwd(&ctx.roots, task.cwd.as_deref()) {
        Ok(c) => c.display().to_string(),
        Err(why) => {
            tracing::warn!(%id, "{why}");
            let _ = tx.send(fail(why)).await;
            return;
        }
    };
    // Without the server the task still runs, just without its reporting tools.
    let server = mcp::serve(id, tx.clone())
        .await
        .inspect_err(|e| tracing::warn!(%id, "no mcp server: {e}"))
        .ok();
    let prompt = match &server {
        Some(_) => mcp::with_hint(&task.prompt),
        None => task.prompt.clone(),
    };
    let (atx, mut arx) = mpsc::channel(64);
    let fwd = async {
        while let Some(ev) = arx.recv().await {
            // Limit readings go into the stats beat, not the transcript.
            if let AgentEvent::Limits { mut windows } = ev {
                let now = unix_ms();
                windows.iter_mut().for_each(|w| w.observed_at = now);
                ctx.add_limits(agent.name(), windows);
                continue;
            }
            if tx.send((id, ev)).await.is_err() {
                break;
            }
        }
    };
    let run = mis_agent::run_guarded(
        agent.as_ref(),
        id,
        &prompt,
        mis_agent::RunOpts {
            cwd: Some(&cwd),
            model: task.model.as_deref(),
            mcp: server.as_ref().map(|s| &s.mcp),
            resume: task.resume.as_deref(),
        },
        atx,
    );
    tokio::join!(run, fwd);
}

/// Cloudflare Access headers from `cfg`, checked so bad values fail early.
fn access_values(cfg: &config::Resolved) -> Result<Vec<(&'static str, HeaderValue)>> {
    access_headers(
        cfg.cf_access_client_id.as_deref(),
        cfg.cf_access_client_secret.as_deref(),
    )
    .map_err(anyhow::Error::msg)?
    .into_iter()
    .map(|(k, v)| Ok((k, v.parse().with_context(|| format!("invalid {k}"))?)))
    .collect()
}

/// `/ws/worker` upgrade request carrying the token and Access headers.
fn worker_request(
    cfg: &config::Resolved,
    access: &[(&'static str, HeaderValue)],
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut req = format!("{}/ws/worker", cfg.url).into_client_request()?;
    if let Some(t) = &cfg.token {
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse()?);
    }
    for (k, v) in access {
        req.headers_mut().insert(*k, v.clone());
    }
    Ok(req)
}

fn text<T: serde::Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap().into())
}

/// `$MIS_STATE_DIR`, default `~/.config/mis`: holds `worker-id` and `worker.toml`.
fn state_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    std::env::var_os("MIS_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config/mis")))
        .context("neither MIS_STATE_DIR nor HOME is set")
}

/// Stable identity across restarts, persisted in `<state_dir>/worker-id`.
/// Holds an exclusive lock on the file for the life of the process so two
/// workers can't share one id.
fn worker_id() -> Result<(WorkerId, std::fs::File)> {
    let dir = state_dir()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str);
    impl Agent for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn models(&self) -> Vec<String> {
            vec![format!("{}-found", self.0)]
        }
        fn run<'a>(
            &'a self,
            _: TaskId,
            _: &'a str,
            _: mis_agent::RunOpts<'a>,
            _: mpsc::Sender<AgentEvent>,
        ) -> mis_agent::BoxFut<'a> {
            unreachable!()
        }
    }

    #[test]
    fn override_replaces_discovery_and_empty_hides() {
        let agents: Vec<Box<dyn Agent>> = vec![Box::new(Stub("claude")), Box::new(Stub("codex"))];
        let none = BTreeMap::new();
        assert_eq!(advertised(&agents, &none)["codex"], ["codex-found"]);
        let overrides = BTreeMap::from([
            ("claude".to_string(), vec!["opus".to_string()]),
            ("codex".to_string(), vec![]),
            ("other".to_string(), vec!["x".to_string()]),
        ]);
        let got = advertised(&agents, &overrides);
        assert_eq!(
            got,
            BTreeMap::from([("claude".into(), vec!["opus".into()])])
        );
    }
}

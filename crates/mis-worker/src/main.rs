//! Worker: connects to the orchestrator, advertises its harnesses, runs
//! assigned tasks through `mis_agent` and streams events back.

mod approvals;
mod config;
mod inventory;
mod mcp;
mod permissions;
mod pr;
mod roots;
mod rules;
mod sessions;
mod setup;
mod stats;
mod transcript;
mod update;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use mis_agent::Agent;
use mis_types::*;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

/// `0.1.0+abc1234`, with `-dirty` when built from uncommitted changes.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("MIS_GIT_REV"));

#[derive(Parser)]
#[command(
    about = "Make It So worker",
    version = VERSION,
    args_conflicts_with_subcommands = true,
    after_help = "Unset flags fall back to $MIS_STATE_DIR/worker.toml (default ~/.config/mis)."
)]
struct Args {
    /// Default: ws://127.0.0.1:7474.
    #[arg(long, env = "MIS_URL")]
    url: Option<String>,
    #[arg(long, env = "MIS_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Cloudflare Access service-token id; the secret comes from
    /// CF_ACCESS_CLIENT_SECRET (env only, so it stays out of `ps`).
    #[arg(long, env = "CF_ACCESS_CLIENT_ID")]
    cf_access_client_id: Option<String>,
    /// Directory tasks may run in (repeatable; MIS_ROOTS is comma/`:`-separated).
    /// Default: the current directory.
    #[arg(long = "root", env = "MIS_ROOTS")]
    roots: Vec<String>,
    /// Let agents use any tool, anywhere: overrides worker.toml `[permissions]`
    /// with mode = "full".
    #[arg(long)]
    skip_permissions: bool,
    /// Don't switch to the orchestrator's version by downloading its signed release build
    /// (`mis-up` sets MIS_NO_SELF_UPDATE). Builds not made by CI never do.
    #[arg(long, env = "MIS_NO_SELF_UPDATE", value_parser = clap::builder::FalseyValueParser::new())]
    no_self_update: bool,
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
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::prelude::*;
    let env =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // At trace, tungstenite logs the raw upgrade request (token, Access secret)
    // and every frame (prompts, images): cap it at debug, whatever RUST_LOG says.
    let cap = Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("tungstenite", LevelFilter::DEBUG)
        .with_target("tokio_tungstenite", LevelFilter::DEBUG);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(cap)
                .with_filter(env),
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
        cf_access_client_secret: access_secret_env(),
        roots: args
            .roots
            .iter()
            .flat_map(|r| roots::split_roots(r))
            .collect(),
        ..Default::default()
    };
    let file = config::load(&path)?;
    let cfg = config::merge(cli, file.clone(), std::env::current_dir()?);
    // Validate before the reconnect loop so bad credentials fail fast.
    let access = access_values(&cfg)?;
    let roots = roots::canonical_roots(&cfg.roots)?;

    let perms = permissions::effective(cfg.permissions.clone(), args.skip_permissions)
        .map(|p| permissions::resolve(p, std::env::home_dir().as_deref()))
        .transpose()?;
    match &perms {
        Some(p) => tracing::info!(?p, "permissions"),
        None => tracing::warn!(
            "no [permissions] in {}: agents' writes, shell and MCP calls will be denied; \
             run `mis-worker setup` or pass --skip-permissions",
            path.display()
        ),
    }

    let (id, id_lock) = worker_id()?;
    // Before any swap: on Linux a replaced executable's path reads "(deleted)". Resolved, so a
    // symlinked install swaps the real file.
    let exe = std::env::current_exe()?.canonicalize()?;
    mis_update::cleanup(&exe);
    let source = mis_update::Source::from_env()?;
    let blocked = mis_update::blocked(
        args.no_self_update,
        VERSION,
        mis_update::CI_BUILD,
        source.custom,
    );
    let updater = update::Updater::new(VERSION, blocked, &source.base, mis_update::TARGET);
    tracing::info!("mis-worker {VERSION}; {}", updater.describe());
    let agents = mis_agent::available();
    if agents.is_empty() {
        anyhow::bail!("no agent harness found on PATH (need `claude` or `codex`)");
    }
    let scan = roots.clone();
    let inventory = tokio::task::spawn_blocking(move || inventory::collect(&scan)).await?;
    if let Err(e) = config::save_inventory(&path, file, &inventory) {
        tracing::warn!("could not save inventory: {e:#}");
    }
    let info = WorkerInfo {
        hostname: hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        harnesses: agents.iter().map(|a| a.name().to_string()).collect(),
        roots: roots.iter().map(|r| r.display().to_string()).collect(),
        models: advertised(&agents, &cfg.models),
        purpose: cfg.purpose.clone(),
        inventory,
        version: Some(VERSION.into()),
        permissions: perms.clone(),
    };
    tracing::info!(?info.harnesses, ?info.models, ?info.roots, "connecting to {}", cfg.url);
    tracing::info!("inventory: {}", info.inventory.brief());
    // PR status reads and purpose confirmations, sent as they come; dropped if
    // the socket dies mid-send.
    let (pr_tx, pr_rx) = mpsc::channel::<WorkerMsg>(64);
    let approvals = approvals::Approvals::new(
        perms,
        pr_tx.clone(),
        path.clone(),
        std::env::home_dir(),
        mis_agent::APPROVAL_WAIT,
    );
    let ctx = Ctx {
        agents: Arc::new(agents),
        approvals: Arc::new(approvals),
        inventory: Arc::new(info.inventory.clone()),
        roots: Arc::new(roots),
        pr_tx,
        limits: Default::default(),
    };
    // Events from in-flight tasks queue here while disconnected. Delivery is best
    // effort: an event already written to a socket that then dies is not resent.
    let (ev_tx, ev_rx) = mpsc::channel::<(TaskId, AgentEvent)>(1024);
    let (upd_tx, upd_rx) = mpsc::channel(1);
    let mut conn = Conn {
        ev_rx,
        pending: None,
        running: HashSet::new(),
        pr_rx,
        watches: HashMap::new(),
        backoff: Duration::from_secs(1),
        sampler: stats::Sampler::new(ctx.roots.first().cloned().unwrap_or_else(|| "/".into())),
        info,
        config: path,
        update: updater,
        source: Arc::new(source),
        exe: exe.clone(),
        upd_tx,
        upd_rx,
    };

    loop {
        match session(&cfg, &access, id, &ctx, &ev_tx, &mut conn).await {
            Ok(Ended::Restart) => break,
            Ok(Ended::Closed) => tracing::info!("orchestrator closed connection"),
            Err(e) => tracing::warn!("connection lost: {e:#}"),
        }
        tracing::info!("reconnecting in {:?}", conn.backoff);
        tokio::time::sleep(conn.backoff).await;
        conn.backoff = (conn.backoff * 2).min(Duration::from_secs(30));
    }
    tracing::info!("restarting on the new build");
    // The new process takes the id lock (on Windows it starts before this one exits).
    drop(id_lock);
    Err(mis_update::restart(&exe))
}

/// Start downloading the orchestrator's build if one is due; the checked executable comes
/// back on `upd_rx`.
fn check_update(c: &mut Conn) {
    let Some(asset) = c.update.due(Instant::now()) else {
        return;
    };
    let (src, done) = (c.source.clone(), c.upd_tx.clone());
    tokio::spawn(async move {
        tracing::info!("updating to {} from {}", asset.version, asset.url);
        let exe = mis_update::fetch(&src, &asset, "mis-worker").await;
        let _ = done
            .send((asset.version, exe.map_err(|e| format!("{e:#}"))))
            .await;
    });
}

/// Swap in a downloaded `version` if the orchestrator still runs it; returns what to report.
async fn install_update(
    c: &mut Conn,
    version: String,
    exe: std::result::Result<Vec<u8>, String>,
) -> Option<WorkerUpdate> {
    if exe.is_ok() && !c.update.wants(&version) {
        tracing::info!("orchestrator moved on from {version}; not installing it");
        c.update.abandon();
        check_update(c);
        return None;
    }
    let (target, v) = (c.exe.clone(), version.clone());
    let installed = match exe {
        Ok(exe) => tokio::task::spawn_blocking(move || {
            mis_update::stage(&target, &exe, "mis-worker", &v)?.install()
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
        .map_err(|e| format!("{e:#}")),
        Err(e) => Err(e),
    };
    match &installed {
        Ok(()) => tracing::info!("installed {version}; restarting once no task runs"),
        Err(e) => tracing::warn!("update to {version} failed: {e}"),
    }
    Some(c.update.fetched(&version, installed, Instant::now()))
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

/// What a task needs from the worker: harnesses, allowed directories, where its
/// runs leave usage-limit readings for the stats beat, and where PR watches send.
#[derive(Clone)]
struct Ctx {
    agents: Arc<Vec<Box<dyn Agent>>>,
    /// Holds the live permissions (None: see `permissions::effective`); in ask
    /// mode, where claude's approvals wait for the user.
    approvals: Arc<approvals::Approvals>,
    /// For `permissions::for_harness`.
    inventory: Arc<Inventory>,
    roots: Arc<Vec<PathBuf>>,
    /// Latest limit windows per harness, from runs and polling.
    limits: Arc<Mutex<BTreeMap<String, Vec<LimitWindow>>>>,
    pr_tx: mpsc::Sender<WorkerMsg>,
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
    pr_rx: mpsc::Receiver<WorkerMsg>,
    /// One PR watch per task; a new `CheckPrs` replaces it.
    watches: HashMap<TaskId, tokio::task::AbortHandle>,
    backoff: Duration,
    sampler: stats::Sampler,
    /// Sent in each Hello; its purpose changes on `OrchMsg::SetPurpose`.
    info: WorkerInfo,
    /// worker.toml, where a new purpose is saved.
    config: PathBuf,
    update: update::Updater,
    source: Arc<mis_update::Source>,
    /// This executable, which an update replaces.
    exe: PathBuf,
    /// Downloads from `check_update`: version, and its checked executable or why not.
    upd_tx: mpsc::Sender<(String, std::result::Result<Vec<u8>, String>)>,
    upd_rx: mpsc::Receiver<(String, std::result::Result<Vec<u8>, String>)>,
}

/// How a session ended without error.
enum Ended {
    Closed,
    /// A new build is installed and nothing runs: restart into it.
    Restart,
}

/// One connection's lifetime: handshake, then forward events and run assignments
/// until the socket drops or it is time to restart on a new build.
async fn session(
    cfg: &config::Resolved,
    access: &[(&'static str, HeaderValue)],
    id: WorkerId,
    ctx: &Ctx,
    ev_tx: &mpsc::Sender<(TaskId, AgentEvent)>,
    c: &mut Conn,
) -> Result<Ended> {
    // An assignment carries up to MAX_IMAGES base64 images in one frame.
    let room = Some(MAX_IMAGES * MAX_IMAGE_BYTES * 4 / 3 + (1 << 20));
    let config = WebSocketConfig::default()
        .max_message_size(room)
        .max_frame_size(room);
    let (ws, _) = connect_async_with_config(worker_request(cfg, access)?, Some(config), false)
        .await
        .with_context(|| format!("connecting to {}", cfg.url))?;
    let (mut sink, mut stream) = ws.split();
    let running: Vec<TaskId> = c.running.iter().copied().collect();
    sink.send(text(&WorkerMsg::Hello {
        id,
        info: c.info.clone(),
        running,
    }))
    .await?;
    // The orchestrator forgets update state on reconnect.
    c.update.connected();
    if c.update.state != WorkerUpdate::None {
        let update = c.update.state.clone();
        sink.send(text(&WorkerMsg::Update { update })).await?;
    }
    // A restarted orchestrator lost them; one that kept them ignores the repeat.
    for msg in ctx.approvals.waiting() {
        sink.send(text(&msg)).await?;
    }
    // Heartbeat with stats. The first tick fires at once, so the orchestrator
    // has numbers right after Hello. Sampling (~20ms) runs inline; at one per
    // 15s that stall is negligible.
    let mut beat = tokio::time::interval(STATS_INTERVAL);
    // After a stall (e.g. laptop sleep), send one beat, not a burst.
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if c.update.restart_due(c.running.is_empty()) {
            return Ok(Ended::Restart);
        }
        // Resend a stranded event before pulling a new one.
        let next = match c.pending.take() {
            Some(ev) => Some(ev),
            None => tokio::select! {
                ev = c.ev_rx.recv() => ev,
                msg = stream.next() => {
                    handle_incoming(msg, ctx, ev_tx, c)?;
                    continue;
                }
                Some(msg) = c.pr_rx.recv() => {
                    // Queued while offline, then resent after Hello and answered: don't revive it.
                    let answered = matches!(&msg, WorkerMsg::ApprovalRequest { approval, .. }
                        if !ctx.approvals.is_waiting(approval.id));
                    if !answered {
                        sink.send(text(&msg)).await?;
                    }
                    continue;
                }
                Some((version, exe)) = c.upd_rx.recv() => {
                    if let Some(update) = install_update(c, version, exe).await {
                        sink.send(text(&WorkerMsg::Update { update })).await?;
                    }
                    continue;
                }
                _ = beat.tick() => {
                    check_update(c);
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
            return Ok(Ended::Closed);
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
    c: &mut Conn,
) -> Result<()> {
    match msg {
        Some(Ok(Message::Text(t))) => match serde_json::from_str::<OrchMsg>(&t) {
            Ok(OrchMsg::Welcome {
                id,
                version,
                channel,
            }) => {
                tracing::info!(%id, ?version, ?channel, "registered");
                c.backoff = Duration::from_secs(1);
                c.update.welcome(version, channel);
                check_update(c);
            }
            Ok(OrchMsg::Assign { task, images }) => {
                if !c.running.insert(task.id) {
                    tracing::warn!(id = %task.id, "already running; ignoring duplicate assign");
                    return Ok(());
                }
                tracing::info!(id = %task.id, "assigned to {}: {}", task.harness, task.prompt);
                let ctx = ctx.clone();
                let tx = ev_tx.clone();
                tokio::spawn(async move { run_task(&ctx, task, images, tx).await });
            }
            Ok(OrchMsg::CheckPrs { task, urls }) => {
                c.watches.retain(|_, h| !h.is_finished());
                if let Some(old) = c.watches.remove(&task) {
                    old.abort();
                }
                let tx = ctx.pr_tx.clone();
                let watch = tokio::spawn(async move {
                    pr::watch(std::ffi::OsStr::new("gh"), task, &urls, tx).await
                });
                c.watches.insert(task, watch.abort_handle());
            }
            Ok(OrchMsg::UpdateHeld) => c.update.held(),
            Ok(OrchMsg::SetPurpose { purpose }) => {
                let saved = config::load(&c.config).and_then(|file| {
                    let file = config::Config {
                        purpose: purpose.clone(),
                        ..file
                    };
                    config::write(&c.config, &file)
                });
                // Confirmed only once on disk, else reported.
                let reply = match saved {
                    Ok(()) => {
                        tracing::info!(?purpose, "purpose saved");
                        c.info.purpose = purpose.clone();
                        WorkerMsg::Purpose { purpose }
                    }
                    Err(e) => {
                        tracing::warn!("saving purpose: {e:#}");
                        WorkerMsg::PurposeFailed {
                            why: format!("{e:#}"),
                        }
                    }
                };
                let tx = ctx.pr_tx.clone();
                tokio::spawn(async move { tx.send(reply).await });
            }
            Ok(OrchMsg::Approval { task, id, decision }) => {
                tracing::info!(%task, %id, ?decision, "answered");
                if let Some(p) = ctx.approvals.decide(id, decision) {
                    c.info.permissions = Some(p.clone());
                    let msg = WorkerMsg::Permissions {
                        permissions: Some(p),
                    };
                    let tx = ctx.pr_tx.clone();
                    tokio::spawn(async move { tx.send(msg).await });
                }
            }
            Ok(OrchMsg::Query { req, query }) => {
                let tx = ctx.pr_tx.clone();
                tokio::spawn(async move {
                    // File reads: off the runtime's threads.
                    let answer = tokio::task::spawn_blocking(move || answer(query)).await;
                    if let Ok(answer) = answer {
                        let _ = tx.send(WorkerMsg::Answer { req, answer }).await;
                    }
                });
            }
            Err(e) => tracing::warn!("bad message from orchestrator: {e}"),
        },
        Some(Ok(_)) => {}
        Some(Err(e)) => return Err(e.into()),
        None => anyhow::bail!("orchestrator closed connection"),
    }
    Ok(())
}

fn answer(query: Query) -> Answer {
    match query {
        Query::ListSessions { headless } => {
            let (claude, codex) = sessions::roots();
            let sessions = sessions::list(&claude, &codex, headless, unix_ms());
            Answer::Sessions { sessions }
        }
        Query::ReadSession { harness, id, limit } => {
            let (claude, codex) = sessions::roots();
            transcript::read(&claude, &codex, &harness, id, limit, unix_ms())
        }
    }
}

async fn run_task(
    ctx: &Ctx,
    task: Task,
    images: Vec<ImageFile>,
    tx: mpsc::Sender<(TaskId, AgentEvent)>,
) {
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
    // Attached images live in a dir of their own until the task ends. No shared
    // parent: on a multi-user /tmp another user's would lock us out.
    let image_dir = std::env::temp_dir().join(format!("mis-task-{id}"));
    let image_paths = match images.is_empty() {
        true => vec![],
        false => match mis_agent::write_images(&image_dir, &images) {
            Ok(paths) => paths,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&image_dir);
                let _ = tx.send(fail(format!("attached images: {e:#}"))).await;
                return;
            }
        },
    };
    let perms = ctx.approvals.permissions.lock().unwrap().clone();
    let perms = perms.map(|p| permissions::for_harness(&p, agent.name(), &ctx.inventory));
    // Only claude can ask; codex in ask mode runs as folders.
    let ask = perms
        .as_ref()
        .is_some_and(|p| p.mode == PermissionMode::Ask)
        && agent.name() == "claude";
    // Without the server the task still runs, just without its reporting tools.
    let server = mcp::serve(id, tx.clone(), ask.then(|| ctx.approvals.clone()))
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
            images: &image_paths,
            permissions: perms.as_ref(),
        },
        atx,
    );
    tokio::join!(run, fwd);
    ctx.approvals.forget(id);
    if !images.is_empty() {
        let _ = std::fs::remove_dir_all(&image_dir);
    }
}

/// `CF_ACCESS_CLIENT_SECRET`; empty counts as unset.
fn access_secret_env() -> Option<String> {
    std::env::var("CF_ACCESS_CLIENT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
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
    fn no_self_update_env_takes_1_like_mis_up_sets_it() {
        for (v, off) in [("1", true), ("true", true), ("0", false), ("", false)] {
            std::env::set_var("MIS_NO_SELF_UPDATE", v);
            let args = Args::try_parse_from(["mis-worker"]).unwrap();
            assert_eq!(args.no_self_update, off, "MIS_NO_SELF_UPDATE={v:?}");
        }
        std::env::remove_var("MIS_NO_SELF_UPDATE");
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

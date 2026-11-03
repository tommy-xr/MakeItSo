//! Worker: connects to the orchestrator, advertises its harnesses, runs
//! assigned tasks through `mis_agent` and streams events back.

mod app_server;
mod approvals;
mod coalesce;
mod config;
mod deliver;
mod evidence;
mod feed;
mod git;
mod inventory;
mod live;
mod mcp;
mod merge;
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
use tokio::sync::{mpsc, oneshot};
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
    let updater = update::Updater::new(
        VERSION,
        mis_update::build_run(),
        blocked,
        &source.base,
        mis_update::TARGET,
    );
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
        // Where an open claude session can be told from a dead record (see `live::alive`).
        deliver: cfg!(unix),
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
        cancels: HashMap::new(),
        backoff: Duration::from_secs(1),
        sampler: stats::Sampler::new(ctx.roots.first().cloned().unwrap_or_else(|| "/".into())),
        info,
        config: path,
        update: updater,
        source: Arc::new(source),
        exe: exe.clone(),
        upd_tx,
        upd_rx,
        feed: Default::default(),
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

/// Start the update step that is due, if any (a download, or reading edge.json); its result
/// comes back on `upd_rx`.
fn check_update(c: &mut Conn) {
    let Some(step) = c.update.due(Instant::now()) else {
        return;
    };
    let (src, done) = (c.source.clone(), c.upd_tx.clone());
    tokio::spawn(async move {
        let got = match step {
            update::Step::Fetch(asset) => {
                tracing::info!("updating to {} from {}", asset.version, asset.url);
                let exe = mis_update::fetch(&src, &asset, "mis-worker").await;
                update::Done::Fetched(asset.version, exe.map_err(|e| update::Fail::of(&e)))
            }
            update::Step::ReadEdge => {
                let head = mis_update::edge_head(&src).await;
                update::Done::Head(head.map_err(|e| format!("{e:#}")))
            }
        };
        let _ = done.send(got).await;
    });
}

/// Swap in a downloaded `version` if the orchestrator still runs it; returns what to report.
async fn install_update(
    c: &mut Conn,
    version: String,
    exe: std::result::Result<Vec<u8>, update::Fail>,
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
        .map_err(|e| update::Fail::Failed(format!("{e:#}"))),
        Err(e) => Err(e),
    };
    let mins = |d: Duration| d.as_secs() / 60;
    match &installed {
        Ok(()) => tracing::info!("installed {version}; restarting once no task runs"),
        Err(update::Fail::Unpublished) => {} // below: on edge, `head` logs the decision instead
        Err(update::Fail::Failed(e)) => tracing::warn!(
            "update to {version} failed: {e}; retry in {}m",
            mins(update::RETRY_AFTER)
        ),
    }
    let unpublished = installed == Err(update::Fail::Unpublished);
    let update = c.update.fetched(&version, installed, Instant::now());
    if unpublished && !c.update.reads_edge() {
        let wait = mins(update::RETRY_UNPUBLISHED);
        tracing::info!("{version} not published yet; retry in {wait}m");
    }
    // Next step at once: on edge, an unpublished build is followed by reading edge.json.
    check_update(c);
    Some(update)
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
    /// Stops a running task (`OrchMsg::Cancel`); dropped once it ends.
    cancels: HashMap<TaskId, oneshot::Sender<()>>,
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
    /// Results of `check_update`'s steps.
    upd_tx: mpsc::Sender<update::Done>,
    upd_rx: mpsc::Receiver<update::Done>,
    /// Live sessions: parsed files, and what this connection was last sent.
    feed: feed::Feed,
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
    // Live sessions: sent on the first tick, then when they change.
    c.feed.reconnected();
    let mut poll = tokio::time::interval(feed::POLL_EVERY);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                Some(done) = c.upd_rx.recv() => {
                    let update = match done {
                        update::Done::Fetched(version, exe) => install_update(c, version, exe).await,
                        update::Done::Head(head) => {
                            if let Some(line) = c.update.head(head, Instant::now()) {
                                tracing::info!("{line}");
                            }
                            check_update(c);
                            None
                        }
                    };
                    if let Some(update) = update {
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
                _ = poll.tick() => {
                    let t = std::time::Instant::now();
                    let mut feed = std::mem::take(&mut c.feed);
                    // File reads: off the runtime's threads. A panic there loses the cache only.
                    let polled = tokio::task::spawn_blocking(move || {
                        let (claude, codex) = sessions::roots();
                        let procs = live::claude_procs(&claude, live::alive);
                        let loaded = app_server::loaded(&codex);
                        let all = feed.poll(&claude, &codex, &procs, &loaded, unix_ms());
                        (feed.changed(all), feed)
                    });
                    let (changed, feed) = polled.await.unwrap_or_default();
                    let parsed = feed.parsed;
                    c.feed = feed;
                    tracing::debug!(took = ?t.elapsed(), parsed, changed = changed.is_some(), "live sessions read");
                    if let Some(sessions) = changed {
                        let msg = text(&WorkerMsg::LiveSessions { sessions });
                        tracing::debug!(bytes = msg.len(), "sending live sessions");
                        sink.send(msg).await?;
                    }
                    continue;
                }
            },
        };
        let Some((task, event)) = next else {
            return Ok(Ended::Closed);
        };
        let finished = matches!(
            event,
            AgentEvent::Finished { .. } | AgentEvent::Cancelled | AgentEvent::Delivered { .. }
        );
        if let Err(e) = sink
            .send(text(&WorkerMsg::Event {
                task,
                event: event.clone(),
            }))
            .await
        {
            // A file too big for an older orchestrator's frames would fail on every
            // reconnect: it is dropped, not resent.
            match event {
                AgentEvent::Artifact(d) => {
                    tracing::warn!(%task, "evidence {:?} not sent: {e}", d.title)
                }
                event => c.pending = Some((task, event)),
            }
            return Err(e.into());
        }
        if finished {
            c.running.remove(&task);
            c.cancels.remove(&task);
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
                let (cancel, cancelled) = oneshot::channel();
                c.cancels.insert(task.id, cancel);
                let ctx = ctx.clone();
                let tx = ev_tx.clone();
                tokio::spawn(async move { run_task(&ctx, task, images, tx, cancelled).await });
            }
            Ok(OrchMsg::Cancel { task }) => match c.cancels.remove(&task) {
                Some(cancel) => {
                    tracing::info!(%task, "cancelling");
                    let _ = cancel.send(());
                }
                // Already ended; its Finished is on the way.
                None => tracing::info!(%task, "cancel for a task not running here"),
            },
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

/// Longest a `SendMode::WhenClosed` message waits for its session to close. It holds
/// this worker meanwhile (one task at a time), so not long.
const WAIT_CLOSED: Duration = Duration::from_secs(15 * 60);
const RECHECK_OPEN: Duration = Duration::from_secs(10);

/// Where a message to a session on this disk goes.
enum Placed {
    /// Continued in the session's own folder (and on its model).
    Run(PathBuf, Option<String>),
    /// Into its open terminal (`deliver_open`): the event that ends the task.
    Delivered(AgentEvent),
}

/// Where a message to a session on this disk goes, once it may be continued as `to.mode`
/// asks (never in place while open). Deliver tries the open terminal at each check.
async fn session_cwd(
    ctx: &Ctx,
    task: &Task,
    to: &SendTo,
    images: bool,
    tx: &mpsc::Sender<(TaskId, AgentEvent)>,
) -> std::result::Result<Placed, String> {
    let deadline = Instant::now() + WAIT_CLOSED;
    let mut waiting = false;
    let (cwd, model) = loop {
        if to.mode == SendMode::Deliver {
            if let Some(ev) = deliver_open(ctx, task, to.session, images, tx).await {
                return Ok(Placed::Delivered(ev));
            }
        }
        let (harness, id) = (task.harness.clone(), to.session);
        let (cwd, model, open) = tokio::task::spawn_blocking(move || session_state(&harness, id))
            .await
            .map_err(|e| e.to_string())??;
        match (to.mode, open) {
            (SendMode::Fork, _) | (_, false) => break (cwd, model),
            (SendMode::InPlace, true) => {
                return Err("the session is open right now; send again once it closes".into())
            }
            (SendMode::WhenClosed | SendMode::Deliver, true) if Instant::now() >= deadline => {
                return Err(format!(
                    "the session was still open after {} min",
                    WAIT_CLOSED.as_secs() / 60
                ))
            }
            (SendMode::WhenClosed | SendMode::Deliver, true) => {
                if !waiting {
                    let text = "waiting for the session to close".to_string();
                    let _ = tx.send((task.id, AgentEvent::Status { text })).await;
                    waiting = true;
                }
                tokio::time::sleep(RECHECK_OPEN).await;
            }
        }
    };
    // Its folder must still exist and sit under a root, like any task's.
    Ok(Placed::Run(
        roots::resolve_cwd(&ctx.roots, Some(&cwd))?,
        model,
    ))
}

/// Where an open session takes a message.
enum Target {
    /// A claude terminal, by its peer name.
    Peer(String),
    /// A codex terminal's session, loaded on the app-server daemon.
    Daemon,
}

/// Deliver `task` into session `session` if a terminal holds it open: claude's under a
/// peer name, codex's on the app-server daemon. The event that ends the task; None
/// otherwise (closed, no peer name, not on the daemon): it is continued in place once
/// it may be.
async fn deliver_open(
    ctx: &Ctx,
    task: &Task,
    session: uuid::Uuid,
    images: bool,
    tx: &mpsc::Sender<(TaskId, AgentEvent)>,
) -> Option<AgentEvent> {
    let (id, harness) = (session.to_string(), task.harness.clone());
    let looked = tokio::task::spawn_blocking(move || {
        let (claude, codex) = sessions::roots();
        let target = match harness.as_str() {
            "claude" => Target::Peer(live::claude_procs(&claude, live::alive).remove(&id)?.name?),
            _ => {
                app_server::loaded(&codex).contains(&id).then_some(())?;
                Target::Daemon
            }
        };
        let path = transcript::find(&claude, &codex, &harness, session)?;
        let cwd = sessions::read_one(&harness, &path, unix_ms())?.cwd;
        Some((claude, codex, path, target, id, cwd))
    });
    let (claude, codex, path, target, id, cwd) = looked.await.ok()??;
    let fail = |why: String| {
        let summary = Some(why);
        Some(AgentEvent::Finished { ok: false, summary })
    };
    // Like any task here: only in a folder under the worker's roots.
    let cwd = cwd.ok_or_else(|| "the session doesn't record its folder".to_string());
    if let Err(why) = cwd.and_then(|c| roots::resolve_cwd(&ctx.roots, Some(&c))) {
        return fail(why);
    }
    if images {
        return fail(
            "images can't go into a session open in a terminal; send the text alone".into(),
        );
    }
    let started = AgentEvent::Started {
        session: Some(id.clone()),
        model: None,
    };
    match target {
        Target::Peer(name) => {
            let status = AgentEvent::Status {
                text: format!("delivering to {name}"),
            };
            for ev in [started, status] {
                let _ = tx.send((task.id, ev)).await;
            }
            Some(deliver::deliver(claude, path, id, &name, &task.prompt).await)
        }
        // Taken in at once, or not at all (None: it is continued in place instead).
        #[cfg(unix)]
        Target::Daemon => {
            let (text, msg_id) = (task.prompt.clone(), app_server::msg_id(task.id));
            let ev = deliver::to_codex(codex, id, text, msg_id).await?;
            let _ = tx.send((task.id, started)).await;
            Some(ev)
        }
        #[cfg(not(unix))]
        Target::Daemon => {
            let _ = codex;
            None
        }
    }
}

/// A session's folder, model, and whether it is open. Err when it isn't on this disk.
type SessionState = (String, Option<String>, bool);
fn session_state(harness: &str, id: uuid::Uuid) -> std::result::Result<SessionState, String> {
    let (claude, codex) = sessions::roots();
    let path = transcript::find(&claude, &codex, harness, id)
        .ok_or_else(|| format!("no {harness} session {id} on this worker"))?;
    let s = sessions::read_one(harness, &path, unix_ms())
        .ok_or_else(|| format!("can't read {harness} session {id}"))?;
    let cwd = s.cwd.ok_or("the session doesn't record its folder")?;
    let open = sessions::is_open(&claude, &codex, harness, &s.id, s.active);
    Ok((cwd, s.model, open))
}

/// Run `task` until it ends or `cancelled` fires, which drops the run where it is
/// (waiting for a session, or the agent, whose processes go with it) and ends it
/// with `AgentEvent::Cancelled`.
async fn run_task(
    ctx: &Ctx,
    task: Task,
    images: Vec<ImageFile>,
    tx: mpsc::Sender<(TaskId, AgentEvent)>,
    cancelled: oneshot::Receiver<()>,
) {
    let id = task.id;
    let had_images = !images.is_empty();
    // Where it started in its repo, until its diff is sent.
    let base = Mutex::new(None);
    let cancelled = tokio::select! {
        () = run(ctx, task, images, &tx, &base) => false,
        // Err: the sender is gone, so no cancel can come; let the run finish.
        Ok(()) = cancelled => true,
    };
    ctx.approvals.forget(id);
    if had_images {
        let _ = std::fs::remove_dir_all(image_dir(id));
    }
    if cancelled {
        tracing::info!(%id, "cancelled");
        send_diff(id, &base, &tx).await;
        let _ = tx.send((id, AgentEvent::Cancelled)).await;
    }
}

/// Attached images live in a dir of their own until the task ends. No shared
/// parent: on a multi-user /tmp another user's would lock us out.
fn image_dir(id: TaskId) -> PathBuf {
    std::env::temp_dir().join(format!("mis-task-{id}"))
}

/// What the task changed in its repo since `base`, sent before the run ends
/// (once: `base` is taken).
async fn send_diff(
    id: TaskId,
    base: &Mutex<Option<DiffBase>>,
    tx: &mpsc::Sender<(TaskId, AgentEvent)>,
) {
    let Some(base) = base.lock().unwrap().take() else {
        return;
    };
    // Bounded: the run's end waits on it.
    let diff = tokio::time::timeout(Duration::from_secs(30), git::diff(&base)).await;
    match diff.ok().flatten() {
        Some(d) => {
            let _ = tx.send((id, AgentEvent::Diff(d))).await;
        }
        None => tracing::warn!(%id, "no diff of {}", base.root),
    }
}

async fn run(
    ctx: &Ctx,
    task: Task,
    images: Vec<ImageFile>,
    tx: &mpsc::Sender<(TaskId, AgentEvent)>,
    base: &Mutex<Option<DiffBase>>,
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
    // A session on this disk runs in its own folder, on its own model.
    let (cwd, model) = match &task.send_to {
        Some(to) => match session_cwd(ctx, &task, to, !images.is_empty(), tx).await {
            Ok(Placed::Run(cwd, model)) => (Ok(cwd), model),
            Ok(Placed::Delivered(ev)) => {
                let _ = tx.send((id, ev)).await;
                return;
            }
            Err(why) => (Err(why), None),
        },
        None => (roots::resolve_cwd(&ctx.roots, task.cwd.as_deref()), None),
    };
    let model = task.model.clone().or(model);
    let cwd = match cwd {
        Ok(c) => c.display().to_string(),
        Err(why) => {
            tracing::warn!(%id, "{why}");
            let _ = tx.send(fail(why)).await;
            return;
        }
    };
    let image_dir = image_dir(id);
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
    let files = evidence::Files {
        cwd: PathBuf::from(&cwd),
        roots: ctx.roots.clone(),
    };
    let merger = match &task.merge {
        MergeRule::Tool(policy) => Some(merge::Merger {
            task: id,
            policy: (**policy).clone(),
            dry: task.read_only,
            approvals: ctx.approvals.clone(),
            gh: "gh".into(),
        }),
        MergeRule::Any | MergeRule::Never => None,
    };
    let approvals = ask.then(|| ctx.approvals.clone());
    let server = mcp::serve(id, tx.clone(), approvals, merger, files)
        .await
        .inspect_err(|e| tracing::warn!(%id, "no mcp server: {e}"))
        .ok();
    // A message to the user's own session goes in as typed.
    let prompt = match &task.send_to {
        Some(_) => task.prompt.clone(),
        None => mcp::with_hint(
            &task.prompt,
            server.is_some(),
            &mcp::machine_hint(&ctx.inventory, agent.name()),
        ),
    };
    // A message to the user's own session changes their repo, not the task's.
    if task.send_to.is_none() {
        *base.lock().unwrap() =
            git::base(&cwd, &ctx.roots, task.diff_base.clone().map(|b| *b)).await;
    }
    let (atx, mut arx) = mpsc::channel(64);
    let fwd = async {
        let mut deltas = coalesce::Coalescer::default();
        loop {
            let due = deltas.due();
            let evs = tokio::select! {
                ev = arx.recv() => match ev {
                    Some(ev) => deltas.event(ev, Instant::now()),
                    None => break,
                },
                () = sleep_until(due) => deltas.flush().into_iter().collect(),
            };
            for ev in evs {
                // Limit readings go into the stats beat, not the transcript.
                if let AgentEvent::Limits { mut windows } = ev {
                    let now = unix_ms();
                    windows.iter_mut().for_each(|w| w.observed_at = now);
                    ctx.add_limits(agent.name(), windows);
                    continue;
                }
                if matches!(ev, AgentEvent::Finished { .. }) {
                    send_diff(id, base, tx).await;
                }
                if tx.send((id, ev)).await.is_err() {
                    return;
                }
            }
        }
    };
    let run = mis_agent::run_guarded(
        agent.as_ref(),
        id,
        &prompt,
        mis_agent::RunOpts {
            cwd: Some(&cwd),
            model: model.as_deref(),
            mcp: server.as_ref().map(|s| &s.mcp),
            resume: task.resume.as_deref(),
            fork: (task.send_to.as_ref()).map(|s| s.mode == SendMode::Fork),
            images: &image_paths,
            permissions: perms.as_ref(),
            read_only: task.read_only,
            // A routine run that may act: other ways to merge are denied.
            no_merge: task.merge != MergeRule::Any && !task.read_only,
        },
        atx,
    );
    tokio::join!(run, fwd);
}

/// Sleeps until `at`; never wakes without one.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
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

/// `MIS_HOSTNAME` overrides it, so demos and PR screenshots needn't show the real one.
fn hostname() -> String {
    if let Some(h) = std::env::var("MIS_HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
    {
        return h;
    }
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

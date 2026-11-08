//! Headless orchestrator: tracks workers, queues tasks, assigns them to idle
//! workers with a matching harness, and fans agent events out to clients.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequest, Path, Query as Params, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use mis_types::*;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};

/// `0.1.0+abc1234`, with `-dirty` when built from uncommitted changes.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("MIS_GIT_REV"));

pub mod access;
pub mod access_gate;
pub mod apps;
pub mod apps_http;
pub mod capture;
pub mod evidence;
pub mod images;
pub mod ledger;
pub mod note;
pub mod plan;
pub mod push;
pub mod routine;
pub mod scope;
pub mod self_update;
pub mod store;
pub mod summary;
pub mod tools;

/// A stuck planner (auth prompt, network stall) fails the task instead of
/// leaving it in Planning forever.
const PLANNER_TIMEOUT: Duration = Duration::from_secs(90);
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the ledger is saved (only files that changed are written).
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// How often tasks past `ledger::persist::KEEP_MS` are forgotten.
const FORGET_EVERY: Duration = Duration::from_secs(3600);
/// How long a worker may take to answer an `OrchMsg::Query`.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// How often routine files are re-read and due ones started.
pub const ROUTINE_EVERY: Duration = Duration::from_secs(30);
/// Largest frame a worker may send: a compare's two images, base64, plus slack.
const WORKER_FRAME: usize = 2 * mis_types::evidence::MAX_FILE_BYTES.div_ceil(3) * 4 + (1 << 20);
/// Largest `/api/capture` body: its images plus room for the other fields.
const CAPTURE_BODY: usize = MAX_IMAGES * MAX_IMAGE_BYTES + (1 << 20);

use access::Identity;
use access_gate::{AccessGate, JWT_HEADER};
use ledger::{ConnId, Effect, Ledger, NotCancelled};
use mis_agent::Agent;
use scope::{Route, Scope};

/// A query's connection, and who waits for its answer.
type Waiting = (ConnId, oneshot::Sender<Answer>);

#[derive(Clone)]
pub struct App {
    ledger: Arc<Mutex<Ledger>>,
    conns: Arc<Mutex<HashMap<ConnId, mpsc::UnboundedSender<OrchMsg>>>>,
    next_conn: Arc<AtomicU64>,
    /// Queries awaiting a worker's `Answer`, by request id.
    queries: Arc<Mutex<HashMap<uuid::Uuid, Waiting>>>,
    query_timeout: Duration,
    events: broadcast::Sender<ServerMsg>,
    token: Option<String>,
    /// Tokens that may look (status, transcripts, MCP) but not act.
    read_tokens: Vec<String>,
    /// Access identities let in (any case); empty: any the Access app admits.
    access_subjects: Vec<String>,
    /// Tokens that may only `POST /api/capture`.
    capture_tokens: Vec<String>,
    /// Browser `Origin`s permitted to connect; requests without `Origin` are always let through.
    allowed_origins: Vec<String>,
    /// How long a disconnected worker may be gone before its tasks are failed.
    grace: Duration,
    /// Chooses a harness and rewrites the prompt for tasks submitted without one.
    /// None: such tasks default to "claude" unplanned.
    planner: Option<Arc<dyn Agent>>,
    /// Bounds concurrent planner processes; tasks wait in Planning.
    planner_slots: Arc<tokio::sync::Semaphore>,
    /// Summarizes finished tasks. None: they keep the agent's own summary.
    summarizer: Option<Arc<dyn Agent>>,
    /// Bounds concurrent summarizer processes, apart from the planner's.
    summary_slots: Arc<tokio::sync::Semaphore>,
    /// Drafts worker purpose notes (the planner's model, free text). None: no suggestions.
    drafter: Option<Arc<dyn Agent>>,
    /// A worker silent (no Hello/Stats) this long is treated as disconnected.
    stale_after: Duration,
    /// How often to check for silent workers.
    tick_every: Duration,
    /// How often to ping each client socket, so proxies don't close it as idle.
    ping_every: Duration,
    /// Origin of the ledger's monotonic `now`.
    started: Instant,
    /// Cloudflare Access in front: remote requests need its JWT.
    access: Option<Arc<AccessGate>>,
    /// Web Push to subscribed browsers. None: notifications are off.
    push: Option<Arc<push::Push>>,
    /// Uploaded images. None: attachments are off.
    images: Option<Arc<images::Images>>,
    /// Files agents attach as evidence. None: they are refused.
    evidence: Option<Arc<evidence::Store>>,
    /// Where captures are kept. None: they are refused.
    captures: Option<Arc<capture::Inbox>>,
    /// Where the ledger is saved. None: task history is lost on restart.
    ledger_dir: Option<std::path::PathBuf>,
    /// Open once serving, until shut down.
    store: Arc<Mutex<Option<store::Store>>>,
    /// Where this version's release builds are published, for workers to follow. None: nowhere.
    channel: Option<Channel>,
    /// Holds routine files (`*.md`). None: no routines.
    routines_dir: Option<std::path::PathBuf>,
    /// How often routine files are re-read and due ones started.
    routine_every: Duration,
    /// Clients watching live sessions (`ClientMsg::WatchSessions`); none: no diffing.
    watchers: Arc<AtomicUsize>,
    /// Published apps. None: publishing is off.
    apps: Option<Arc<apps::Apps>>,
}

impl App {
    /// `token`, if set, must be presented as `Authorization: Bearer <token>`.
    pub fn new(token: Option<String>) -> Self {
        Self {
            ledger: Default::default(),
            conns: Default::default(),
            next_conn: Default::default(),
            queries: Default::default(),
            query_timeout: QUERY_TIMEOUT,
            events: broadcast::channel(256).0,
            token,
            read_tokens: Vec::new(),
            access_subjects: Vec::new(),
            capture_tokens: Vec::new(),
            allowed_origins: Vec::new(),
            grace: Duration::from_secs(60),
            planner: None,
            planner_slots: Arc::new(tokio::sync::Semaphore::new(2)),
            summarizer: None,
            summary_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            drafter: None,
            // Two missed beats plus slack for network jitter.
            stale_after: 2 * STATS_INTERVAL + Duration::from_secs(5),
            tick_every: Duration::from_secs(5),
            // Cloudflare closes sockets idle for ~100s.
            ping_every: Duration::from_secs(25),
            started: Instant::now(),
            access: None,
            push: None,
            images: None,
            evidence: None,
            captures: None,
            ledger_dir: None,
            store: Default::default(),
            channel: None,
            routines_dir: None,
            routine_every: ROUTINE_EVERY,
            watchers: Default::default(),
            apps: None,
        }
    }

    /// Monotonic time for the ledger, which never reads the clock itself.
    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Override heartbeat timing (tests use short values).
    pub fn with_liveness(mut self, stale_after: Duration, tick_every: Duration) -> Self {
        self.stale_after = stale_after;
        self.tick_every = tick_every;
        self
    }

    /// Override the client ping interval (tests use short values).
    pub fn with_ping_every(mut self, every: Duration) -> Self {
        self.ping_every = every;
        self
    }

    /// Override how long workers may take to answer queries (tests use short values).
    pub fn with_query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = timeout;
        self
    }

    /// Also accept these tokens, read-only. Only meaningful with a full token.
    pub fn with_read_tokens(mut self, tokens: Vec<String>) -> Self {
        self.read_tokens = tokens;
        self
    }

    /// Also accept these tokens for `POST /api/capture` only. Only meaningful with a full token.
    pub fn with_capture_tokens(mut self, tokens: Vec<String>) -> Self {
        self.capture_tokens = tokens;
        self
    }

    /// Allow browser pages from these origins (already normalised, see [`normalize_origins`]).
    pub fn with_allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.allowed_origins = origins;
        self
    }

    /// Require a valid Cloudflare Access JWT on non-local requests.
    pub fn with_access(mut self, gate: AccessGate) -> Self {
        self.access = Some(Arc::new(gate));
        self
    }

    /// Only these Access identities (emails, service token client ids) get in;
    /// others are refused as if they had no JWT. Empty: any the Access app admits.
    pub fn with_access_subjects(mut self, subjects: Vec<String>) -> Self {
        self.access_subjects = subjects;
        self
    }

    /// Notify subscribed browsers when a task needs the user.
    pub fn with_push(mut self, push: push::Push) -> Self {
        self.push = Some(Arc::new(push));
        self
    }

    /// Accept image uploads and attach them to tasks.
    /// Tell workers this version's builds are published on `channel`, so they can switch to it.
    pub fn with_channel(mut self, channel: Option<Channel>) -> Self {
        self.channel = channel;
        self
    }

    pub fn with_images(mut self, images: images::Images) -> Self {
        self.images = Some(Arc::new(images));
        self
    }

    /// Accept, list and activate published apps.
    pub fn with_apps(mut self, apps: Arc<apps::Apps>) -> Self {
        self.apps = Some(apps);
        self
    }

    /// Keep and serve the files agents attach as evidence.
    pub fn with_evidence(mut self, store: evidence::Store) -> Self {
        self.evidence = Some(Arc::new(store));
        self
    }

    /// Keep captures in `inbox`.
    pub fn with_captures(mut self, inbox: capture::Inbox) -> Self {
        self.captures = Some(Arc::new(inbox));
        self
    }

    /// Save the ledger in `dir` and restore it from there when serving starts.
    pub fn with_ledger_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.ledger_dir = Some(dir);
        self
    }

    /// Run the routines defined in `dir`, checking every `every`.
    pub fn with_routines(mut self, dir: std::path::PathBuf, every: Duration) -> Self {
        self.routines_dir = Some(dir);
        self.routine_every = every;
        self
    }

    pub fn with_planner(mut self, planner: Arc<dyn Agent>) -> Self {
        self.planner = Some(planner);
        self
    }

    pub fn with_summarizer(mut self, summarizer: Arc<dyn Agent>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    pub fn with_drafter(mut self, drafter: Arc<dyn Agent>) -> Self {
        self.drafter = Some(drafter);
        self
    }

    /// The locked ledger, told the wall-clock time for whatever happens next.
    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        let mut l = self.ledger.lock().unwrap();
        l.now_ms = unix_ms();
        l
    }

    /// Run a ledger transition under the lock, then perform its effects outside it.
    /// Tasks a restart would interrupt.
    pub fn running(&self) -> usize {
        self.ledger().running()
    }

    /// Show `update` to clients (the page's update chip).
    pub fn report_update(&self, update: Update) {
        self.transition(|l| match l.set_update(update) {
            Ok(effects) => ((), effects),
            Err(why) => {
                tracing::warn!("update status: {why}");
                ((), vec![])
            }
        })
    }

    fn transition<R>(&self, f: impl FnOnce(&mut Ledger) -> (R, Vec<Effect>)) -> R {
        let (r, effects) = f(&mut self.ledger());
        self.apply(effects);
        r
    }

    fn apply(&self, effects: Vec<Effect>) {
        // Save who runs what before telling the worker, so a crash can't hand
        // the task to another worker on restart.
        if effects.iter().any(|e| matches!(e, Effect::Assign { .. })) {
            self.save();
        }
        for e in effects {
            match e {
                Effect::Send { conn, msg } => {
                    if let Some(tx) = self.conns.lock().unwrap().get(&conn) {
                        let _ = tx.send(msg);
                    }
                }
                Effect::Assign { conn, task, images } => {
                    let images = match (&self.images, images.is_empty()) {
                        (Some(store), false) => store.files(&images),
                        _ => vec![],
                    };
                    if let Some(tx) = self.conns.lock().unwrap().get(&conn) {
                        let _ = tx.send(OrchMsg::Assign { task, images });
                    }
                }
                Effect::Broadcast(msg) => {
                    let _ = self.events.send(msg);
                }
                Effect::StartGrace { wid, generation } => {
                    let app = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(app.grace).await;
                        let effects = app.ledger().expire(wid, generation);
                        app.apply(effects);
                    });
                }
                Effect::Plan { task, prompt } => {
                    let app = self.clone();
                    tokio::spawn(async move { app.run_planner(task, prompt).await });
                }
                Effect::SuggestPurpose { worker, prompt } => {
                    if let Some(agent) = self.drafter.clone() {
                        let app = self.clone();
                        tokio::spawn(async move { app.run_drafter(agent, worker, prompt).await });
                    }
                }
                Effect::Summarize { task, prompt } => {
                    if let Some(agent) = self.summarizer.clone() {
                        let app = self.clone();
                        tokio::spawn(async move { app.run_summarizer(agent, task, prompt).await });
                    }
                }
                // Dropping the sender ends that socket's `worker_conn` loop.
                Effect::Close { conn } => {
                    self.conns.lock().unwrap().remove(&conn);
                }
                Effect::Notify {
                    task,
                    title,
                    body,
                    urgent,
                } => {
                    if let Some(push) = self.push.clone() {
                        let opens = push::Opens::Task(task);
                        tokio::spawn(async move { push.send(opens, &title, &body, urgent).await });
                    }
                }
                Effect::NotifySession {
                    worker,
                    harness,
                    id,
                    title,
                    body,
                    urgent,
                } => {
                    if let Some(push) = self.push.clone() {
                        tokio::spawn(async move {
                            let (harness, id) = (&harness, &id);
                            let opens = push::Opens::Session {
                                worker,
                                harness,
                                id,
                            };
                            push.send(opens, &title, &body, urgent).await
                        });
                    }
                }
            }
        }
        // Any transition may change the sessions view (a report, a worker leaving, a
        // message's task moving on): diffed only while someone watches.
        if self.watchers.load(Ordering::Relaxed) > 0 {
            if let Some(Effect::Broadcast(msg)) = self.ledger().sessions_changed() {
                let _ = self.events.send(msg);
            }
        }
    }

    /// Stream the planner's events into the ledger. Only reached when a planner
    /// is set, since `client_conn` fills in the default harness otherwise.
    async fn run_planner(&self, task: TaskId, prompt: String) {
        let planner = self.planner.clone().expect("planner");
        run_oneshot(
            "planner",
            planner.as_ref(),
            &self.planner_slots,
            task,
            &prompt,
            PLANNER_TIMEOUT,
            |ev| {
                let effects = self.ledger().planning_event(task, ev);
                self.apply(effects);
            },
        )
        .await;
    }

    /// Replace a Done task's summary with the summarizer's; on failure keep it.
    async fn run_summarizer(&self, agent: Arc<dyn Agent>, task: TaskId, prompt: String) {
        run_oneshot(
            "summarizer",
            agent.as_ref(),
            &self.summary_slots,
            task,
            &prompt,
            SUMMARY_TIMEOUT,
            |ev| match ev {
                AgentEvent::Finished {
                    ok: true,
                    summary: Some(reply),
                } => self.transition(|l| ((), l.summarized(task, &reply))),
                AgentEvent::Finished { summary, .. } => {
                    tracing::warn!(%task, "summarizer failed: {}", summary.unwrap_or_default())
                }
                _ => {}
            },
        )
        .await;
    }

    /// Broadcast the drafter's note for `worker`; it is never saved here.
    async fn run_drafter(&self, agent: Arc<dyn Agent>, worker: WorkerId, prompt: String) {
        // The worker id stands in for a task id: it only labels the run.
        run_oneshot(
            "drafter",
            agent.as_ref(),
            &self.summary_slots,
            worker,
            &prompt,
            SUMMARY_TIMEOUT,
            |ev| {
                let (draft, error) = match ev {
                    AgentEvent::Finished {
                        ok: true,
                        summary: Some(reply),
                    } => (note::clean(&reply), None),
                    AgentEvent::Finished { summary, .. } => {
                        (String::new(), Some(summary.unwrap_or("no draft".into())))
                    }
                    _ => return,
                };
                let msg = ServerMsg::PurposeDraft {
                    worker,
                    draft,
                    error,
                };
                let _ = self.events.send(msg);
            },
        )
        .await;
    }

    /// Images for a new task: Err says why they can't be attached.
    fn check_images(&self, ids: &[uuid::Uuid]) -> Result<(), String> {
        match &self.images {
            _ if ids.is_empty() => Ok(()),
            Some(store) => images::check(ids, |id| store.find(id).is_some()),
            None => Err("image attachments are off".into()),
        }
    }

    /// Delete week-old images and month-old evidence now and every `SWEEP_EVERY`.
    fn spawn_image_sweep(&self) {
        let (images, evidence) = (self.images.clone(), self.evidence.clone());
        if images.is_none() && evidence.is_none() {
            return;
        }
        tokio::spawn(async move {
            let mut every = tokio::time::interval(images::SWEEP_EVERY);
            loop {
                every.tick().await;
                let now = std::time::SystemTime::now();
                let gone = images.as_ref().map_or(0, |s| s.sweep(now));
                if gone > 0 {
                    tracing::info!(gone, "swept old images");
                }
                let gone = evidence.as_ref().map_or(0, |s| s.sweep(now));
                if gone > 0 {
                    tracing::info!(gone, "swept old evidence files");
                }
            }
        });
    }

    /// Check and store a file the agent of `task` attached, then list it on the
    /// task; one refused becomes a note saying why. Decoding and writing run off
    /// the runtime; the caller awaits it, so the worker's later events wait too.
    async fn attach(&self, wid: WorkerId, conn: ConnId, task: TaskId, draft: EvidenceDraft) {
        let title = draft.title.clone();
        let Some(held) = self.ledger().attached(wid, conn, task).map(<[_]>::to_vec) else {
            return;
        };
        let store = self.evidence.clone();
        let stored = tokio::task::spawn_blocking(move || match store {
            None => Err("evidence files are off".to_string()),
            Some(store) => evidence::check(draft, &held).and_then(|checked| {
                let ids = (checked.files.iter())
                    .map(|(kind, bytes)| store.store(*kind, bytes))
                    .collect::<std::io::Result<Vec<_>>>()
                    .map_err(|e| {
                        tracing::warn!(%task, "storing evidence: {e}");
                        "couldn't store it".to_string()
                    })?;
                Ok(checked.evidence(uuid::Uuid::new_v4(), ids, unix_ms()))
            }),
        })
        .await
        .unwrap_or_else(|e| Err(format!("checking it failed: {e}")));
        if let Err(why) = &stored {
            tracing::warn!(%task, "evidence {title:?} refused: {why}");
        }
        let stored = stored.map_err(|why| (title, why));
        self.transition(|l| ((), l.add_evidence(wid, conn, task, stored)));
    }

    /// Bound and keep what `task` changed in its repo, then show its counts on
    /// the task. The caller awaits it, so the run's `Finished` lands after.
    async fn diffed(&self, wid: WorkerId, conn: ConnId, task: TaskId, diff: TaskDiff) {
        if self.ledger().attached(wid, conn, task).is_none() {
            return;
        }
        let diff = diff.bounded();
        if let Some(store) = self.evidence.clone() {
            // Served without the base: where the repo is and what was dirty stay here.
            let view = TaskDiff {
                base: Default::default(),
                ..diff.clone()
            };
            let stored = tokio::task::spawn_blocking(move || store.store_diff(task, &view)).await;
            if !matches!(stored, Ok(Ok(()))) {
                tracing::warn!(%task, "storing its diff failed: {stored:?}");
            }
        }
        self.transition(|l| ((), l.add_diff(wid, conn, task, &diff)));
    }

    /// Restore the saved ledger, if it has a dir. Unusable: serve without
    /// saving. Err: another orchestrator has it.
    fn open_store(&self) -> anyhow::Result<()> {
        let Some(dir) = &self.ledger_dir else {
            return Ok(());
        };
        match store::Store::open(dir, self.now(), unix_ms()) {
            Ok((store, ledger, effects)) => {
                tracing::info!(dir = %dir.display(), "task history kept");
                *self.ledger.lock().unwrap() = ledger;
                *self.store.lock().unwrap() = Some(store);
                self.apply(effects);
                self.forget_old_tasks();
            }
            Err(e) if e.is::<store::Locked>() => return Err(e),
            Err(e) => tracing::warn!("task history won't survive a restart: {e:#}"),
        }
        Ok(())
    }

    /// Snapshot the ledger under its lock, then write it. The store's lock is
    /// held throughout, so an older snapshot can't overwrite a newer one.
    fn save(&self) {
        let mut store = self.store.lock().unwrap();
        let Some(store) = store.as_mut() else {
            return;
        };
        let snap = self.ledger.lock().unwrap().snapshot(store.written());
        store.save(snap);
    }

    fn forget_old_tasks(&self) {
        let gone = self.transition(|l| l.sweep());
        if let Some(store) = self.store.lock().unwrap().as_mut() {
            store.forget(&gone);
        }
    }

    /// Save every `SAVE_EVERY` and forget old tasks every `FORGET_EVERY`.
    async fn keep_saved(&self) {
        if self.store.lock().unwrap().is_none() {
            return std::future::pending().await;
        }
        let mut save = tokio::time::interval(SAVE_EVERY);
        let mut forget =
            tokio::time::interval_at(tokio::time::Instant::now() + FORGET_EVERY, FORGET_EVERY);
        loop {
            tokio::select! {
                _ = save.tick() => {
                    let app = self.clone();
                    let _ = tokio::task::spawn_blocking(move || app.save()).await;
                }
                _ = forget.tick() => self.forget_old_tasks(),
            }
        }
    }

    /// Save a last time and release the dir for the next orchestrator. The
    /// ledger stays locked while writing, so nothing changes unsaved meanwhile.
    fn close_store(&self) {
        let mut store = self.store.lock().unwrap();
        let Some(mut s) = store.take() else {
            return;
        };
        let ledger = self.ledger.lock().unwrap();
        s.save(ledger.snapshot(s.written()));
        tracing::info!("task history saved");
    }

    /// The routine files as they are now: none without a routines dir. Ends (and
    /// saves at once) live for routines whose file no longer says `act: true`.
    async fn routine_defs(&self) -> Vec<routine::Def> {
        let Some(dir) = self.routines_dir.clone() else {
            return vec![];
        };
        let defs = tokio::task::spawn_blocking(move || load_routines(&dir)).await;
        let defs = defs.unwrap_or_default();
        if self.transition(|l| l.forget_live(&defs)) {
            self.save_now().await;
        }
        defs
    }

    /// `save` off the async runtime.
    async fn save_now(&self) {
        let app = self.clone();
        let _ = tokio::task::spawn_blocking(move || app.save()).await;
    }

    /// Re-read the routine files and start due runs every `routine_every`. The
    /// first check waits one period, so workers can reconnect after a restart.
    fn spawn_routines(&self) {
        let Some(dir) = self.routines_dir.clone() else {
            return;
        };
        tracing::info!(dir = %dir.display(), "routines on");
        let app = self.clone();
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + app.routine_every;
            let mut every = tokio::time::interval_at(start, app.routine_every);
            loop {
                every.tick().await;
                let defs = app.routine_defs().await;
                app.transition(|l| ((), l.routine_tick(&defs, &chrono::Local)));
            }
        });
    }

    /// Periodically disconnect workers that stopped sending heartbeats.
    fn spawn_liveness(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(app.tick_every);
            loop {
                every.tick().await;
                app.transition(|l| ((), l.tick(app.now(), app.stale_after)));
            }
        });
    }

    fn open_conn(&self, tx: mpsc::UnboundedSender<OrchMsg>) -> ConnId {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed) + 1;
        self.conns.lock().unwrap().insert(id, tx);
        id
    }

    fn close_conn(&self, wid: WorkerId, conn: ConnId) {
        self.conns.lock().unwrap().remove(&conn);
        // Its unanswered queries fail at once rather than time out.
        self.queries.lock().unwrap().retain(|_, (c, _)| *c != conn);
        self.transition(|l| ((), l.disconnect(wid, conn)));
    }

    /// Ask worker `wid` and wait for its answer. Err: the HTTP status and why.
    async fn ask(&self, wid: WorkerId, query: Query) -> Result<Answer, (StatusCode, &'static str)> {
        let conn = match self.ledger().worker_conn(wid) {
            None => return Err((StatusCode::NOT_FOUND, "unknown worker")),
            Some(None) => return Err((StatusCode::SERVICE_UNAVAILABLE, "worker offline")),
            Some(Some(conn)) => conn,
        };
        // Unique across restarts: a worker may deliver an old answer after reconnecting.
        let req = uuid::Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        self.queries.lock().unwrap().insert(req, (conn, tx));
        let _forget = Forget(&self.queries, req);
        let msg = OrchMsg::Query { req, query };
        let sent = (self.conns.lock().unwrap().get(&conn)).is_some_and(|c| c.send(msg).is_ok());
        if !sent {
            // Dropping the sender makes `rx` fail at once.
            self.queries.lock().unwrap().remove(&req);
        }
        match tokio::time::timeout(self.query_timeout, rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err((StatusCode::SERVICE_UNAVAILABLE, "worker disconnected")),
            Err(_) => Err((StatusCode::GATEWAY_TIMEOUT, "worker didn't answer in time")),
        }
    }

    /// Hand `answer` to whoever waits on `req`, if `conn` is the connection asked.
    fn answered(&self, conn: ConnId, req: uuid::Uuid, answer: Answer) {
        let mut queries = self.queries.lock().unwrap();
        if queries.get(&req).is_some_and(|(c, _)| *c == conn) {
            let (_, tx) = queries.remove(&req).expect("checked");
            let _ = tx.send(answer);
        }
    }

    async fn access_check(&self, headers: &HeaderMap, route: Route) -> Access {
        let Some(gate) = &self.access else {
            return Access::Off;
        };
        let Some(jwt) = headers.get(JWT_HEADER) else {
            return Access::Failed;
        };
        let jwt = jwt.to_str().unwrap_or_default();
        let main = gate.verify(jwt).await;
        // Only `/api/capture` takes the capture app's JWT. Its identity is capped at
        // capture scope, so the --access-subject list (who gets full) doesn't apply.
        let capture = match (&main, route) {
            (Err(_), Route::Capture) => gate.verify_capture(jwt).await,
            _ => None,
        };
        match (main, capture) {
            (Ok(id), _) if subject_listed(&self.access_subjects, &id.subject) => Access::Passed(id),
            (Ok(id), _) => {
                tracing::warn!(subject = %id.subject, "Cloudflare Access identity not in --access-subject");
                Access::Failed
            }
            (Err(_), Some(Ok(id))) => Access::Capture(id),
            (Err(e), capture) => {
                let capture = capture.and_then(Result::err);
                tracing::warn!(?capture, "rejected Cloudflare Access JWT: {e:?}");
                Access::Failed
            }
        }
    }

    /// Auth for a request to any route but `/api/capture`: None refuses it.
    async fn admit_scope(
        &self,
        headers: &HeaderMap,
        peer: SocketAddr,
        path: &str,
    ) -> Option<Scope> {
        self.admit_route(headers, peer, path, Route::Other).await
    }

    /// Auth for a request to `route`: None refuses it. Logs who Cloudflare Access let through.
    async fn admit_route(
        &self,
        headers: &HeaderMap,
        peer: SocketAddr,
        path: &str,
        route: Route,
    ) -> Option<Scope> {
        let access = self.access_check(headers, route).await;
        let scope = self.authorized_on(headers, peer.ip(), &access, route);
        if let (Some(_), Access::Passed(id) | Access::Capture(id)) = (scope, &access) {
            tracing::info!(subject = %id.subject, "{path}: Cloudflare Access");
        }
        scope
    }

    /// Admitted with full access; else why not (401, or 403 read-only).
    async fn admit_full(
        &self,
        headers: &HeaderMap,
        peer: SocketAddr,
        path: &str,
    ) -> Result<(), (StatusCode, &'static str)> {
        match self.admit_scope(headers, peer, path).await {
            Some(Scope::Full) => Ok(()),
            Some(Scope::Read | Scope::Capture) => Err((StatusCode::FORBIDDEN, "read-only token")),
            None => Err((StatusCode::UNAUTHORIZED, "")),
        }
    }

    /// Admitted with any scope: enough for read-only routes (`/mcp`).
    async fn admit(&self, headers: &HeaderMap, peer: SocketAddr, path: &str) -> bool {
        self.admit_scope(headers, peer, path).await.is_some()
    }

    /// [`Self::authorized_on`] any route but `/api/capture`.
    fn authorized(&self, headers: &HeaderMap, peer: IpAddr, access: &Access) -> Option<Scope> {
        self.authorized_on(headers, peer, access, Route::Other)
    }

    /// `peer` is the TCP peer. None: refused.
    fn authorized_on(
        &self,
        headers: &HeaderMap,
        peer: IpAddr,
        access: &Access,
        route: Route,
    ) -> Option<Scope> {
        // The capture app's JWT opens only `/api/capture`.
        if matches!(access, Access::Capture(_)) && route != Route::Capture {
            return None;
        }
        let passed = matches!(access, Access::Passed(_));
        // Behind Access only direct local connections may skip it: loopback, or a
        // private-network peer presenting the full token (a LAN worker). cloudflared also
        // connects from loopback, but always adds Cloudflare's headers. (Any local
        // process can still connect directly, as without Access.)
        let peer = peer.to_canonical();
        let full = |t: &String| bearer_from(headers) == Some(t.as_str());
        let direct =
            peer.is_loopback() || (private_net(peer) && self.token.as_ref().is_some_and(full));
        if matches!(access, Access::Failed) && (!direct || via_cloudflare(headers)) {
            return None;
        }
        let origin = match headers.get("origin").map(|v| v.to_str()) {
            None => None,
            Some(Ok(o)) => Some(o),
            Some(Err(_)) => return None,
        };
        let host = headers.get("host").and_then(|v| v.to_str().ok());
        let own_page = match (origin, host) {
            // Without a token, only loopback: a DNS-rebound name (evil.com -> 127.0.0.1)
            // would otherwise pass as same-origin.
            (Some(o), Some(h)) => {
                same_origin(o, h) && (passed || self.token.is_some() || loopback_host(h))
            }
            _ => false,
        };
        if !own_page && !origin_allowed(origin, &self.allowed_origins) {
            return None;
        }
        let bearer = bearer_from(headers).filter(|b| !b.is_empty());
        let listed = |tokens: &[String]| bearer.is_some_and(|b| tokens.iter().any(|t| t == b));
        let scope = match &self.token {
            None => Scope::Full,
            // Before the Access login: that never upgrades a capture token.
            Some(_) if listed(&self.capture_tokens) => Scope::Capture,
            Some(t) if passed || bearer == Some(t.as_str()) => Scope::Full,
            Some(_) if listed(&self.read_tokens) => Scope::Read,
            Some(_) => return None,
        };
        // The capture app's JWT grants at most capturing, whatever token rides along.
        let scope = match (access, scope) {
            (Access::Capture(_), Scope::Full) => Scope::Capture,
            _ => scope,
        };
        scope::reaches(scope, route).then_some(scope)
    }
}

/// Drops a query's waiter however `ask` ends, also when the HTTP client goes away mid-wait.
struct Forget<'a>(&'a Mutex<HashMap<uuid::Uuid, Waiting>>, uuid::Uuid);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap().remove(&self.1);
    }
}

/// Run a one-shot `agent` call (planner, summarizer) once a slot is free,
/// feeding its events to `on_event`. Past `timeout` the process is killed and a
/// failed `Finished` is fed instead.
async fn run_oneshot(
    what: &str,
    agent: &dyn Agent,
    slots: &tokio::sync::Semaphore,
    task: TaskId,
    prompt: &str,
    timeout: Duration,
    mut on_event: impl FnMut(AgentEvent),
) {
    let _slot = slots.acquire().await.expect("semaphore open");
    let (tx, mut rx) = mpsc::channel(64);
    let run = tokio::time::timeout(
        timeout,
        mis_agent::run_guarded(agent, task, prompt, Default::default(), tx),
    );
    let feed = async {
        while let Some(ev) = rx.recv().await {
            // A one-shot's answer counts once whole.
            if !matches!(ev, AgentEvent::TextDelta { .. }) {
                on_event(ev);
            }
        }
    };
    let (timed_out, _) = tokio::join!(run, feed);
    if timed_out.is_err() {
        // Dropping `run` killed the process (kill_on_drop) without a Finished.
        on_event(AgentEvent::Finished {
            ok: false,
            summary: Some(format!("{what} timed out after {timeout:?}")),
        });
    }
}

/// Cloudflare Access outcome for one request.
enum Access {
    /// Not configured.
    Off,
    Passed(Identity),
    /// A valid JWT from the capture-only Access app; only `/api/capture` checks for one.
    Capture(Identity),
    /// No valid Access JWT.
    Failed,
}

/// Whether `subject` may pass: listed (case-insensitively), or no list.
fn subject_listed(listed: &[String], subject: &str) -> bool {
    listed.is_empty() || listed.iter().any(|l| l.eq_ignore_ascii_case(subject))
}

/// RFC 1918 / link-local IPv4, or unique-local / link-local IPv6: the same LAN.
fn private_net(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

/// Request came through Cloudflare's edge (e.g. via cloudflared).
pub(crate) fn via_cloudflare(headers: &HeaderMap) -> bool {
    [JWT_HEADER, "cf-ray", "cf-connecting-ip"]
        .iter()
        .any(|h| headers.contains_key(*h))
}

/// Token from `Authorization: Bearer <t>`, or `Sec-WebSocket-Protocol: bearer, <t>`
/// since browsers cannot set headers on a WebSocket.
fn bearer_from(headers: &HeaderMap) -> Option<&str> {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(t) = header("authorization").and_then(|v| v.strip_prefix("Bearer ")) {
        return Some(t);
    }
    let mut protos = header("sec-websocket-protocol")?.split(',').map(str::trim);
    (protos.next()? == "bearer")
        .then(|| protos.next())
        .flatten()
}

/// `Origin` names the server itself, i.e. the request comes from the page at `/`:
/// `https://x:443` matches `Host: x:443`.
fn same_origin(origin: &str, host: &str) -> bool {
    origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

/// `localhost:7474`, `127.0.0.1`, `[::1]:7474`.
fn loopback_host(host: &str) -> bool {
    let Ok(a) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let name = a.host().trim_start_matches('[').trim_end_matches(']');
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Native clients send no `Origin`; a browser's must exactly match an allowlist entry.
/// Rejecting other origins stops arbitrary web pages from driving a local orchestrator.
fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    origin.is_none_or(|o| allowed.iter().any(|a| a == o))
}

/// Trim, lowercase, drop a trailing `/`, skip blanks: ` HTTPS://X.com/ ` -> `https://x.com`,
/// which is the form browsers send in `Origin`.
pub fn normalize_origins(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    raw.into_iter()
        .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
        .filter(|o| !o.is_empty())
        .collect()
}

/// Needs `ConnectInfo<SocketAddr>`; see [`serve`].
pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.svg", get(icon))
        .route("/apple-touch-icon.png", get(touch_icon))
        .route("/sw.js", get(service_worker))
        .route("/api/info", get(info))
        .route("/ws/worker", get(ws_worker))
        .route("/ws/client", get(ws_client))
        .route("/mcp", post(mcp))
        .route("/api/upload", post(upload))
        .route(
            "/api/capture",
            post(capture).layer(axum::extract::DefaultBodyLimit::max(CAPTURE_BODY)),
        )
        .route("/api/images/{id}", get(image))
        .route("/api/evidence/{id}", get(evidence_file))
        .route("/api/update", get(update_status).post(set_update))
        .route("/api/tasks/{id}/cancel", post(cancel))
        .route("/api/tasks/{id}/diff", get(task_diff))
        .route("/api/routines", get(routines).post(new_routine))
        .route("/api/routines/{name}/{action}", post(routine_action))
        .route("/api/apps", get(list_apps))
        .route("/api/apps/{slug}/releases", post(publish_app))
        .route("/api/apps/{slug}/activate", post(activate_app))
        .route("/api/sessions", get(live_sessions))
        .route("/api/workers/{id}/sessions", get(sessions))
        .route(
            "/api/workers/{id}/sessions/{harness}/{session}",
            get(session),
        )
        .route(
            "/api/workers/{id}/sessions/{harness}/{session}/messages",
            post(message),
        )
        .with_state(app)
}

/// Wall-clock seconds, for display (`stats_at`) and push JWT expiry.
fn unix_secs() -> u64 {
    unix_ms() / 1000
}

/// Wall-clock millis, only for display (task timing).
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub async fn serve(listener: TcpListener, app: App) -> anyhow::Result<()> {
    serve_until(listener, app, std::future::pending()).await
}

/// Serve until `shutdown` resolves, then save the ledger a last time.
pub async fn serve_until(
    listener: TcpListener,
    app: App,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    app.open_store()?;
    app.spawn_liveness();
    app.spawn_routines();
    app.spawn_image_sweep();
    let svc = router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::select! {
        served = axum::serve(listener, svc) => served?,
        _ = app.keep_saved() => {}
        _ = shutdown => {}
    }
    app.close_store();
    Ok(())
}

/// The apps listener; see [`apps_http`].
pub async fn serve_apps(listener: TcpListener, apps: Arc<apps::Apps>) -> anyhow::Result<()> {
    Ok(axum::serve(listener, apps_http::router(apps)).await?)
}

/// `--self-test`: this build can read the task history in `dir` (without taking its lock: the
/// running orchestrator holds it) and bind a port. Run on a new build before switching to it.
pub fn self_test(dir: Option<&std::path::Path>) -> anyhow::Result<()> {
    if let Some(dir) = dir {
        let path = dir.join(store::LEDGER_FILE);
        match std::fs::read_to_string(&path) {
            Ok(json) => {
                Ledger::restore(&json, |_| None, Duration::ZERO, 0)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::Error::from(e).context(path.display().to_string())),
        }
    }
    std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(())
}

/// The phone page. Not framable: it acts with the token stored for this origin.
/// `no-cache` makes browsers revalidate, so a rebuilt orchestrator's page shows on reload.
async fn page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Html(include_str!("../static/index.html")),
    )
}

// Home-screen install: manifest and icons, baked in like the page.
async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        include_str!("../static/manifest.webmanifest"),
    )
}
async fn icon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../static/icon.svg"),
    )
}
async fn touch_icon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        include_bytes!("../static/apple-touch-icon.png").as_slice(),
    )
}

/// Shows pushed notifications and opens their task. Revalidated like the page,
/// so a rebuilt orchestrator's worker takes over.
async fn service_worker() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
            (
                header::HeaderName::from_static("service-worker-allowed"),
                "/",
            ),
        ],
        include_str!("../static/sw.js"),
    )
}

/// Lets the page decide whether to ask for a token (not when Access already let
/// it in) and whether to offer composers, and shows what is running. `scope`: what
/// this request's bearer token, if any, would get (null: refused). `vapid`: the
/// key to subscribe to push with (null: notifications off).
async fn info(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let access = app.access_check(&headers, Route::Other).await;
    let scope = app.authorized(&headers, peer.ip(), &access);
    let access = matches!(access, Access::Passed(_));
    Json(serde_json::json!({
        "auth": app.token.is_some() && !access,
        "access": access,
        "scope": scope,
        "vapid": app.push.as_ref().map(|p| p.public_key()),
        "version": VERSION,
    }))
}

/// Read-only fleet tools over MCP (streamable HTTP, JSON replies only), behind
/// the same token, origin and Cloudflare Access checks as the WebSockets.
async fn mcp(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !app.admit(&headers, peer, "/mcp").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(req) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(mcp::parse_error())).into_response();
    };
    let reply = mcp::handle(&req, "mis", &tools::list(), |name, args| {
        tools::call(&app.ledger(), name, args).map(|v| v.to_string())
    });
    match reply {
        Some(r) => Json(r).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// Each `*.md` in `dir` by file name, parsed; a missing dir holds none.
fn load_routines(dir: &std::path::Path) -> Vec<routine::Def> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            let file = dir.display().to_string();
            return vec![Err(routine::BadFile {
                file,
                error: e.to_string(),
            })];
        }
    };
    let mut files: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let read = || std::fs::read_to_string(e.path()).map_err(|e| e.to_string());
            (name.ends_with(".md") && !name.starts_with('.')).then(|| (name, read()))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    routine::parse_all(files)
}

/// Routines with their schedules and last runs. Read-only, like task status.
async fn routines(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !app.admit(&headers, peer, "/api/routines").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let defs = app.routine_defs().await;
    Json(app.ledger().routine_views(&defs, &chrono::Local)).into_response()
}

/// Write a routine file from a [`mis_types::NewRoutine`]: 201, 400 with why it
/// doesn't parse, 409 if its name is taken. Read-only tokens get 403.
async fn new_routine(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/routines").await {
        return refused.into_response();
    }
    let Some(dir) = app.routines_dir.clone() else {
        return (StatusCode::NOT_FOUND, "routines are off").into_response();
    };
    let new = match serde_json::from_slice::<mis_types::NewRoutine>(&body) {
        Ok(new) => new,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let (file, text) = match routine::new_file(&new) {
        Ok(f) => f,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    // Another file may name itself this; the file itself is caught by `create_new`.
    let name = routine::stem(&file).to_string();
    if app
        .routine_defs()
        .await
        .iter()
        .flatten()
        .any(|r| r.name == name)
    {
        return (StatusCode::CONFLICT, "a routine has that name").into_response();
    }
    // Before the file exists, so the scheduler never pairs it with an old routine's state.
    app.ledger().reset_routine(&name);
    app.save_now().await;
    let write = move || {
        use std::io::Write;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(file);
        let mut f = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(path)?;
        f.write_all(text.as_bytes())
    };
    match tokio::task::spawn_blocking(write).await {
        Ok(Ok(())) => StatusCode::CREATED.into_response(),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            (StatusCode::CONFLICT, "a routine file has that name").into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `run` (replies `{"task": id}`), `pause`, `resume`, `live` or `dry-run` routine
/// `name`. Acting, like submitting a task: read-only tokens get 403. `live`
/// carries `{"pin": ..}`, the `RoutineView::pin` the user reviewed: 409 if the
/// file has changed since, so nothing goes live unseen.
async fn routine_action(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((name, action)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/routines").await {
        return refused.into_response();
    }
    // A valid routine first: a bad file named like it mustn't shadow it.
    let defs = app.routine_defs().await;
    let valid = defs.iter().flatten().find(|r| r.name == name).cloned();
    let bad = || defs.iter().filter_map(|d| d.as_ref().err());
    let r = match (valid, bad().find(|b| routine::stem(&b.file) == name)) {
        (Some(r), _) => r,
        (None, Some(bad)) => return (StatusCode::BAD_REQUEST, bad.error.clone()).into_response(),
        (None, None) => return (StatusCode::NOT_FOUND, "unknown routine").into_response(),
    };
    match action.as_str() {
        "run" => app.transition(|l| {
            if l.routine_runs(&name).busy {
                let why = "its last run hasn't finished";
                return ((StatusCode::CONFLICT, why).into_response(), vec![]);
            }
            match l.start_routine(&r) {
                Ok((task, effects)) => (
                    Json(serde_json::json!({ "task": task })).into_response(),
                    effects,
                ),
                Err(why) => (
                    (StatusCode::SERVICE_UNAVAILABLE, why).into_response(),
                    vec![],
                ),
            }
        }),
        "pause" | "resume" => {
            app.ledger().pause_routine(&name, action == "pause");
            StatusCode::NO_CONTENT.into_response()
        }
        "live" | "dry-run" => {
            let seen = serde_json::from_slice::<serde_json::Value>(&body).ok();
            let seen = seen.as_ref().and_then(|v| v["pin"].as_str());
            match seen {
                // Not acting: `set_live` says why.
                _ if action != "live" || !r.act => {}
                None => {
                    let why = "send {\"pin\": ..} from GET /api/routines: the version you reviewed";
                    return (StatusCode::BAD_REQUEST, why).into_response();
                }
                Some(p) if p != r.pin() => {
                    let why = "its file changed since you read it: review it and go live again";
                    return (StatusCode::CONFLICT, why).into_response();
                }
                Some(_) => {}
            }
            if let Err(why) = app.transition(|l| match l.set_live(&r, action == "live") {
                Ok(effects) => (Ok(()), effects),
                Err(why) => (Err(why), vec![]),
            }) {
                return (StatusCode::BAD_REQUEST, why).into_response();
            }
            // Saved now: a crash mustn't bring back a live it was switched out of.
            app.save_now().await;
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Store one image (raw body) for a later `Submit`; replies `{"id": ..}`.
/// Acting, so read-only tokens get 403.
async fn upload(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/upload").await {
        return refused.into_response();
    }
    let Some(store) = &app.images else {
        let off = "image attachments are off";
        return (StatusCode::SERVICE_UNAVAILABLE, off).into_response();
    };
    let Ok(bytes) = axum::body::to_bytes(body, MAX_IMAGE_BYTES).await else {
        let why = format!("images are limited to {} MB", MAX_IMAGE_BYTES >> 20);
        return (StatusCode::PAYLOAD_TOO_LARGE, why).into_response();
    };
    let content_type = headers.get(header::CONTENT_TYPE);
    let kind = match images::validate(content_type.and_then(|v| v.to_str().ok()), &bytes) {
        Ok(kind) => kind,
        Err(why) => return (StatusCode::UNSUPPORTED_MEDIA_TYPE, why).into_response(),
    };
    match store.store(kind, &bytes) {
        Ok(id) => Json(serde_json::json!({ "id": id })).into_response(),
        Err(e) => {
            tracing::warn!("storing image: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Keep a link, text and/or images shared from a phone (multipart `url`, `text`, `via`,
/// up to 4 `image` files; or those text fields url-encoded) in the inbox before replying `{"id", "file"}`: 201, or 200
/// for a resend of one already kept. Capture tokens and full access only.
async fn capture(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
) -> Response {
    let path = "/api/capture";
    match app
        .admit_route(req.headers(), peer, path, Route::Capture)
        .await
    {
        Some(Scope::Full | Scope::Capture) => {}
        Some(Scope::Read) | None => return StatusCode::UNAUTHORIZED.into_response(),
    }
    let Some(inbox) = app.captures.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "captures are off").into_response();
    };
    // Parsed only once admitted, so strangers learn nothing about the form.
    // iOS Shortcuts sends multipart only when a file is attached; else url-encoded.
    let url_encoded = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| {
            t.to_ascii_lowercase()
                .starts_with("application/x-www-form-urlencoded")
        });
    let c = if url_encoded {
        match axum::Form::<TextCapture>::from_request(req, &()).await {
            Ok(axum::Form(f)) => text_capture(f),
            // Unknown or repeated fields: 400 like the multipart path, not axum's 422.
            Err(e) => match e.status() {
                StatusCode::UNPROCESSABLE_ENTITY => Err((StatusCode::BAD_REQUEST, e.body_text())),
                status => Err((status, e.body_text())),
            },
        }
    } else {
        match axum::extract::Multipart::from_request(req, &()).await {
            Ok(form) => capture_form(form).await,
            Err(e) => return e.into_response(),
        }
    };
    let c = match c {
        Ok(c) => c,
        Err(refused) => return refused.into_response(),
    };
    // What arrived, for the operator's log: `note` is the first 80 chars of the typed text.
    let note: String = c.text.as_deref().unwrap_or("").chars().take(80).collect();
    let (url, shared, images) = (c.url.clone(), c.shared.is_some(), c.images.len());
    match tokio::task::spawn_blocking(move || inbox.store(&c)).await {
        Ok(Ok(saved)) => {
            let url = url.as_deref().unwrap_or("");
            tracing::info!(file = %saved.file, new = saved.new, url, shared, images, ?note, "captured");
            let status = match saved.new {
                true => StatusCode::CREATED,
                false => StatusCode::OK,
            };
            let body = serde_json::json!({ "id": saved.id, "file": saved.file });
            (status, Json(body)).into_response()
        }
        r => {
            tracing::warn!("storing capture: {r:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A capture form's fields, checked; else why not.
async fn capture_form(
    mut form: axum::extract::Multipart,
) -> Result<capture::Capture, (StatusCode, String)> {
    let refuse = |e: axum::extract::multipart::MultipartError| (e.status(), e.body_text());
    let bad = |why: String| (StatusCode::BAD_REQUEST, why);
    let (mut url, mut text, mut shared, mut via) = (None, None, None, None);
    let mut images = Vec::new();
    while let Some(field) = form.next_field().await.map_err(refuse)? {
        let name = field.name().unwrap_or_default().to_string();
        let slot = match name.as_str() {
            "url" => &mut url,
            "text" => &mut text,
            "shared" => &mut shared,
            "via" => &mut via,
            "image" => {
                images.push(field.bytes().await.map_err(refuse)?.into());
                continue;
            }
            _ => {
                return Err(bad(format!(
                    "unknown field {name:?}: send url, text, shared, image, via"
                )))
            }
        };
        if slot.is_some() {
            return Err(bad(format!("{name} sent twice")));
        }
        *slot = Some(field.text().await.map_err(refuse)?);
    }
    capture::Capture::new(url, text, shared, images, via).map_err(refused_capture)
}

/// A url-encoded capture: the same fields, no files. Duplicates and unknown fields
/// are refused by serde.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TextCapture {
    url: Option<String>,
    text: Option<String>,
    shared: Option<String>,
    via: Option<String>,
    /// Tolerated empty, in case a client sends an empty file field as `image=`.
    image: Option<String>,
}

fn text_capture(f: TextCapture) -> Result<capture::Capture, (StatusCode, String)> {
    if f.image.is_some_and(|i| !i.is_empty()) {
        let why = "images need multipart/form-data".to_string();
        return Err((StatusCode::BAD_REQUEST, why));
    }
    capture::Capture::new(f.url, f.text, f.shared, Vec::new(), f.via).map_err(refused_capture)
}

fn refused_capture(r: capture::Refused) -> (StatusCode, String) {
    match r {
        capture::Refused::Invalid(why) => (StatusCode::BAD_REQUEST, why),
        capture::Refused::TooLarge(why) => (StatusCode::PAYLOAD_TOO_LARGE, why),
        capture::Refused::NotAnImage(why) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, why),
    }
}

/// For `scripts/mis-up --watch`: the update shown to clients, and how many
/// tasks a restart would interrupt (it restarts at 0).
async fn update_status(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !app.admit(&headers, peer, "/api/update").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let l = app.ledger();
    Json(serde_json::json!({ "update": l.update(), "running": l.running() })).into_response()
}

/// The supervisor reports an [`Update`]; clients see it live in Status.
async fn set_update(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/update").await {
        return refused.into_response();
    }
    let update = match serde_json::from_slice::<Update>(&body) {
        Ok(u) => u,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let set = app.transition(|l| match l.set_update(update) {
        Ok(effects) => (Ok(()), effects),
        Err(why) => (Err(why), vec![]),
    });
    match set {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(why) => (StatusCode::BAD_REQUEST, why).into_response(),
    }
}

/// Published apps and their releases. Read-only, like task status.
async fn list_apps(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !app.admit(&headers, peer, "/api/apps").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let list = app.apps.as_ref().map(|a| a.list()).unwrap_or_default();
    Json(list).into_response()
}

#[derive(serde::Deserialize)]
struct PublishParams {
    #[serde(default)]
    activate: bool,
}

/// A `.tar.gz` bundle (raw body) as `slug`'s next release; `?activate=true` makes it live.
/// Replies 201 `{"release": n}`. Full scope only.
async fn publish_app(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Params(params): Params<PublishParams>,
    body: axum::body::Body,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/apps").await {
        return refused.into_response();
    }
    let Some(apps) = app.apps.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "apps are off: no state dir",
        )
            .into_response();
    };
    if !apps::valid_slug(&slug) {
        let why = "app names are a-z, 0-9 and -, up to 32, starting with a letter or digit";
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(body, apps::MAX_ARCHIVE).await else {
        let why = format!("bundles are limited to {} MiB", apps::MAX_BYTES >> 20);
        return (StatusCode::PAYLOAD_TOO_LARGE, why).into_response();
    };
    let at = unix_ms();
    let published = tokio::task::spawn_blocking(move || {
        let bundle = apps::unpack(&bytes).map_err(|why| (StatusCode::BAD_REQUEST, why))?;
        let n = apps.publish(&slug, &bundle, at, params.activate);
        n.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
    });
    match published.await.expect("publish doesn't panic") {
        Ok(n) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "release": n })),
        )
            .into_response(),
        Err((status, why)) => {
            if status.is_server_error() {
                tracing::warn!("publishing an app: {why}");
            }
            (status, why).into_response()
        }
    }
}

/// `{"release": n}` becomes `slug`'s live release; an earlier n reverts. Full scope only.
async fn activate_app(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/apps").await {
        return refused.into_response();
    }
    let Some(apps) = &app.apps else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "apps are off: no state dir",
        )
            .into_response();
    };
    #[derive(serde::Deserialize)]
    struct Activate {
        release: u32,
    }
    let n = match serde_json::from_slice::<Activate>(&body) {
        Ok(a) => a.release,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    if !apps::valid_slug(&slug) {
        return (StatusCode::NOT_FOUND, "no such release").into_response();
    }
    let apps = apps.clone();
    let activated = tokio::task::spawn_blocking(move || apps.activate(&slug, n));
    match activated.await.expect("activating doesn't panic") {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(apps::ActivateError::NoSuchRelease) => {
            (StatusCode::NOT_FOUND, "no such release").into_response()
        }
        Err(apps::ActivateError::Io(e)) => {
            tracing::warn!("activating an app: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// An uploaded image, for thumbnails. `id` parses as a Uuid, so it can't name a path.
async fn image(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if !app.admit(&headers, peer, "/api/images").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match app.images.as_ref().and_then(|s| s.read(id)) {
        Some((kind, bytes)) => (
            [
                (header::CONTENT_TYPE, kind.mime()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// An evidence file, as stored: its sniffed type, never sniffed again by the
/// browser, and never run as a page (no scripts, styles or frames load).
async fn evidence_file(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
) -> Response {
    if !app.admit(&headers, peer, "/api/evidence").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match app.evidence.as_ref().and_then(|s| s.read(id)) {
        Some((kind, bytes)) => (
            [
                (header::CONTENT_TYPE, kind.content_type()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
                (header::CONTENT_DISPOSITION, "inline"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// What a task changed in its repo (`TaskDiff`, base left out), as JSON; 404
/// until its worker sends one. Auth as for evidence.
async fn task_diff(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(task): Path<TaskId>,
) -> Response {
    if !app.admit(&headers, peer, "/api/tasks/diff").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match app.evidence.as_ref().and_then(|s| s.read_diff(task)) {
        Some(json) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
            ],
            json,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ListParams {
    /// Also list `claude -p` / `codex exec` runs.
    #[serde(default)]
    headless: bool,
}

/// Every worker's live sessions (`SessionRow`s) in Home's order, offline workers' as last
/// reported. Session data needs full access: read-only tokens get 403.
async fn live_sessions(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/sessions").await {
        return refused.into_response();
    }
    Json(app.ledger().sessions()).into_response()
}

/// Worker `id`'s Claude and Codex sessions, newest first: read from its disk
/// now, never kept here. Full access only, like transcripts.
async fn sessions(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<WorkerId>,
    Params(p): Params<ListParams>,
) -> Response {
    if let Err(refused) = app
        .admit_full(&headers, peer, "/api/workers/sessions")
        .await
    {
        return refused.into_response();
    }
    let query = Query::ListSessions {
        headless: p.headless,
    };
    match app.ask(id, query).await {
        Ok(Answer::Sessions { sessions }) => Json(sessions).into_response(),
        Ok(_) => StatusCode::BAD_GATEWAY.into_response(),
        Err(why) => why.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ReadParams {
    limit: Option<usize>,
}

/// Turns sent when `?limit=` is absent.
const TURNS: usize = 30;

/// The last turns of one session on worker `wid`, read from its disk now. `session`
/// must be a UUID (400 otherwise), so it can't name a path. Full access only.
async fn session(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((wid, harness, id)): Path<(WorkerId, String, uuid::Uuid)>,
    Params(p): Params<ReadParams>,
) -> Response {
    if let Err(refused) = app
        .admit_full(&headers, peer, "/api/workers/sessions")
        .await
    {
        return refused.into_response();
    }
    let limit = p.limit.unwrap_or(TURNS).clamp(1, MAX_TURNS);
    match app
        .ask(wid, Query::ReadSession { harness, id, limit })
        .await
    {
        Ok(t @ Answer::Transcript { .. }) => Json(t).into_response(),
        Ok(Answer::NotFound) => (StatusCode::NOT_FOUND, "unknown session").into_response(),
        Ok(_) => StatusCode::BAD_GATEWAY.into_response(),
        Err(why) => why.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct SessionMessage {
    text: String,
    #[serde(default = "deliver")]
    mode: SendMode,
    /// The session's title, as listed, for the task's heading.
    #[serde(default)]
    title: Option<String>,
}

fn deliver() -> SendMode {
    SendMode::Deliver
}

/// Message one of worker `wid`'s sessions: a task, pinned there, that continues it.
/// Acting, like submitting a task: read-only tokens get 403. Replies `{"task": id}`.
async fn message(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((wid, harness, session)): Path<(WorkerId, String, uuid::Uuid)>,
    body: Bytes,
) -> Response {
    if let Err(refused) = app
        .admit_full(&headers, peer, "/api/workers/sessions")
        .await
    {
        return refused.into_response();
    }
    let m = match serde_json::from_slice::<SessionMessage>(&body) {
        Ok(m) if m.mode == SendMode::Fork => {
            let why = "messages go into the session itself, not a copy";
            return (StatusCode::BAD_REQUEST, why).into_response();
        }
        Ok(m) if !m.text.trim().is_empty() => m,
        Ok(_) => return (StatusCode::BAD_REQUEST, "empty message").into_response(),
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let to = SendTo {
        session,
        title: m.title.map(|t| first_line(&t, 120)),
        mode: m.mode,
    };
    let sent = app.transition(|l| match l.worker_conn(wid) {
        None => (
            Err((StatusCode::NOT_FOUND, "unknown worker".into())),
            vec![],
        ),
        Some(None) => (
            Err((StatusCode::SERVICE_UNAVAILABLE, "worker offline".into())),
            vec![],
        ),
        Some(Some(_)) => match l.send_to_session(wid, harness, m.text, to, vec![]) {
            Ok((id, effects)) => (Ok(id), effects),
            Err(why) => (Err((StatusCode::BAD_REQUEST, why)), vec![]),
        },
    });
    match sent {
        Ok(task) => Json(serde_json::json!({ "task": task })).into_response(),
        Err::<_, (StatusCode, String)>(why) => why.into_response(),
    }
}

/// Cancel a task. Acting, like submitting one: read-only tokens get 403. Replies
/// `{"task": id, "status": s}`: `cancelled`, or `running` while its worker stops it.
/// 404 for an unknown task; 409 when it already ended or its worker is offline.
async fn cancel(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(task): Path<TaskId>,
) -> Response {
    if let Err(refused) = app.admit_full(&headers, peer, "/api/tasks/cancel").await {
        return refused.into_response();
    }
    let done = app.transition(|l| match l.cancel(task) {
        Ok((status, effects)) => (Ok(status), effects),
        Err(why) => (Err(why), vec![]),
    });
    match done {
        Ok(status) => Json(serde_json::json!({ "task": task, "status": status })).into_response(),
        Err(NotCancelled::Unknown) => (StatusCode::NOT_FOUND, "unknown task").into_response(),
        Err(NotCancelled::Finished(s)) => {
            let s = format!("{s:?}").to_lowercase();
            (StatusCode::CONFLICT, format!("already {s}")).into_response()
        }
        Err(NotCancelled::Offline) => {
            let why = "its worker is offline; cancel once it reconnects";
            (StatusCode::CONFLICT, why).into_response()
        }
    }
}

async fn ws_worker(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if app.admit_scope(&headers, peer, "/ws/worker").await != Some(Scope::Full) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.max_frame_size(WORKER_FRAME)
        .on_upgrade(move |sock| worker_conn(sock, app))
        .into_response()
}

async fn ws_client(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(scope) = app.admit_scope(&headers, peer, "/ws/client").await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // Browsers abort unless the server echoes a protocol they offered.
    ws.protocols(["bearer"])
        .on_upgrade(move |sock| client_conn(sock, app, scope))
        .into_response()
}

async fn recv_json<T: serde::de::DeserializeOwned>(sock: &mut WebSocket) -> Option<T> {
    while let Some(Ok(msg)) = sock.next().await {
        if let Message::Text(t) = msg {
            match serde_json::from_str(&t) {
                Ok(v) => return Some(v),
                Err(e) => tracing::warn!("bad message: {e}"),
            }
        }
    }
    None
}

async fn send_json<T: serde::Serialize>(sock: &mut WebSocket, v: &T) -> bool {
    sock.send(Message::Text(serde_json::to_string(v).unwrap().into()))
        .await
        .is_ok()
}

async fn worker_conn(mut sock: WebSocket, app: App) {
    let Some(WorkerMsg::Hello {
        id: wid,
        info,
        running,
    }) = recv_json::<WorkerMsg>(&mut sock).await
    else {
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let conn = app.open_conn(tx);
    let unknown = app.transition(|l| l.connect(wid, info, &running, conn, app.now()));
    tracing::info!(%wid, conn, ?running, "worker joined");
    if !unknown.is_empty() {
        tracing::warn!(%wid, ?unknown, "worker reports tasks unknown to this orchestrator");
    }
    let welcome = OrchMsg::Welcome {
        id: wid,
        version: Some(VERSION.into()),
        channel: app.channel,
    };
    if send_json(&mut sock, &welcome).await {
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(out) => if !send_json(&mut sock, &out).await { break; },
                    None => break, // closed by `Effect::Close`
                },
                incoming = recv_json::<WorkerMsg>(&mut sock) => match incoming {
                    Some(WorkerMsg::Event { task, event: AgentEvent::Artifact(draft) }) => {
                        app.attach(wid, conn, task, draft).await;
                    }
                    Some(WorkerMsg::Event { task, event: AgentEvent::Diff(diff) }) => {
                        app.diffed(wid, conn, task, diff).await;
                    }
                    Some(WorkerMsg::Event { task, event }) => {
                        app.transition(|l| ((), l.event(wid, conn, task, event)));
                    }
                    Some(WorkerMsg::Stats(stats)) => {
                        let now = app.now();
                        app.transition(|l| ((), l.worker_stats(wid, conn, stats, now, unix_secs())));
                    }
                    Some(WorkerMsg::PrStatus { task, url, status }) => {
                        app.transition(|l| ((), l.pr_status(wid, conn, task, &url, status)));
                    }
                    Some(WorkerMsg::PrUnavailable { task, why }) => {
                        app.transition(|l| ((), l.pr_unavailable(wid, conn, task, &why)));
                    }
                    Some(WorkerMsg::Purpose { purpose }) => {
                        app.transition(|l| ((), l.worker_purpose(wid, conn, purpose)));
                    }
                    Some(WorkerMsg::PurposeFailed { why }) => {
                        app.transition(|l| ((), l.purpose_failed(wid, conn, &why)));
                    }
                    Some(WorkerMsg::Update { update }) => {
                        app.transition(|l| ((), l.worker_update(wid, conn, update)));
                    }
                    Some(WorkerMsg::ApprovalRequest { task, approval }) => {
                        app.transition(|l| ((), l.approval_request(wid, conn, task, approval)));
                    }
                    Some(WorkerMsg::ApprovalClosed { task, id }) => {
                        app.transition(|l| ((), l.approval_closed(wid, conn, task, id)));
                    }
                    Some(WorkerMsg::Permissions { permissions }) => {
                        app.transition(|l| ((), l.worker_permissions(wid, conn, permissions)));
                    }
                    Some(WorkerMsg::Answer { req, answer }) => app.answered(conn, req, answer),
                    Some(WorkerMsg::LiveSessions { sessions }) => {
                        app.transition(|l| ((), l.live_sessions(wid, conn, sessions)));
                    }
                    Some(WorkerMsg::Hello { .. }) => {}
                    None => break,
                }
            }
        }
    }
    tracing::info!(%wid, conn, "worker left");
    app.close_conn(wid, conn);
}

/// Next broadcast event; a lagged subscriber gets an Error instead of silently stalling.
async fn next_event(sub: Option<&mut broadcast::Receiver<ServerMsg>>) -> ServerMsg {
    let Some(r) = sub else {
        return std::future::pending().await;
    };
    loop {
        match r.recv().await {
            Ok(ev) => return ev,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                return ServerMsg::Error {
                    message: format!("dropped {n} events; re-run status"),
                }
            }
            Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
        }
    }
}

async fn client_conn(mut sock: WebSocket, app: App, scope: Scope) {
    let mut sub: Option<broadcast::Receiver<ServerMsg>> = None;
    // Sent `WatchSessions`: gets `ServerMsg::Sessions`, which others never see.
    let mut watching = false;
    let start = tokio::time::Instant::now() + app.ping_every;
    let mut ping = tokio::time::interval_at(start, app.ping_every);
    loop {
        // Drain queued events before handling a request, so a `Show` snapshot is not
        // followed by stale copies of events it already contains.
        tokio::select! {
            biased;
            ev = next_event(sub.as_mut()) => {
                if matches!(ev, ServerMsg::Sessions { .. }) && !watching { continue; }
                if !send_json(&mut sock, &ev).await { break; }
            }
            _ = ping.tick() => {
                if sock.send(Message::Ping(Bytes::new())).await.is_err() { break; }
            }
            incoming = recv_json::<ClientMsg>(&mut sock) => match incoming {
                Some(msg) if !scope::allowed(scope, &msg) => {
                    let refused = ServerMsg::Error { message: "read-only token".into() };
                    send_json(&mut sock, &refused).await;
                }
                Some(ClientMsg::Status) => {
                    let status = app.ledger().status();
                    send_json(&mut sock, &status).await;
                }
                Some(ClientMsg::Submit { prompt, reply_to: Some(parent), images, .. }) => {
                    let planner = app.planner.is_some();
                    let task = app.check_images(&images).and_then(|()| {
                        app.transition(|l| match l.reply(parent, prompt, images, planner) {
                            Ok((task, effects)) => (Ok(task), effects),
                            Err(why) => (Err(why), vec![]),
                        })
                    });
                    let reply = match task {
                        Ok(task) => ServerMsg::Submitted { task },
                        Err(message) => ServerMsg::Error { message },
                    };
                    send_json(&mut sock, &reply).await;
                }
                Some(ClientMsg::Submit { prompt, cwd, harness, model, reply_to: None, images }) => {
                    let model = model.filter(|m| !m.trim().is_empty());
                    // A chosen model pins the default harness: the planner could pick
                    // one that doesn't know that model.
                    let unplanned = app.planner.is_none() || model.is_some();
                    let harness = harness.or_else(|| unplanned.then(|| "claude".into()));
                    let reply = match app.check_images(&images) {
                        Ok(()) => ServerMsg::Submitted {
                            task: app.transition(|l| l.submit(prompt, cwd, harness, model, images)),
                        },
                        Err(message) => ServerMsg::Error { message },
                    };
                    send_json(&mut sock, &reply).await;
                }
                Some(ClientMsg::Subscribe) => sub = Some(app.events.subscribe()),
                Some(ClientMsg::WatchSessions) => {
                    if !watching {
                        watching = true;
                        app.watchers.fetch_add(1, Ordering::Relaxed);
                    }
                    // Subscribed first, so no change after this snapshot is missed.
                    sub.get_or_insert_with(|| app.events.subscribe());
                    // Not counted as pushed: a change still in flight reaches everyone.
                    let sessions = app.ledger().sessions();
                    send_json(&mut sock, &ServerMsg::Sessions { sessions }).await;
                }
                Some(ClientMsg::Seen { task }) => app.transition(|l| ((), l.seen(task))),
                Some(ClientMsg::Show { task: id }) => {
                    let reply = {
                        let ledger = app.ledger();
                        match ledger.show(id) {
                            Some((task, events, truncated)) => ServerMsg::TaskDetail {
                                task,
                                events: events.to_vec(),
                                truncated,
                                partial: ledger.partial(id),
                            },
                            None => ServerMsg::Error { message: format!("unknown task {id}") },
                        }
                    };
                    send_json(&mut sock, &reply).await;
                }
                Some(ClientMsg::PushSubscribe { endpoint, p256dh, auth }) => {
                    let sub = push::Subscription { endpoint, p256dh, auth };
                    let refused = match &app.push {
                        Some(p) => p.subscribe(sub).err(),
                        None => Some("push notifications are off".into()),
                    };
                    if let Some(message) = refused {
                        send_json(&mut sock, &ServerMsg::Error { message }).await;
                    }
                }
                Some(ClientMsg::PushUnsubscribe { endpoint }) => {
                    if let Some(p) = &app.push {
                        p.unsubscribe(&endpoint);
                    }
                }
                Some(ClientMsg::SetPurpose { worker, purpose }) => {
                    let refused = app.transition(|l| match l.set_purpose(worker, &purpose) {
                        Ok(effects) => (None, effects),
                        Err(why) => (Some(why), vec![]),
                    });
                    if let Some(message) = refused {
                        send_json(&mut sock, &ServerMsg::Error { message }).await;
                    }
                }
                Some(ClientMsg::Approve { task, id, decision }) => {
                    let refused = app.transition(|l| match l.approve(task, id, decision) {
                        Ok(effects) => (None, effects),
                        Err(why) => (Some(why), vec![]),
                    });
                    if let Some(message) = refused {
                        send_json(&mut sock, &ServerMsg::Error { message }).await;
                    }
                }
                Some(ClientMsg::SuggestPurpose { worker }) => {
                    let refused = match app.drafter {
                        None => Some("suggestions need the planner (claude on the orchestrator)".into()),
                        Some(_) => app.transition(|l| match l.suggest_purpose(worker) {
                            Ok(effects) => (None, effects),
                            Err(why) => (Some(why.to_string()), vec![]),
                        }),
                    };
                    if let Some(message) = refused {
                        send_json(&mut sock, &ServerMsg::Error { message }).await;
                    }
                }
                Some(ClientMsg::SessionBell { worker, harness, id, on }) => {
                    let key = ledger::SessionKey { worker, harness, id };
                    let refused = app.transition(|l| match l.session_bell(key, on) {
                        Ok(effects) => (None, effects),
                        Err(why) => (Some(why), vec![]),
                    });
                    if let Some(message) = refused {
                        send_json(&mut sock, &ServerMsg::Error { message }).await;
                    }
                }
                Some(ClientMsg::RefreshEvidence { task }) => {
                    let refused = app.transition(|l| match l.refresh_evidence(task) {
                        Ok(effects) => (None, effects),
                        Err(why) => (Some(why), vec![]),
                    });
                    if let Some(message) = refused {
                        send_json(&mut sock, &ServerMsg::Error { message }).await;
                    }
                }
                None => break,
            },
        }
    }
    if watching {
        app.watchers.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_origin_is_allowed() {
        assert!(origin_allowed(None, &[]));
        assert!(origin_allowed(None, &list(&["https://x.com"])));
    }

    #[test]
    fn origin_must_match_exactly() {
        let allowed = list(&["https://x.com", "http://localhost:3000"]);
        assert!(origin_allowed(Some("https://x.com"), &allowed));
        assert!(origin_allowed(Some("http://localhost:3000"), &allowed));
        for o in [
            "https://evil.example",
            "http://x.com",
            "https://X.com",
            "https://x.com/",
            "https://x.com:8443",
            "https://x.com.evil.example",
            "http://localhost:3001",
            "null",
        ] {
            assert!(!origin_allowed(Some(o), &allowed), "{o}");
        }
        assert!(!origin_allowed(Some("https://x.com"), &[]));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (axum::http::HeaderName::from_static(k), v.parse().unwrap()))
            .collect()
    }

    #[test]
    fn bearer_from_either_header() {
        let b = |pairs: &[(&'static str, &str)]| bearer_from(&headers(pairs)).map(str::to_owned);
        assert_eq!(b(&[("authorization", "Bearer s")]).as_deref(), Some("s"));
        assert_eq!(
            b(&[("sec-websocket-protocol", "bearer, s")]).as_deref(),
            Some("s")
        );
        assert_eq!(
            b(&[("sec-websocket-protocol", "bearer,s")]).as_deref(),
            Some("s")
        );
        assert_eq!(b(&[("authorization", "Basic s")]), None);
        assert_eq!(b(&[("sec-websocket-protocol", "bearer")]), None);
        assert_eq!(b(&[("sec-websocket-protocol", "chat, s")]), None);
        assert_eq!(b(&[("sec-websocket-protocol", "s, bearer")]), None);
        assert_eq!(b(&[]), None);
    }

    #[test]
    fn same_origin_matches_host() {
        assert!(same_origin("https://x:443", "x:443"));
        assert!(same_origin("http://127.0.0.1:7474", "127.0.0.1:7474"));
        assert!(same_origin("https://X.com", "x.com"));
        assert!(!same_origin("https://x:443", "y:443"));
        assert!(!same_origin("https://x:443", "x:444"));
        assert!(!same_origin("https://x.evil", "x"));
        assert!(!same_origin("null", "null"));
        assert!(!same_origin("x:443", "x:443"));
    }

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost:7474",
            "127.0.0.1:7474",
            "127.0.0.1",
            "[::1]:7474",
            "LOCALHOST",
        ] {
            assert!(loopback_host(h), "{h}");
        }
        for h in [
            "evil.com:7474",
            "192.168.1.2:7474",
            "localhost.evil.com",
            "",
        ] {
            assert!(!loopback_host(h), "{h}");
        }
    }

    #[test]
    fn authorized_combines_origin_and_token() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let ok = |app: &App, pairs: &[(&'static str, &str)]| {
            app.authorized(&headers(pairs), lo, &Access::Off).is_some()
        };
        let open = App::new(None);
        // Same-origin page needs no allowlist entry.
        let local = [
            ("origin", "http://127.0.0.1:7474"),
            ("host", "127.0.0.1:7474"),
        ];
        assert!(ok(&open, &local));
        // No token: a rebound non-loopback name is not trusted as same-origin.
        let rebound = [
            ("origin", "http://evil.com:7474"),
            ("host", "evil.com:7474"),
        ];
        assert!(!ok(&open, &rebound));
        // Missing Host falls through to the (empty) allowlist.
        assert!(!ok(&open, &local[..1]));
        let t = App::new(Some("s".into()));
        let (o, h) = (("origin", "https://m.example"), ("host", "m.example"));
        assert!(ok(&t, &[o, h, ("sec-websocket-protocol", "bearer, s")]));
        assert!(!ok(&t, &[o, h, ("sec-websocket-protocol", "bearer, x")]));
        assert!(!ok(&t, &[o, h]));
        // Right token from a foreign page is still rejected.
        let foreign = ("origin", "https://evil.example");
        assert!(!ok(
            &t,
            &[foreign, h, ("sec-websocket-protocol", "bearer, s")]
        ));
    }

    #[test]
    fn authorized_behind_access() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let mapped_lo: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let remote: IpAddr = "192.0.2.1".parse().unwrap();
        let t = App::new(Some("s".into()));
        let bearer = ("authorization", "Bearer s");
        let pass = || {
            Access::Passed(Identity {
                subject: "me".into(),
            })
        };
        let ok = |pairs: &[(&'static str, &str)], peer, passed: bool| {
            let access = if passed { pass() } else { Access::Failed };
            t.authorized(&headers(pairs), peer, &access).is_some()
        };
        // Valid JWT: no token needed, from anywhere.
        assert!(ok(&[], remote, true));
        assert!(ok(&[(JWT_HEADER, "j")], lo, true));
        // No valid JWT: only a direct local connection, and it still needs the token.
        assert!(ok(&[bearer], lo, false));
        assert!(ok(&[bearer], mapped_lo, false));
        assert!(!ok(&[], lo, false));
        assert!(!ok(&[bearer], remote, false));
        // A LAN peer is direct too: token required, Cloudflare headers not allowed.
        for lan in [
            "192.168.4.20",
            "10.0.0.5",
            "172.16.1.1",
            "fd00::1",
            "fe80::1",
        ] {
            let lan: IpAddr = lan.parse().unwrap();
            assert!(ok(&[bearer], lan, false), "{lan}");
            assert!(!ok(&[], lan, false), "{lan}");
            assert!(!ok(&[bearer, ("cf-ray", "x")], lan, false), "{lan}");
        }
        // Through cloudflared (loopback peer, Cloudflare headers): never falls back.
        assert!(!ok(&[bearer, (JWT_HEADER, "bad")], lo, false));
        assert!(!ok(&[bearer, ("cf-ray", "x")], lo, false));
        assert!(!ok(&[bearer, ("cf-connecting-ip", "192.0.2.1")], lo, false));
        // A valid JWT does not bypass the origin check; its own page passes.
        let foreign = [("origin", "https://evil.example"), ("host", "m.example")];
        assert!(!ok(&foreign, remote, true));
        let open = App::new(None);
        let page = [("origin", "https://m.example"), ("host", "m.example")];
        assert!(open.authorized(&headers(&page), remote, &pass()).is_some());
        assert!(open
            .authorized(&headers(&page), lo, &Access::Failed)
            .is_none());
        // A read token is not enough from the LAN.
        let ro = App::new(Some("s".into())).with_read_tokens(list(&["r"]));
        let lan: IpAddr = "192.168.4.20".parse().unwrap();
        let read = headers(&[("authorization", "Bearer r")]);
        assert!(ro.authorized(&read, lan, &Access::Failed).is_none());
        assert!(ro.authorized(&read, lo, &Access::Failed).is_some());
        // Without a token, LAN peers are not trusted.
        assert!(open
            .authorized(&headers(&[]), lan, &Access::Failed)
            .is_none());
    }

    #[test]
    fn capture_credentials_reach_only_the_capture_route() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let remote: IpAddr = "192.0.2.1".parse().unwrap();
        let t = App::new(Some("s".into()))
            .with_read_tokens(list(&["r"]))
            .with_capture_tokens(list(&["c"]));
        let scope = |bearer: Option<&str>, peer, access: &Access, route| {
            let auth = bearer.map(|b| format!("Bearer {b}"));
            let pairs: Vec<_> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
            t.authorized_on(&headers(&pairs), peer, access, route)
        };
        use Route::{Capture, Other};
        // Without Access: none / read / capture / full on each route.
        for (bearer, capture, other) in [
            (None, None, None),
            (Some("r"), None, Some(Scope::Read)),
            (Some("c"), Some(Scope::Capture), None),
            (Some("s"), Some(Scope::Full), Some(Scope::Full)),
        ] {
            assert_eq!(
                scope(bearer, lo, &Access::Off, Capture),
                capture,
                "{bearer:?}"
            );
            assert_eq!(scope(bearer, lo, &Access::Off, Other), other, "{bearer:?}");
        }
        let id = || Identity {
            subject: "phone".into(),
        };
        // The capture app's JWT: capture route only, at most capturing, plus a token.
        let cap = Access::Capture(id());
        assert_eq!(
            scope(Some("c"), remote, &cap, Capture),
            Some(Scope::Capture)
        );
        assert_eq!(
            scope(Some("s"), remote, &cap, Capture),
            Some(Scope::Capture)
        );
        assert_eq!(scope(Some("r"), remote, &cap, Capture), None);
        assert_eq!(scope(None, remote, &cap, Capture), None);
        for peer in [remote, lo] {
            for b in [None, Some("c"), Some("s"), Some("r")] {
                assert_eq!(scope(b, peer, &cap, Other), None, "{peer} {b:?}");
            }
        }
        // A main-app login is full, but never upgrades a capture token.
        let main = Access::Passed(id());
        assert_eq!(scope(None, remote, &main, Other), Some(Scope::Full));
        assert_eq!(scope(None, remote, &main, Capture), Some(Scope::Full));
        assert_eq!(scope(Some("c"), remote, &main, Other), None);
        assert_eq!(
            scope(Some("c"), remote, &main, Capture),
            Some(Scope::Capture)
        );
        // Its own hostname, no Origin (a Shortcut): admitted.
        let phone = headers(&[("host", "capture.m6o.app"), ("authorization", "Bearer c")]);
        let got = t.authorized_on(&phone, remote, &cap, Capture);
        assert_eq!(got, Some(Scope::Capture));
        // No valid JWT from afar: a capture token alone is not enough.
        assert_eq!(scope(Some("c"), remote, &Access::Failed, Capture), None);
    }

    #[test]
    fn read_tokens_get_read_scope() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let t = App::new(Some("s".into())).with_read_tokens(list(&["r1", "r2"]));
        let scope = |bearer: &str, access: &Access| {
            let auth = format!("Bearer {bearer}");
            t.authorized(&headers(&[("authorization", &auth)]), lo, access)
        };
        assert_eq!(scope("s", &Access::Off), Some(Scope::Full));
        assert_eq!(scope("r1", &Access::Off), Some(Scope::Read));
        assert_eq!(scope("r2", &Access::Off), Some(Scope::Read));
        assert_eq!(scope("r3", &Access::Off), None);
        assert_eq!(scope("", &Access::Off), None);
        let blank = App::new(Some("s".into())).with_read_tokens(list(&[""]));
        let page = [("sec-websocket-protocol", "bearer,")];
        assert_eq!(blank.authorized(&headers(&page), lo, &Access::Off), None);
        // An Access login is full, whatever token rides along.
        let passed = Access::Passed(Identity {
            subject: "me".into(),
        });
        assert_eq!(scope("r1", &passed), Some(Scope::Full));
        // Via the page's subprotocol too.
        let page = [("sec-websocket-protocol", "bearer, r1")];
        assert_eq!(
            t.authorized(&headers(&page), lo, &Access::Off),
            Some(Scope::Read)
        );
    }

    #[test]
    fn subjects_listed_case_insensitively_or_all_when_unset() {
        assert!(subject_listed(&[], "anyone@example.com"));
        let listed = list(&["me@example.com"]);
        assert!(subject_listed(&listed, "Me@Example.com"));
        assert!(!subject_listed(&listed, "guest@example.com"));
        assert!(!subject_listed(&listed, ""));
    }

    #[test]
    fn normalizes_entries() {
        assert_eq!(
            normalize_origins(list(&[" HTTPS://X.com/ ", "", "  ", "http://a:1"])),
            list(&["https://x.com", "http://a:1"])
        );
    }
}

//! Headless orchestrator: tracks workers, queues tasks, assigns them to idle
//! workers with a matching harness, and fans agent events out to clients.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use mis_types::*;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

/// `0.1.0+abc1234`, with `-dirty` when built from uncommitted changes.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("MIS_GIT_REV"));

pub mod access;
pub mod access_gate;
pub mod ledger;
pub mod plan;
pub mod summary;

/// A stuck planner (auth prompt, network stall) fails the task instead of
/// leaving it in Planning forever.
const PLANNER_TIMEOUT: Duration = Duration::from_secs(90);
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(60);

use access::Identity;
use access_gate::{AccessGate, JWT_HEADER};
use ledger::{ConnId, Effect, Ledger};
use mis_agent::Agent;

#[derive(Clone)]
pub struct App {
    ledger: Arc<Mutex<Ledger>>,
    conns: Arc<Mutex<HashMap<ConnId, mpsc::UnboundedSender<OrchMsg>>>>,
    next_conn: Arc<AtomicU64>,
    events: broadcast::Sender<ServerMsg>,
    token: Option<String>,
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
    /// A worker silent (no Hello/Stats) this long is treated as disconnected.
    stale_after: Duration,
    /// How often to check for silent workers.
    tick_every: Duration,
    /// Origin of the ledger's monotonic `now`.
    started: Instant,
    /// Cloudflare Access in front: remote requests need its JWT.
    access: Option<Arc<AccessGate>>,
}

impl App {
    /// `token`, if set, must be presented as `Authorization: Bearer <token>`.
    pub fn new(token: Option<String>) -> Self {
        Self {
            ledger: Default::default(),
            conns: Default::default(),
            next_conn: Default::default(),
            events: broadcast::channel(256).0,
            token,
            allowed_origins: Vec::new(),
            grace: Duration::from_secs(60),
            planner: None,
            planner_slots: Arc::new(tokio::sync::Semaphore::new(2)),
            summarizer: None,
            summary_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            // Two missed beats plus slack for network jitter.
            stale_after: 2 * STATS_INTERVAL + Duration::from_secs(5),
            tick_every: Duration::from_secs(5),
            started: Instant::now(),
            access: None,
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

    pub fn with_planner(mut self, planner: Arc<dyn Agent>) -> Self {
        self.planner = Some(planner);
        self
    }

    pub fn with_summarizer(mut self, summarizer: Arc<dyn Agent>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// The locked ledger, told the wall-clock time for whatever happens next.
    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        let mut l = self.ledger.lock().unwrap();
        l.now_ms = unix_ms();
        l
    }

    /// Run a ledger transition under the lock, then perform its effects outside it.
    fn transition<R>(&self, f: impl FnOnce(&mut Ledger) -> (R, Vec<Effect>)) -> R {
        let (r, effects) = f(&mut self.ledger());
        self.apply(effects);
        r
    }

    fn apply(&self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Send { conn, msg } => {
                    if let Some(tx) = self.conns.lock().unwrap().get(&conn) {
                        let _ = tx.send(msg);
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
        self.transition(|l| ((), l.disconnect(wid, conn)));
    }

    /// None when Access is off; else the identity from a valid Access JWT, if any.
    async fn access_check(&self, headers: &HeaderMap) -> Option<Option<Identity>> {
        let gate = self.access.as_ref()?;
        let Some(jwt) = headers.get(JWT_HEADER) else {
            return Some(None);
        };
        match gate.verify(jwt.to_str().unwrap_or_default()).await {
            Ok(id) => Some(Some(id)),
            Err(e) => {
                tracing::warn!("rejected Cloudflare Access JWT: {e:?}");
                Some(None)
            }
        }
    }

    /// Auth for a WebSocket upgrade; logs who Cloudflare Access let through.
    async fn admit(&self, headers: &HeaderMap, peer: SocketAddr, path: &str) -> bool {
        let access = self.access_check(headers).await;
        let ok = self.authorized(headers, peer.ip(), access.as_ref().map(Option::is_some));
        if let (true, Some(Some(id))) = (ok, &access) {
            tracing::info!(subject = %id.subject, "{path}: Cloudflare Access");
        }
        ok
    }

    /// `access`: None when Access is off, else whether a valid Access JWT came with
    /// the request. `peer` is the TCP peer.
    fn authorized(&self, headers: &HeaderMap, peer: IpAddr, access: Option<bool>) -> bool {
        let passed = access == Some(true);
        // Behind Access only direct local connections may skip it. cloudflared also
        // connects from loopback, but always adds Cloudflare's headers. (Any local
        // process can still connect directly, as without Access.)
        if access == Some(false) && (!peer.to_canonical().is_loopback() || via_cloudflare(headers))
        {
            return false;
        }
        let origin = match headers.get("origin").map(|v| v.to_str()) {
            None => None,
            Some(Ok(o)) => Some(o),
            Some(Err(_)) => return false,
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
            return false;
        }
        match &self.token {
            None => true,
            Some(t) => passed || bearer_from(headers) == Some(t.as_str()),
        }
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
        mis_agent::run_guarded(agent, task, prompt, None, None, None, tx),
    );
    let feed = async {
        while let Some(ev) = rx.recv().await {
            on_event(ev);
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

/// Request came through Cloudflare's edge (e.g. via cloudflared).
fn via_cloudflare(headers: &HeaderMap) -> bool {
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

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/info", get(info))
        .route("/ws/worker", get(ws_worker))
        .route("/ws/client", get(ws_client))
        .with_state(app)
}

/// Wall-clock seconds, only for display (`stats_at`).
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
    app.spawn_liveness();
    let svc = router(app).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, svc).await?;
    Ok(())
}

/// The phone page. Not framable: it acts with the token stored for this origin.
async fn page() -> impl IntoResponse {
    (
        [(header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'")],
        Html(include_str!("../static/index.html")),
    )
}

/// Lets the page decide whether to ask for a token (not when Access already let
/// it in), and shows what is running.
async fn info(State(app): State<App>, headers: HeaderMap) -> Json<serde_json::Value> {
    let access = matches!(app.access_check(&headers).await, Some(Some(_)));
    Json(serde_json::json!({
        "auth": app.token.is_some() && !access,
        "access": access,
        "version": VERSION,
    }))
}

async fn ws_worker(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !app.admit(&headers, peer, "/ws/worker").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |sock| worker_conn(sock, app))
        .into_response()
}

async fn ws_client(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !app.admit(&headers, peer, "/ws/client").await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Browsers abort unless the server echoes a protocol they offered.
    ws.protocols(["bearer"])
        .on_upgrade(move |sock| client_conn(sock, app))
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
    if send_json(&mut sock, &OrchMsg::Welcome { id: wid }).await {
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(out) => if !send_json(&mut sock, &out).await { break; },
                    None => break, // closed by `Effect::Close`
                },
                incoming = recv_json::<WorkerMsg>(&mut sock) => match incoming {
                    Some(WorkerMsg::Event { task, event }) => {
                        app.transition(|l| ((), l.event(wid, conn, task, event)));
                    }
                    Some(WorkerMsg::Stats(stats)) => {
                        let now = app.now();
                        app.ledger().worker_stats(wid, conn, stats, now, unix_secs());
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

async fn client_conn(mut sock: WebSocket, app: App) {
    let mut sub: Option<broadcast::Receiver<ServerMsg>> = None;
    loop {
        // Drain queued events before handling a request, so a `Show` snapshot is not
        // followed by stale copies of events it already contains.
        tokio::select! {
            biased;
            ev = next_event(sub.as_mut()) => {
                if !send_json(&mut sock, &ev).await { break; }
            }
            incoming = recv_json::<ClientMsg>(&mut sock) => match incoming {
                Some(ClientMsg::Status) => {
                    let status = app.ledger().status();
                    send_json(&mut sock, &status).await;
                }
                Some(ClientMsg::Submit { prompt, cwd, harness, model }) => {
                    let model = model.filter(|m| !m.trim().is_empty());
                    // A chosen model pins the default harness: the planner could pick
                    // one that doesn't know that model.
                    let unplanned = app.planner.is_none() || model.is_some();
                    let harness = harness.or_else(|| unplanned.then(|| "claude".into()));
                    let task = app.transition(|l| l.submit(prompt, cwd, harness, model));
                    send_json(&mut sock, &ServerMsg::Submitted { task }).await;
                }
                Some(ClientMsg::Subscribe) => sub = Some(app.events.subscribe()),
                Some(ClientMsg::Show { task }) => {
                    let reply = match app.ledger().show(task) {
                        Some((task, events, truncated)) => ServerMsg::TaskDetail {
                            task,
                            events: events.to_vec(),
                            truncated,
                        },
                        None => ServerMsg::Error { message: format!("unknown task {task}") },
                    };
                    send_json(&mut sock, &reply).await;
                }
                None => break,
            },
        }
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
        let ok =
            |app: &App, pairs: &[(&'static str, &str)]| app.authorized(&headers(pairs), lo, None);
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
        let ok = |pairs: &[(&'static str, &str)], peer, access| {
            t.authorized(&headers(pairs), peer, Some(access))
        };
        // Valid JWT: no token needed, from anywhere.
        assert!(ok(&[], remote, true));
        assert!(ok(&[(JWT_HEADER, "j")], lo, true));
        // No valid JWT: only a direct local connection, and it still needs the token.
        assert!(ok(&[bearer], lo, false));
        assert!(ok(&[bearer], mapped_lo, false));
        assert!(!ok(&[], lo, false));
        assert!(!ok(&[bearer], remote, false));
        // Through cloudflared (loopback peer, Cloudflare headers): never falls back.
        assert!(!ok(&[bearer, (JWT_HEADER, "bad")], lo, false));
        assert!(!ok(&[bearer, ("cf-ray", "x")], lo, false));
        assert!(!ok(&[bearer, ("cf-connecting-ip", "192.0.2.1")], lo, false));
        // A valid JWT does not bypass the origin check; its own page passes.
        let foreign = [("origin", "https://evil.example"), ("host", "m.example")];
        assert!(!ok(&foreign, remote, true));
        let open = App::new(None);
        let page = [("origin", "https://m.example"), ("host", "m.example")];
        assert!(open.authorized(&headers(&page), remote, Some(true)));
        assert!(!open.authorized(&headers(&page), lo, Some(false)));
    }

    #[test]
    fn normalizes_entries() {
        assert_eq!(
            normalize_origins(list(&[" HTTPS://X.com/ ", "", "  ", "http://a:1"])),
            list(&["https://x.com", "http://a:1"])
        );
    }
}

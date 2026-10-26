//! Headless orchestrator: tracks workers, queues tasks, assigns them to idle
//! workers with a matching harness, and fans agent events out to clients.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use mis_types::*;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

pub mod ledger;
pub mod plan;

/// A stuck planner (auth prompt, network stall) fails the task instead of
/// leaving it in Planning forever.
const PLANNER_TIMEOUT: Duration = Duration::from_secs(90);

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
    /// A worker silent (no Hello/Stats) this long is treated as disconnected.
    stale_after: Duration,
    /// How often to check for silent workers.
    tick_every: Duration,
    /// Origin of the ledger's monotonic `now`.
    started: Instant,
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
            // Two missed beats plus slack for network jitter.
            stale_after: 2 * STATS_INTERVAL + Duration::from_secs(5),
            tick_every: Duration::from_secs(5),
            started: Instant::now(),
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

    pub fn with_planner(mut self, planner: Arc<dyn Agent>) -> Self {
        self.planner = Some(planner);
        self
    }

    /// Run a ledger transition under the lock, then perform its effects outside it.
    fn transition<R>(&self, f: impl FnOnce(&mut Ledger) -> (R, Vec<Effect>)) -> R {
        let (r, effects) = f(&mut self.ledger.lock().unwrap());
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
                        let effects = app.ledger.lock().unwrap().expire(wid, generation);
                        app.apply(effects);
                    });
                }
                Effect::Plan { task, prompt } => {
                    let app = self.clone();
                    tokio::spawn(async move { app.run_planner(task, prompt).await });
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
        let _slot = self.planner_slots.acquire().await.expect("semaphore open");
        let (tx, mut rx) = mpsc::channel(64);
        let run = tokio::time::timeout(
            PLANNER_TIMEOUT,
            mis_agent::run_guarded(planner.as_ref(), task, &prompt, None, None, tx),
        );
        let feed = async {
            while let Some(ev) = rx.recv().await {
                let effects = self.ledger.lock().unwrap().planning_event(task, ev);
                self.apply(effects);
            }
        };
        let (timed_out, _) = tokio::join!(run, feed);
        if timed_out.is_err() {
            // Dropping `run` killed the process (kill_on_drop) without a Finished.
            let effects = self.ledger.lock().unwrap().planning_event(
                task,
                AgentEvent::Finished {
                    ok: false,
                    summary: Some(format!("planner timed out after {PLANNER_TIMEOUT:?}")),
                },
            );
            self.apply(effects);
        }
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

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let origin = match headers.get("origin").map(|v| v.to_str()) {
            None => None,
            Some(Ok(o)) => Some(o),
            Some(Err(_)) => return false,
        };
        let host = headers.get("host").and_then(|v| v.to_str().ok());
        let own_page = match (origin, host) {
            // Without a token, only loopback: a DNS-rebound name (evil.com -> 127.0.0.1)
            // would otherwise pass as same-origin.
            (Some(o), Some(h)) => same_origin(o, h) && (self.token.is_some() || loopback_host(h)),
            _ => false,
        };
        if !own_page && !origin_allowed(origin, &self.allowed_origins) {
            return false;
        }
        match &self.token {
            None => true,
            Some(t) => bearer_from(headers) == Some(t.as_str()),
        }
    }
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub async fn serve(listener: TcpListener, app: App) -> anyhow::Result<()> {
    app.spawn_liveness();
    axum::serve(listener, router(app)).await?;
    Ok(())
}

/// The phone page. Not framable: it acts with the token stored for this origin.
async fn page() -> impl IntoResponse {
    (
        [(header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'")],
        Html(include_str!("../static/index.html")),
    )
}

/// Lets the page decide whether to ask for a token.
async fn info(State(app): State<App>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "auth": app.token.is_some() }))
}

async fn ws_worker(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !app.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |sock| worker_conn(sock, app))
        .into_response()
}

async fn ws_client(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !app.authorized(&headers) {
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
                        app.ledger.lock().unwrap().worker_stats(wid, conn, stats, now, unix_secs());
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
                    let status = app.ledger.lock().unwrap().status();
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
                    let reply = match app.ledger.lock().unwrap().show(task) {
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
        let ok = |app: &App, pairs: &[(&'static str, &str)]| app.authorized(&headers(pairs));
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
    fn normalizes_entries() {
        assert_eq!(
            normalize_origins(list(&[" HTTPS://X.com/ ", "", "  ", "http://a:1"])),
            list(&["https://x.com", "http://a:1"])
        );
    }
}

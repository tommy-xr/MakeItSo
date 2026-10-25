//! Headless orchestrator: tracks workers, queues tasks, assigns them to idle
//! workers with a matching harness, and fans agent events out to clients.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
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
        }
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
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
            mis_agent::run_guarded(planner.as_ref(), task, &prompt, None, tx),
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
        if !origin_allowed(origin, &self.allowed_origins) {
            return false;
        }
        match &self.token {
            None => true,
            Some(t) => headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == format!("Bearer {t}"))
                .unwrap_or(false),
        }
    }
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
        .route("/ws/worker", get(ws_worker))
        .route("/ws/client", get(ws_client))
        .with_state(app)
}

pub async fn serve(listener: TcpListener, app: App) -> anyhow::Result<()> {
    axum::serve(listener, router(app)).await?;
    Ok(())
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
    ws.on_upgrade(move |sock| client_conn(sock, app))
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
    let unknown = app.transition(|l| l.connect(wid, info, &running, conn));
    tracing::info!(%wid, conn, ?running, "worker joined");
    if !unknown.is_empty() {
        tracing::warn!(%wid, ?unknown, "worker reports tasks unknown to this orchestrator");
    }
    if send_json(&mut sock, &OrchMsg::Welcome { id: wid }).await {
        loop {
            tokio::select! {
                Some(out) = rx.recv() => {
                    if !send_json(&mut sock, &out).await { break; }
                }
                incoming = recv_json::<WorkerMsg>(&mut sock) => match incoming {
                    Some(WorkerMsg::Event { task, event }) => {
                        app.transition(|l| ((), l.event(wid, conn, task, event)));
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
        tokio::select! {
            incoming = recv_json::<ClientMsg>(&mut sock) => match incoming {
                Some(ClientMsg::Status) => {
                    let status = app.ledger.lock().unwrap().status();
                    send_json(&mut sock, &status).await;
                }
                Some(ClientMsg::Submit { prompt, cwd, harness }) => {
                    let harness = harness.or_else(|| app.planner.is_none().then(|| "claude".into()));
                    let task = app.transition(|l| l.submit(prompt, cwd, harness));
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
            ev = next_event(sub.as_mut()) => {
                if !send_json(&mut sock, &ev).await { break; }
            }
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

    #[test]
    fn normalizes_entries() {
        assert_eq!(
            normalize_origins(list(&[" HTTPS://X.com/ ", "", "  ", "http://a:1"])),
            list(&["https://x.com", "http://a:1"])
        );
    }
}

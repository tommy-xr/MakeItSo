//! Fake worker + client drive the orchestrator end to end over real WebSockets.

use futures_util::{SinkExt, StreamExt};
use mis_orchestrator::App;
use mis_types::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn send<T: serde::Serialize>(ws: &mut Ws, v: &T) {
    ws.send(Message::Text(serde_json::to_string(v).unwrap().into()))
        .await
        .unwrap();
}
async fn recv<T: serde::de::DeserializeOwned>(ws: &mut Ws) -> T {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

fn info(harness: &str) -> WorkerInfo {
    WorkerInfo {
        hostname: "w".into(),
        os: "o".into(),
        arch: "a".into(),
        harnesses: vec![harness.into()],
        roots: vec![],
        models: [(harness.into(), vec![format!("{harness}-mini")])].into(),
        ..Default::default()
    }
}

/// Connect a worker and complete the handshake.
async fn join(base: &str, id: WorkerId, harness: &str, running: Vec<TaskId>) -> Ws {
    let (mut ws, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    send(
        &mut ws,
        &WorkerMsg::Hello {
            id,
            info: info(harness),
            running,
        },
    )
    .await;
    let OrchMsg::Welcome { id: got, .. } = recv(&mut ws).await else {
        panic!()
    };
    assert_eq!(got, id);
    ws
}

async fn start() -> String {
    start_with_grace(std::time::Duration::from_millis(50)).await
}

async fn start_with_grace(grace: std::time::Duration) -> String {
    start_app(App::new(None).with_grace(grace)).await
}

async fn start_app(app: App) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mis_orchestrator::serve(listener, app));
    format!("ws://{addr}")
}

/// `/ws/client` upgrade request with extra headers.
fn client_req(
    base: &str,
    pairs: &[(&'static str, &str)],
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("{base}/ws/client"),
    )
    .unwrap();
    for (k, v) in pairs {
        req.headers_mut().insert(*k, v.parse().unwrap());
    }
    req
}

/// `/ws/client` upgrade request carrying a browser `Origin` header.
fn client_req_with_origin(
    base: &str,
    origin: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    client_req(base, &[("origin", origin)])
}

fn allow_mis_example(app: App) -> App {
    app.with_allowed_origins(vec!["https://mis.example.com".into()])
}

#[tokio::test]
async fn submit_assign_finish() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;

    // Task submitted with no workers stays queued.
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert!(workers.is_empty());
    assert_eq!(tasks[0].status, TaskStatus::Queued);

    // Worker joins and immediately receives the queued task.
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    let OrchMsg::Assign { task: assigned, .. } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!(assigned.id, task);
    assert_eq!(assigned.harness, "claude");

    // Events stream through to the subscribed client; Finished marks it done.
    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Text {
                text: "working".into(),
            },
        },
    )
    .await;
    let ServerMsg::Event {
        worker: w,
        event: AgentEvent::Text { text },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!((w, text.as_str()), (Some(wid), "working"));
    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        },
    )
    .await;
    let ServerMsg::Event {
        event: AgentEvent::Finished { ok: true, .. },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };

    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Done);
    assert_eq!(tasks[0].worker, Some(wid));
    assert!(!workers[0].busy);
}

#[tokio::test]
async fn model_reaches_worker_and_status() {
    // FakePlanner would route to codex: an explicit model must skip it.
    let base =
        start_app(App::new(None).with_planner(std::sync::Arc::new(FakePlanner(ROUTE_CODEX)))).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;

    // Requested model: default harness, sent to the worker, kept over the reported one.
    // Then a task with none: the reported model fills in.
    let mut ids = vec![];
    for (harness, model, reported) in [
        (None, Some("opus"), "claude-opus-4-1"),
        (Some("claude"), None, "claude-sonnet-4-5"),
    ] {
        send(
            &mut client,
            &ClientMsg::Submit {
                prompt: "hi".into(),
                cwd: None,
                harness: harness.map(String::from),
                model: model.map(String::from),
                reply_to: None,
                images: vec![],
            },
        )
        .await;
        let ServerMsg::Submitted { task } = recv(&mut client).await else {
            panic!()
        };
        let OrchMsg::Assign { task: assigned, .. } = recv(&mut worker).await else {
            panic!()
        };
        assert_eq!(
            (
                assigned.id,
                assigned.harness.as_str(),
                assigned.model.as_deref()
            ),
            (task, "claude", model)
        );
        for event in [
            AgentEvent::Started {
                session: None,
                model: Some(reported.into()),
            },
            AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        ] {
            send(&mut worker, &WorkerMsg::Event { task, event }).await;
            let ServerMsg::Event { .. } = recv(&mut client).await else {
                panic!()
            };
        }
        ids.push(task);
    }

    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    let models: Vec<_> = ids
        .iter()
        .map(|id| tasks.iter().find(|t| t.id == *id).unwrap().model.as_deref())
        .collect();
    assert_eq!(models, [Some("opus"), Some("claude-sonnet-4-5")]);
}

#[tokio::test]
async fn harness_mismatch_stays_queued() {
    let base = start().await;
    let _worker = join(&base, uuid::Uuid::new_v4(), "codex", vec![]).await;

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { .. } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Queued);
}

#[tokio::test]
async fn worker_disconnect_fails_task_and_notifies() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;

    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };

    drop(worker);
    let ServerMsg::Event {
        task: t,
        event: AgentEvent::Finished { ok: false, .. },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(t, task);
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Failed);
    assert!(workers.is_empty());
}

#[tokio::test]
async fn events_from_wrong_worker_are_ignored() {
    let base = start().await;
    let w1 = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let w2 = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };

    // Whichever worker got the task, the other one tries to finish it.
    let (mut assignee, mut other) = (w1, w2);
    let assign = tokio::time::timeout(std::time::Duration::from_millis(500), assignee.next()).await;
    if assign.is_err() {
        std::mem::swap(&mut assignee, &mut other);
        let OrchMsg::Assign { .. } = recv(&mut assignee).await else {
            panic!()
        };
    }
    send(
        &mut other,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        },
    )
    .await;

    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Running);
}

#[tokio::test]
async fn browser_origin_rejected() {
    let base = start().await;
    let req = client_req_with_origin(&base, "http://evil.example");
    assert!(connect_async(req).await.is_err());
}

#[tokio::test]
async fn allowed_origin_connects() {
    let base = start_app(allow_mis_example(App::new(None))).await;
    let mut client = connect_async(client_req_with_origin(&base, "https://mis.example.com"))
        .await
        .unwrap()
        .0;
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut client).await else {
        panic!()
    };
}

#[tokio::test]
async fn unlisted_origin_rejected_with_allowlist() {
    let base = start_app(allow_mis_example(App::new(None))).await;
    let req = client_req_with_origin(&base, "https://evil.example");
    assert!(connect_async(req).await.is_err());
}

#[tokio::test]
async fn allowed_origin_still_needs_token() {
    let base = start_app(allow_mis_example(App::new(Some("s".into())))).await;
    let req = client_req_with_origin(&base, "https://mis.example.com");
    assert!(connect_async(req).await.is_err());
}

/// Browser-shaped upgrade: token as subprotocol, `Origin` naming this server.
fn page_req(base: &str, token: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let origin = base.replacen("ws://", "http://", 1);
    let mut req = client_req_with_origin(base, &origin);
    let protos = format!("bearer, {token}").parse().unwrap();
    req.headers_mut().insert("sec-websocket-protocol", protos);
    req
}

#[tokio::test]
async fn same_origin_page_connects_with_subprotocol_token() {
    let base = start_app(App::new(Some("s".into()))).await;
    let (mut client, resp) = connect_async(page_req(&base, "s")).await.unwrap();
    assert_eq!(resp.headers()["sec-websocket-protocol"], "bearer");
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut client).await else {
        panic!()
    };

    let err = connect_async(page_req(&base, "wrong")).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(resp) = err else {
        panic!("{err}")
    };
    assert_eq!(resp.status(), 401);
}

fn with_read_token() -> App {
    App::new(Some("s".into())).with_read_tokens(vec!["r".into()])
}

#[tokio::test]
async fn read_token_sees_but_cannot_submit() {
    let base = start_app(with_read_token()).await;
    let (mut client, _) = connect_async(client_req(&base, &[("authorization", "Bearer r")]))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut client).await else {
        panic!()
    };
    let submit = ClientMsg::Submit {
        prompt: "rm -rf".into(),
        cwd: None,
        harness: None,
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(&mut client, &submit).await;
    let reply: ServerMsg = recv(&mut client).await;
    assert_eq!(
        reply,
        ServerMsg::Error {
            message: "read-only token".into()
        }
    );
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert!(tasks.is_empty());
    // The page learns its scope from /api/info.
    let info = |auth: &'static str| http_get_with(&base, "/api/info", auth);
    assert!(info("Authorization: Bearer r\r\n")
        .await
        .contains(r#""scope":"read""#));
    assert!(info("Authorization: Bearer s\r\n")
        .await
        .contains(r#""scope":"full""#));
}

#[tokio::test]
async fn worker_needs_the_full_token() {
    let base = start_app(with_read_token()).await;
    let worker = |token: &str| {
        let mut req = format!("{base}/ws/worker").into_client_request().unwrap();
        let auth = format!("Bearer {token}").parse().unwrap();
        req.headers_mut().insert("authorization", auth);
        req
    };
    let err = connect_async(worker("r")).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(resp) = err else {
        panic!("{err}")
    };
    assert_eq!(resp.status(), 401);
    assert!(connect_async(worker("s")).await.is_ok());
}

/// Minimal HTTP/1.1 GET; returns the raw response.
async fn http_get(base: &str, path: &str) -> String {
    http_get_with(base, path, "").await
}

/// [`http_get`] with extra raw header lines (`"name: value\r\n"`).
async fn http_get_with(base: &str, path: &str, extra: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("ws://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}Connection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn serves_page_and_info_without_token() {
    let base = start_app(App::new(Some("s".into()))).await;
    let page = http_get(&base, "/").await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("text/html"));
    assert!(page.contains("frame-ancestors 'none'"));
    assert!(page.contains("cache-control: no-cache"), "{page}");
    assert!(page.contains("<title>Make It So</title>"));
    // Voice: a mic on each composer (new task, reply, session), readback controls.
    assert_eq!(
        page.matches(r#"class="mic""#).count(),
        3,
        "mic on each composer"
    );
    assert!(page.contains(r#"id="autoread""#));
    assert!(page.contains(r#"id="speak""#));
    let info = http_get(&base, "/api/info").await;
    assert!(
        info.contains(r#"{"access":false,"auth":true,"scope":null,"vapid":null,"version":"#),
        "{info}"
    );
    // Home-screen install assets.
    let manifest = http_get(&base, "/manifest.webmanifest").await;
    assert!(manifest.contains("application/manifest+json"), "{manifest}");
    assert!(
        manifest.contains(r#""display": "standalone""#),
        "{manifest}"
    );
    assert!(http_get(&base, "/icon.svg").await.contains("image/svg+xml"));
    // Service worker for push: may control `/`, revalidated like the page.
    let sw = http_get(&base, "/sw.js").await;
    for h in [
        "text/javascript",
        "service-worker-allowed: /",
        "cache-control: no-cache",
        "showNotification",
    ] {
        assert!(sw.contains(h), "{h}: {sw}");
    }
}

const ACCESS_AUD: &str = "aud-tag";

/// Orchestrator behind (simulated) Cloudflare Access, with test keys injected.
async fn start_behind_access() -> String {
    start_app(behind_access()).await
}

/// App checking Access JWTs signed by the fixture key.
fn behind_access() -> App {
    let jwks = serde_json::from_str(include_str!("fixtures/access-test-jwks.json")).unwrap();
    let gate = mis_orchestrator::access_gate::AccessGate::with_keys(
        "team".into(),
        ACCESS_AUD.into(),
        jwks,
    );
    App::new(Some("s".into())).with_access(gate)
}

/// RS256 Access JWT signed by the fixture key, valid for an hour.
fn access_jwt(aud: &str) -> String {
    access_jwt_for(aud, "me@example.com")
}

/// [`access_jwt`] for another email.
fn access_jwt_for(aud: &str, email: &str) -> String {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut h = Header::new(Algorithm::RS256);
    h.kid = Some("test-kid".into());
    let claims = serde_json::json!({
        "aud": [aud], "iss": "https://team.cloudflareaccess.com",
        "exp": now + 3600, "iat": now, "email": email,
    });
    let key = include_bytes!("fixtures/access-test-key.pem");
    encode(&h, &claims, &EncodingKey::from_rsa_pem(key).unwrap()).unwrap()
}

/// `/ws/client` upgrade with extra headers; returns the HTTP status on failure.
async fn client_status(base: &str, pairs: &[(&'static str, &str)]) -> u16 {
    match connect_async(client_req(base, pairs)).await {
        Ok((_, resp)) => resp.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
async fn access_jwt_admits_and_bad_or_missing_is_401() {
    let base = start_behind_access().await;
    let jwt = access_jwt(ACCESS_AUD);
    let via_tunnel = ("cf-ray", "8f00-SJC");
    // Valid JWT: no token needed.
    let ok = [("cf-access-jwt-assertion", jwt.as_str()), via_tunnel];
    assert_eq!(client_status(&base, &ok).await, 101);
    // Bad JWT, even with the right token.
    let wrong_aud = access_jwt("other");
    let bad = [
        ("cf-access-jwt-assertion", wrong_aud.as_str()),
        ("authorization", "Bearer s"),
    ];
    assert_eq!(client_status(&base, &bad).await, 401);
    // Through the tunnel without a JWT (Access not covering the path): no fallback.
    let bare = [via_tunnel, ("authorization", "Bearer s")];
    assert_eq!(client_status(&base, &bare).await, 401);
    // Direct local connection: the bearer token still works, and is still needed.
    assert_eq!(
        client_status(&base, &[("authorization", "Bearer s")]).await,
        101
    );
    assert_eq!(client_status(&base, &[]).await, 401);
}

#[tokio::test]
async fn access_subjects_limit_who_gets_in() {
    let app = behind_access().with_access_subjects(vec!["Me@Example.com".into()]);
    let base = start_app(app).await;
    let via_tunnel = ("cf-ray", "8f00-SJC");
    let me = access_jwt(ACCESS_AUD);
    let ok = [("cf-access-jwt-assertion", me.as_str()), via_tunnel];
    assert_eq!(client_status(&base, &ok).await, 101);
    // Another identity the Access app admits: refused, even with the token.
    let other = access_jwt_for(ACCESS_AUD, "guest@example.com");
    let refused = [
        ("cf-access-jwt-assertion", other.as_str()),
        via_tunnel,
        ("authorization", "Bearer s"),
    ];
    assert_eq!(client_status(&base, &refused).await, 401);
}

#[tokio::test]
async fn mcp_goes_through_access() {
    let base = start_behind_access().await;
    let list = || rpc("tools/list", serde_json::json!({}));
    let jwt = access_jwt(ACCESS_AUD);
    let via_tunnel = ("cf-ray", "8f00-SJC");
    let ok = [("cf-access-jwt-assertion", jwt.as_str()), via_tunnel];
    assert!(mcp_post(&base, &ok, list()).await.0.contains("200"));
    // Through the tunnel without a JWT, even with the token: refused.
    let bare = [via_tunnel, ("authorization", "Bearer s")];
    assert!(mcp_post(&base, &bare, list()).await.0.contains("401"));
}

#[tokio::test]
async fn info_reports_access_so_page_skips_token() {
    let base = start_behind_access().await;
    let jwt = format!("cf-access-jwt-assertion: {}\r\n", access_jwt(ACCESS_AUD));
    let info = http_get_with(&base, "/api/info", &jwt).await;
    assert!(
        info.contains(r#"{"access":true,"auth":false,"scope":"full","vapid":null,"version":"#),
        "{info}"
    );
    let info = http_get(&base, "/api/info").await;
    assert!(
        info.contains(r#"{"access":false,"auth":true,"scope":null,"vapid":null,"version":"#),
        "{info}"
    );
}

#[tokio::test]
async fn client_socket_is_pinged() {
    let app = App::new(None).with_ping_every(std::time::Duration::from_millis(50));
    let base = start_app(app).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();
    assert!(matches!(msg, Message::Ping(_)), "{msg:?}");
}

#[tokio::test]
async fn reconnect_keeps_reported_task_and_fails_unreported() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "a".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };

    // Reconnect, still running the task: ledger keeps one busy entry, task stays Running.
    let mut worker2 = join(&base, wid, "claude", vec![task]).await;
    drop(worker); // stale socket closing must not remove the new registration
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(workers.len(), 1);
    assert!(workers[0].busy);
    assert_eq!(tasks[0].status, TaskStatus::Running);

    // Events over the new connection still count.
    send(
        &mut worker2,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Text {
                text: "still here".into(),
            },
        },
    )
    .await;
    let ServerMsg::Event {
        event: AgentEvent::Text { text },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(text, "still here");

    // Reconnect after a restart (nothing running): task is failed and followers told.
    let _worker3 = join(&base, wid, "claude", vec![]).await;
    let ServerMsg::Event {
        task: t,
        event: AgentEvent::Finished { ok: false, summary },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(t, task);
    assert_eq!(summary.as_deref(), Some("worker restarted"));
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(workers.len(), 1);
    assert!(!workers[0].busy);
    assert_eq!(tasks[0].status, TaskStatus::Failed);
}

#[tokio::test]
async fn unknown_running_task_does_not_pin_worker_busy() {
    let base = start().await;
    let _worker = join(
        &base,
        uuid::Uuid::new_v4(),
        "claude",
        vec![uuid::Uuid::new_v4()],
    )
    .await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert!(!workers[0].busy);
}

#[tokio::test]
async fn disconnect_then_reconnect_within_grace_keeps_task() {
    let base = start_with_grace(std::time::Duration::from_secs(5)).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "a".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };

    worker.close(None).await.unwrap();
    drop(worker);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut worker = join(&base, wid, "claude", vec![task]).await;

    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        },
    )
    .await;
    let ServerMsg::Event {
        event: AgentEvent::Finished { ok: true, .. },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };

    // Worker is free again: the next task is assigned to it.
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "b".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task: next } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { task: assigned, .. } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!(assigned.id, next);
}

#[tokio::test]
async fn show_returns_transcript() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };

    let events = vec![
        AgentEvent::Started {
            session: Some("sess".into()),
            model: None,
        },
        AgentEvent::Text { text: "hi".into() },
        AgentEvent::ToolUse {
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        },
        AgentEvent::ToolResult {
            content: "a.txt".into(),
        },
        AgentEvent::Finished {
            ok: true,
            summary: Some("listed".into()),
        },
    ];
    for event in &events {
        send(
            &mut worker,
            &WorkerMsg::Event {
                task,
                event: event.clone(),
            },
        )
        .await;
        let ServerMsg::Event { .. } = recv(&mut client).await else {
            panic!()
        };
    }

    send(&mut client, &ClientMsg::Show { task }).await;
    let ServerMsg::TaskDetail {
        task: info,
        events: got,
        truncated,
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(got, events);
    assert!(!truncated);
    assert_eq!(info.status, TaskStatus::Done);
    assert_eq!(info.session.as_deref(), Some("sess"));
    assert_eq!(info.summary.as_deref(), Some("listed"));

    send(
        &mut client,
        &ClientMsg::Show {
            task: uuid::Uuid::nil(),
        },
    )
    .await;
    let ServerMsg::Error { .. } = recv(&mut client).await else {
        panic!()
    };
}

#[tokio::test]
async fn status_shows_activity_tokens_and_timing() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };
    for event in [
        AgentEvent::ToolUse {
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        },
        AgentEvent::Usage {
            input: 12_300,
            output: 1_100,
        },
    ] {
        send(&mut worker, &WorkerMsg::Event { task, event }).await;
        // Usage is broadcast too, so followers can update live.
        let ServerMsg::Event { .. } = recv(&mut client).await else {
            panic!()
        };
    }
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, now_ms, .. } = recv(&mut client).await else {
        panic!()
    };
    let t = &tasks[0];
    assert_eq!(t.activity, "> Bash ls");
    assert!(now_ms.unwrap() >= t.started_at.unwrap());
    assert_eq!((t.tokens_in, t.tokens_out), (Some(12_300), Some(1_100)));
    assert!(t.started_at.unwrap() > 1_700_000_000_000);
    assert_eq!(t.finished_at, None);
}

/// Picks codex and its advertised model, and rewrites the prompt.
const ROUTE_CODEX: &str =
    r#"{"harness":"codex","model":"codex-mini","prompt":"say hi","reason":"user asked for codex"}"#;

/// Planner stub replying with its JSON plan, after reporting model `fake-sonnet`.
struct FakePlanner(&'static str);
impl mis_agent::Agent for FakePlanner {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn models(&self) -> Vec<String> {
        vec![]
    }
    fn run<'a>(
        &'a self,
        _task: TaskId,
        prompt: &'a str,
        _opts: mis_agent::RunOpts<'a>,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> mis_agent::BoxFut<'a> {
        Box::pin(async move {
            assert!(
                prompt.contains("claude, codex"),
                "fleet harnesses in prompt"
            );
            assert!(
                prompt.contains("- codex: codex-mini"),
                "fleet models in prompt"
            );
            tx.send(AgentEvent::Started {
                session: None,
                model: Some("fake-sonnet".into()),
            })
            .await?;
            tx.send(AgentEvent::Finished {
                ok: true,
                summary: Some(self.0.into()),
            })
            .await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn planner_routes_to_codex_and_streams_its_output() {
    let base =
        start_app(App::new(None).with_planner(std::sync::Arc::new(FakePlanner(ROUTE_CODEX)))).await;
    let mut claude = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let codex_id = uuid::Uuid::new_v4();
    let mut codex = join(&base, codex_id, "codex", vec![]).await;

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hey run as codex and say hi".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    // The planner's reason and decision reach followers as events with no worker.
    let mut texts = vec![];
    loop {
        match recv::<ServerMsg>(&mut client).await {
            ServerMsg::Event {
                worker: None,
                event: AgentEvent::Text { text },
                ..
            } => texts.push(text),
            ServerMsg::Event {
                worker: None,
                event:
                    AgentEvent::Planned {
                        harness,
                        prompt,
                        model,
                        worker: None,
                    },
                task: t,
            } => {
                assert_eq!(
                    (t, harness.as_str(), prompt.as_str(), model.as_deref()),
                    (task, "codex", "say hi", Some("codex-mini"))
                );
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(texts, ["user asked for codex"]);
    // Only the codex worker gets it, with the rewritten prompt and chosen model.
    let OrchMsg::Assign { task: assigned, .. } = recv(&mut codex).await else {
        panic!()
    };
    assert_eq!(
        (
            assigned.id,
            assigned.harness.as_str(),
            assigned.prompt.as_str(),
            assigned.model.as_deref()
        ),
        (task, "codex", "say hi", Some("codex-mini"))
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), claude.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn planner_answers_trivial_question_without_a_worker() {
    let planner = FakePlanner(r#"{"answer":"4","reason":"arithmetic"}"#);
    let base = start_app(App::new(None).with_planner(std::sync::Arc::new(planner))).await;
    let mut claude = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let mut codex = join(&base, uuid::Uuid::new_v4(), "codex", vec![]).await;

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "what is 2+2?".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let ServerMsg::Event {
        worker: None,
        event: AgentEvent::Text { text },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(text, "4");
    let ServerMsg::Event {
        worker: None,
        event: AgentEvent::Finished { ok: true, summary },
        ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(summary.as_deref(), Some("4"));

    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers, .. } = recv(&mut client).await else {
        panic!()
    };
    let t = tasks.iter().find(|t| t.id == task).unwrap();
    assert_eq!(
        (
            t.status,
            t.summary.as_deref(),
            t.harness.as_str(),
            t.model.as_deref(),
            t.worker
        ),
        (
            TaskStatus::Done,
            Some("4"),
            "planner",
            Some("fake-sonnet"),
            None
        )
    );
    assert!(workers.iter().all(|w| !w.busy));
    for w in [&mut claude, &mut codex] {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), w.next())
                .await
                .is_err(),
            "worker got a task"
        );
    }
}

/// The sole worker's summary from a fresh Status.
async fn only_worker(client: &mut Ws) -> WorkerSummary {
    send(client, &ClientMsg::Status).await;
    let ServerMsg::Status { mut workers, .. } = recv(client).await else {
        panic!()
    };
    assert_eq!(workers.len(), 1);
    workers.pop().unwrap()
}

#[tokio::test]
async fn stats_reported_then_silent_worker_goes_offline() {
    use std::time::{Duration, Instant};
    let app = App::new(None)
        .with_grace(Duration::from_secs(5))
        .with_liveness(Duration::from_millis(300), Duration::from_millis(50));
    let base = start_app(app).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    let stats = WorkerStats {
        disk_free_gb: 120.5,
        disk_used_pct: 38,
        cpu_pct: 64,
        mem_pct: 82,
        ..Default::default()
    };
    send(&mut worker, &WorkerMsg::Stats(stats.clone())).await;

    // Stats and Status ride different sockets, so poll until it lands.
    let start = Instant::now();
    let w = loop {
        let w = only_worker(&mut client).await;
        if w.stats.is_some() {
            break w;
        }
        assert!(start.elapsed() < Duration::from_secs(1), "no stats");
    };
    assert!(w.online);
    assert_eq!(w.stats, Some(stats));
    assert!(w.stats_at.unwrap() > 1_700_000_000);

    // Silent from here: flagged offline (threshold timing is unit-tested) and its socket closed.
    while only_worker(&mut client).await.online {
        assert!(start.elapsed() < Duration::from_secs(2), "still online");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let closed = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(Ok(_)) = worker.next().await {}
    })
    .await;
    assert!(closed.is_ok(), "socket not closed");

    // Reconnecting brings it back.
    let _worker = join(&base, wid, "claude", vec![]).await;
    assert!(only_worker(&mut client).await.online);
}

#[tokio::test]
async fn limits_ride_worker_stats_into_status() {
    use std::time::{Duration, Instant};
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    // As a worker sends it: claude's windows from a run, codex's from its rollout.
    let raw = r#"{"type":"stats","cpu_pct":5,"limits":{
        "claude":[{"name":"5h","used_pct":43.0,"resets_at":1793223000000,"observed_at":1793200000000},
                  {"name":"7d","used_pct":62.0,"resets_at":1793444400000,"observed_at":1793200000000}],
        "codex":[{"name":"5h","used_pct":100.0,"observed_at":1793200000000}]}}"#;
    worker
        .send(tokio_tungstenite::tungstenite::Message::Text(raw.into()))
        .await
        .unwrap();
    let start = Instant::now();
    let limits = loop {
        if let Some(s) = only_worker(&mut client).await.stats {
            break s.limits;
        }
        assert!(start.elapsed() < Duration::from_secs(1), "no stats");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let names: Vec<_> = limits
        .iter()
        .flat_map(|(h, ws)| {
            ws.iter()
                .map(move |w| format!("{h} {} {}", w.name, w.used_pct))
        })
        .collect();
    assert_eq!(names, ["claude 5h 43", "claude 7d 62", "codex 5h 100"]);
    assert_eq!(limits["claude"][0].resets_at, Some(1_793_223_000_000));
    assert_eq!(limits["codex"][0].resets_at, None);
}

#[tokio::test]
async fn out_of_usage_worker_waits_until_its_limit_lifts() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let claude_5h = |used_pct| WorkerStats {
        limits: [(
            "claude".to_string(),
            vec![LimitWindow {
                name: "5h".into(),
                used_pct,
                resets_at: Some(now + 3_600_000),
                observed_at: now,
                overage: false,
            }],
        )]
        .into(),
        ..Default::default()
    };
    send(&mut worker, &WorkerMsg::Stats(claude_5h(100.0))).await;
    // Stats and Submit ride different sockets: wait until the reading has landed.
    while only_worker(&mut client).await.stats.is_none() {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].status, TaskStatus::Queued);
    assert_eq!(tasks[0].blocked_until, Some(now + 3_600_000));
    assert!(tasks[0].activity.starts_with("queued · claude resets "));

    // A later beat reads under the limit: the task goes out.
    send(&mut worker, &WorkerMsg::Stats(claude_5h(20.0))).await;
    let OrchMsg::Assign { task: assigned, .. } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!(assigned.id, task);
}

/// Summarizer that counts its calls and replies with a fixed summary.
struct FakeSummarizer(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl mis_agent::Agent for FakeSummarizer {
    fn name(&self) -> &'static str {
        "fake-summarizer"
    }
    fn models(&self) -> Vec<String> {
        vec![]
    }
    fn run<'a>(
        &'a self,
        _task: TaskId,
        prompt: &'a str,
        _opts: mis_agent::RunOpts<'a>,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> mis_agent::BoxFut<'a> {
        Box::pin(async move {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(prompt.contains("> Bash"), "transcript in prompt");
            tx.send(AgentEvent::Finished {
                ok: true,
                summary: Some("Summary: Listed 2 files.".into()),
            })
            .await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn finished_task_gets_summary_but_planner_answer_does_not() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = App::new(None)
        .with_planner(std::sync::Arc::new(FakePlanner(
            r#"{"answer":"4","reason":"arithmetic"}"#,
        )))
        .with_summarizer(std::sync::Arc::new(FakeSummarizer(calls.clone())));
    let base = start_app(app).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let _codex = join(&base, uuid::Uuid::new_v4(), "codex", vec![]).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;

    // Planner-answered: Done with the answer, never summarized.
    let submit = |harness: Option<&str>| ClientMsg::Submit {
        prompt: "q".into(),
        cwd: None,
        harness: harness.map(String::from),
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(&mut client, &submit(None)).await;
    let ServerMsg::Submitted { task: answered } = recv(&mut client).await else {
        panic!()
    };
    for _ in 0..2 {
        let ServerMsg::Event { .. } = recv(&mut client).await else {
            panic!()
        };
    }

    // Worker task with a tool call: the summary replaces the agent's own.
    send(&mut client, &submit(Some("claude"))).await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };
    for event in [
        AgentEvent::ToolUse {
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        },
        AgentEvent::Finished {
            ok: true,
            summary: Some("a.txt\nb.txt".into()),
        },
    ] {
        send(&mut worker, &WorkerMsg::Event { task, event }).await;
        let ServerMsg::Event { .. } = recv(&mut client).await else {
            panic!()
        };
    }
    // Followers get a fresh Status carrying the new summary.
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    let by_id = |id| tasks.iter().find(|t| t.id == id).unwrap();
    assert_eq!(by_id(task).summary.as_deref(), Some("Listed 2 files."));
    assert_eq!(by_id(task).status, TaskStatus::Done);
    assert_eq!(by_id(answered).summary.as_deref(), Some("4"));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    send(&mut client, &ClientMsg::Show { task }).await;
    let ServerMsg::TaskDetail { task: info, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(info.summary.as_deref(), Some("Listed 2 files."));
}

#[tokio::test]
async fn status_event_sets_activity_and_is_kept() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "hi".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };
    let status = AgentEvent::Status {
        text: "Counting repos".into(),
    };
    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: status.clone(),
        },
    )
    .await;
    let ServerMsg::Event { event, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(event, status);
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].activity, "status: Counting repos");
    send(&mut client, &ClientMsg::Show { task }).await;
    let ServerMsg::TaskDetail { events, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(events, [status]);
}

#[tokio::test]
async fn pr_link_in_agent_text_reaches_status_and_show() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "open a PR".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };
    let text = "Opened (https://github.com/o/r/pull/42).".into();
    send(
        &mut worker,
        &WorkerMsg::Event {
            task,
            event: AgentEvent::Text { text },
        },
    )
    .await;
    let ServerMsg::Event { .. } = recv(&mut client).await else {
        panic!()
    };
    let want = vec![Link {
        kind: LinkKind::Pr,
        url: "https://github.com/o/r/pull/42".into(),
        label: "o/r#42".into(),
        ok: None,
        detail: None,
        pr: None,
    }];
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].links, want);
    send(&mut client, &ClientMsg::Show { task }).await;
    let ServerMsg::TaskDetail { task: info, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(info.links, want);
}

#[tokio::test]
async fn reported_evidence_reaches_status_and_show() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "run the tests".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };
    let test = Link {
        kind: LinkKind::Test,
        url: String::new(),
        label: "cargo test".into(),
        ok: Some(true),
        detail: Some("78 passed".into()),
        pr: None,
    };
    let event = AgentEvent::Evidence(test.clone());
    send(&mut worker, &WorkerMsg::Event { task, event }).await;
    let ServerMsg::Event { .. } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].links, std::slice::from_ref(&test));
    send(&mut client, &ClientMsg::Show { task }).await;
    let ServerMsg::TaskDetail { task: info, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(info.links, [test]);
}

#[tokio::test]
async fn agent_plan_reaches_watchers_and_show() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let task = submit_claude(&mut client, &mut worker, "fix the test").await;
    let items = vec![
        PlanItem {
            text: "Reproduce the failure".into(),
            status: PlanStatus::Completed,
            active_form: Some("Reproducing the failure".into()),
        },
        PlanItem {
            text: "Fix the race".into(),
            status: PlanStatus::InProgress,
            active_form: Some("Fixing the race".into()),
        },
    ];
    let event = AgentEvent::Plan {
        items: items.clone(),
    };
    send(&mut worker, &WorkerMsg::Event { task, event }).await;
    // Pushed to the watching client unasked, as its task's latest plan.
    loop {
        match recv(&mut client).await {
            ServerMsg::Status { tasks, .. } if !tasks[0].plan.is_empty() => {
                assert_eq!(tasks[0].plan, items);
                break;
            }
            ServerMsg::Event { event, .. } => {
                assert!(!matches!(event, AgentEvent::Plan { .. }), "off the stream")
            }
            _ => {}
        }
    }
    send(&mut client, &ClientMsg::Show { task }).await;
    let ServerMsg::TaskDetail {
        task: info, events, ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(info.plan, items);
    assert!(events.is_empty(), "not on the transcript");
}

/// POST a JSON-RPC body to `/mcp` with extra headers; returns (status line, JSON body if any).
async fn mcp_post(
    base: &str,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) -> (String, Option<serde_json::Value>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("ws://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let auth: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let body = body.to_string();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let status = head.lines().next().unwrap().to_string();
    (status, serde_json::from_str(body).ok())
}

fn rpc(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
}

#[tokio::test]
async fn mcp_lists_tools_and_shows_a_finished_task() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "count files".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };
    for event in [
        AgentEvent::Text {
            text: "Three.".into(),
        },
        AgentEvent::Finished {
            ok: true,
            summary: Some("Three files.".into()),
        },
    ] {
        send(&mut worker, &WorkerMsg::Event { task, event }).await;
    }
    // The ledger has the task done once its Finished is broadcast.
    while !matches!(
        recv(&mut client).await,
        ServerMsg::Event {
            event: AgentEvent::Finished { .. },
            ..
        }
    ) {}

    let (status, list) = mcp_post(&base, &[], rpc("tools/list", serde_json::json!({}))).await;
    assert!(status.contains("200"), "{status}");
    let names: Vec<_> = list.unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["list_workers", "list_tasks", "show_task"]);

    let args =
        serde_json::json!({"name": "show_task", "arguments": {"id": &task.to_string()[..8]}});
    let (_, reply) = mcp_post(&base, &[], rpc("tools/call", args)).await;
    let text = reply.unwrap()["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let shown: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(shown["status"], "done");
    assert_eq!(shown["summary"], "Three files.");
    assert_eq!(shown["transcript"], "Three.\ndone");
}

#[tokio::test]
async fn mcp_needs_the_token() {
    let base = start_app(App::new(Some("s".into()))).await;
    let list = || rpc("tools/list", serde_json::json!({}));
    assert!(mcp_post(&base, &[], list()).await.0.contains("401"));
    assert!(mcp_post(&base, &[("authorization", "Bearer x")], list())
        .await
        .0
        .contains("401"));
    let (status, reply) = mcp_post(&base, &[("authorization", "Bearer s")], list()).await;
    assert!(status.contains("200"), "{status}");
    assert!(reply.unwrap()["result"]["tools"].is_array());
    // GET (SSE stream) is not offered.
    assert!(http_get(&base, "/mcp").await.starts_with("HTTP/1.1 405"));
}

/// Submit a reply to `parent`; returns the new task id.
async fn reply(client: &mut Ws, parent: TaskId, text: &str) -> TaskId {
    send(
        client,
        &ClientMsg::Submit {
            prompt: text.into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: Some(parent),
            images: vec![],
        },
    )
    .await;
    loop {
        match recv(client).await {
            ServerMsg::Submitted { task } => return task,
            ServerMsg::Error { message } => panic!("{message}"),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn reply_resumes_the_session_on_the_same_worker() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let mut a = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "what files?".into(),
            cwd: Some("/code/x".into()),
            harness: Some("claude".into()),
            model: Some("haiku".into()),
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task: parent } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut a).await else {
        panic!()
    };
    // A second idle worker joins; the reply must still go to `a`, which has the session.
    let mut b = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(&mut client, &ClientMsg::Subscribe).await;
    for event in [
        AgentEvent::Started {
            session: Some("sess-1".into()),
            model: None,
        },
        AgentEvent::Finished {
            ok: true,
            summary: Some("two".into()),
        },
    ] {
        send(
            &mut a,
            &WorkerMsg::Event {
                task: parent,
                event,
            },
        )
        .await;
        let _: ServerMsg = recv(&mut client).await;
    }

    let task = reply(&mut client, parent, "which have tests?").await;
    let OrchMsg::Assign { task: sent, .. } = recv(&mut a).await else {
        panic!()
    };
    assert_eq!(sent.id, task);
    assert_eq!(
        (
            sent.prompt.as_str(),
            sent.resume.as_deref(),
            sent.cwd.as_deref(),
            sent.model.as_deref()
        ),
        (
            "which have tests?",
            Some("sess-1"),
            Some("/code/x"),
            Some("haiku")
        )
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), b.next())
            .await
            .is_err(),
        "other worker got the reply"
    );
    send(&mut client, &ClientMsg::Status).await;
    let tasks = loop {
        if let ServerMsg::Status { tasks, .. } = recv(&mut client).await {
            break tasks;
        }
    };
    assert_eq!(
        tasks.iter().find(|t| t.id == task).unwrap().parent,
        Some(parent)
    );
}

/// Answers a fresh request itself; routes a follow-up (one carrying context) to claude.
struct ThreadPlanner;
impl mis_agent::Agent for ThreadPlanner {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn models(&self) -> Vec<String> {
        vec![]
    }
    fn run<'a>(
        &'a self,
        _task: TaskId,
        prompt: &'a str,
        _opts: mis_agent::RunOpts<'a>,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> mis_agent::BoxFut<'a> {
        Box::pin(async move {
            let plan = match prompt.contains("This follows up an earlier request.") {
                true => r#"{"harness":"claude","reason":"needs a worker"}"#,
                false => r#"{"answer":"4","reason":"arithmetic"}"#,
            };
            tx.send(AgentEvent::Finished {
                ok: true,
                summary: Some(plan.into()),
            })
            .await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn reply_to_a_planner_answer_is_planned_with_context() {
    let base = start_app(App::new(None).with_planner(std::sync::Arc::new(ThreadPlanner))).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "what is 2+2?".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task: parent } = recv(&mut client).await else {
        panic!()
    };
    // Answered: Text then Finished, no worker involved.
    while !matches!(
        recv(&mut client).await,
        ServerMsg::Event {
            event: AgentEvent::Finished { ok: true, .. },
            ..
        }
    ) {}

    let task = reply(&mut client, parent, "now show it in a file").await;
    let OrchMsg::Assign { task: sent, .. } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!((sent.id, sent.resume.as_deref()), (task, None));
    assert!(sent.prompt.starts_with("now show it in a file\n"));
    assert!(sent
        .prompt
        .ends_with("Earlier request:\nwhat is 2+2?\n\nEarlier answer:\n4"));
}

#[tokio::test]
async fn pr_status_reaches_status_and_show_and_refresh_reaches_the_worker() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let mut worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    send(
        &mut client,
        &ClientMsg::Submit {
            prompt: "open a PR".into(),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        },
    )
    .await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { .. } = recv(&mut worker).await else {
        panic!()
    };
    let url = "https://github.com/o/r/pull/42".to_string();
    let event = AgentEvent::Finished {
        ok: true,
        summary: Some(format!("Opened {url}")),
    };
    send(&mut worker, &WorkerMsg::Event { task, event }).await;
    let ServerMsg::Event { .. } = recv(&mut client).await else {
        panic!()
    };
    // Finishing with a PR link asks the worker to read it.
    let want = OrchMsg::CheckPrs {
        task,
        urls: vec![url.clone()],
    };
    assert_eq!(recv::<OrchMsg>(&mut worker).await, want);
    let status = PrStatus {
        state: PrState::Open,
        checks: Checks::Pending,
        failing: vec![],
        mergeable: Some(true),
        review: None,
        head: "abc".into(),
        observed_at: 1,
    };
    let msg = WorkerMsg::PrStatus {
        task,
        url: url.clone(),
        status: status.clone(),
    };
    send(&mut worker, &msg).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    // Stamped with the orchestrator's clock.
    let got = tasks[0].links[0].pr.clone().unwrap();
    assert!(got.observed_at > 1);
    assert_eq!(
        got,
        PrStatus {
            observed_at: got.observed_at,
            ..status
        }
    );
    send(&mut client, &ClientMsg::Show { task }).await;
    let ServerMsg::TaskDetail { task: info, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(info.links[0].pr, Some(got));
    send(&mut client, &ClientMsg::RefreshEvidence { task }).await;
    assert_eq!(recv::<OrchMsg>(&mut worker).await, want);
}

#[tokio::test]
async fn failed_and_unread_tasks_need_attention_until_seen() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    // One at a time on the one worker: the first fails, the second succeeds.
    let mut ids = vec![];
    for ok in [false, true] {
        let submit = ClientMsg::Submit {
            prompt: format!("ok={ok}"),
            cwd: None,
            harness: Some("claude".into()),
            model: None,
            reply_to: None,
            images: vec![],
        };
        send(&mut client, &submit).await;
        let ServerMsg::Submitted { task } = recv(&mut client).await else {
            panic!()
        };
        let OrchMsg::Assign { task: assigned, .. } = recv(&mut worker).await else {
            panic!()
        };
        assert_eq!(assigned.id, task);
        let event = AgentEvent::Finished { ok, summary: None };
        send(&mut worker, &WorkerMsg::Event { task, event }).await;
        ids.push(task);
    }
    let attention = |tasks: &[TaskSummary]| -> Vec<Attention> {
        ids.iter()
            .map(|id| tasks.iter().find(|t| t.id == *id).unwrap().attention)
            .collect()
    };
    // Status until both finishes have landed.
    let tasks = loop {
        send(&mut client, &ClientMsg::Status).await;
        let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
            panic!()
        };
        if tasks.iter().all(|t| t.status != TaskStatus::Running) {
            break tasks;
        }
    };
    assert_eq!(attention(&tasks), [Attention::Failed, Attention::Unread]);

    send(&mut client, &ClientMsg::Seen { task: ids[1] }).await;
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(attention(&tasks), [Attention::Failed, Attention::None]);
}

/// Local stand-in for a browser vendor's push service: records each POST and
/// answers with `status`.
struct PushStub {
    endpoint: String,
    hits: tokio::sync::mpsc::UnboundedReceiver<(axum::http::HeaderMap, Vec<u8>)>,
    status: std::sync::Arc<std::sync::atomic::AtomicU16>,
}

async fn push_stub() -> PushStub {
    use axum::http::{HeaderMap, StatusCode};
    use std::sync::atomic::Ordering;
    let (tx, hits) = tokio::sync::mpsc::unbounded_channel();
    let status = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(201));
    let answer = status.clone();
    let stub = axum::Router::new().route(
        "/push/dev1",
        axum::routing::post(
            move |headers: HeaderMap, body: axum::body::Bytes| async move {
                tx.send((headers, body.to_vec())).unwrap();
                StatusCode::from_u16(answer.load(Ordering::SeqCst)).unwrap()
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, stub).await });
    PushStub {
        endpoint: format!("http://{addr}/push/dev1"),
        hits,
        status,
    }
}

/// The browser's side of a subscription: its key pair and auth secret.
fn browser_keys() -> (web_push_native::p256::SecretKey, [u8; 16]) {
    (
        web_push_native::p256::SecretKey::from_slice(&[7; 32]).unwrap(),
        [1; 16],
    )
}

fn b64(bytes: &[u8]) -> String {
    use base64ct::{Base64UrlUnpadded, Encoding};
    Base64UrlUnpadded::encode_string(bytes)
}

/// Orchestrator with push on (state in a fresh dir), a client subscribed to
/// `stub`, and a worker. Returns (base, state dir, client, worker).
async fn push_setup(stub: &PushStub) -> (String, std::path::PathBuf, Ws, Ws) {
    let dir = std::env::temp_dir().join(format!("mis-e2e-push-{}", uuid::Uuid::new_v4()));
    let push = mis_orchestrator::push::Push::open(dir.clone(), "mailto:ops@example.invalid".into())
        .unwrap();
    let base = start_app(App::new(None).with_push(push)).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let (ua, auth) = browser_keys();
    let p256dh = b64(&ua.public_key().to_sec1_bytes());
    let endpoint = stub.endpoint.clone();
    let auth = b64(&auth);
    send(
        &mut client,
        &ClientMsg::PushSubscribe {
            endpoint,
            p256dh,
            auth,
        },
    )
    .await;
    // Same socket, handled in order: once Status answers, the subscription is saved.
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut client).await else {
        panic!("subscribe refused?")
    };
    let worker = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    (base, dir, client, worker)
}

/// Submit `prompt` and have the worker fail it.
async fn fail_task(client: &mut Ws, worker: &mut Ws, prompt: &str) -> TaskId {
    let submit = ClientMsg::Submit {
        prompt: prompt.into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(client, &submit).await;
    let ServerMsg::Submitted { task } = recv(client).await else {
        panic!("subscribe refused?")
    };
    let OrchMsg::Assign { .. } = recv(worker).await else {
        panic!()
    };
    let event = AgentEvent::Finished {
        ok: false,
        summary: None,
    };
    send(worker, &WorkerMsg::Event { task, event }).await;
    task
}

async fn next_push(stub: &mut PushStub) -> (axum::http::HeaderMap, Vec<u8>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), stub.hits.recv())
        .await
        .expect("no push")
        .unwrap()
}

async fn no_more_pushes(stub: &mut PushStub) {
    let more = tokio::time::timeout(std::time::Duration::from_millis(300), stub.hits.recv()).await;
    assert!(more.is_err(), "unexpected push");
}

#[tokio::test]
async fn failed_task_pushes_once_encrypted_and_signed() {
    let mut stub = push_stub().await;
    let (base, dir, mut client, mut worker) = push_setup(&stub).await;
    let task = fail_task(&mut client, &mut worker, "fix the mic button").await;
    let (headers, body) = next_push(&mut stub).await;
    no_more_pushes(&mut stub).await;

    let h = |k: &str| headers.get(k).unwrap().to_str().unwrap().to_string();
    assert_eq!(h("content-encoding"), "aes128gcm");
    assert_eq!(h("ttl"), "3600");
    assert_eq!(h("urgency"), "high");
    // VAPID: a JWT for the push service's origin, signed by the key /api/info offers.
    let info = http_get(&base, "/api/info").await;
    let info: serde_json::Value =
        serde_json::from_str(info.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let auth = h("authorization");
    let (jwt, k) = auth
        .strip_prefix("vapid t=")
        .and_then(|rest| rest.split_once(", k="))
        .unwrap();
    assert_eq!(info["vapid"], k);
    use base64ct::{Base64UrlUnpadded, Encoding};
    let point = Base64UrlUnpadded::decode_vec(k).unwrap();
    let key =
        jsonwebtoken::DecodingKey::from_ec_components(&b64(&point[1..33]), &b64(&point[33..]))
            .unwrap();
    let origin = stub.endpoint.trim_end_matches("/push/dev1");
    let mut check = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    check.set_audience(&[origin]);
    let claims = jsonwebtoken::decode::<serde_json::Value>(jwt, &key, &check)
        .unwrap()
        .claims;
    assert_eq!(claims["sub"], "mailto:ops@example.invalid");
    // Only the browser's keys open the payload.
    let (ua, auth) = browser_keys();
    let auth = web_push_native::Auth::clone_from_slice(&auth);
    let plain = web_push_native::decrypt(body, &ua, &auth).unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({
            "title": "failed · fix the mic button",
            "body": "after 0s",
            "tag": task,
            "url": format!("/#task/{task}"),
        })
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn gone_push_subscription_is_dropped() {
    let mut stub = push_stub().await;
    stub.status.store(410, std::sync::atomic::Ordering::SeqCst);
    let (_base, dir, mut client, mut worker) = push_setup(&stub).await;
    let saved = || std::fs::read_to_string(dir.join("push-subscriptions.json")).unwrap();
    assert!(saved().contains(&stub.endpoint));
    fail_task(&mut client, &mut worker, "one").await;
    next_push(&mut stub).await;
    for _ in 0..50 {
        if !saved().contains(&stub.endpoint) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!saved().contains(&stub.endpoint), "{}", saved());
    fail_task(&mut client, &mut worker, "two").await;
    no_more_pushes(&mut stub).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_belled_session_turning_waiting_pushes_once_and_an_unwatched_one_never() {
    let mut stub = push_stub().await;
    let (base, dir, mut client, mut worker) = push_setup(&stub).await;
    let wid = only_worker(&mut client).await.id;
    let (watched, other) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    let at = |id: &str, state, why: Option<&str>| LiveSession {
        harness: "claude".into(),
        id: id.into(),
        name: Some("ci-fix".into()),
        state,
        waiting_for: why.map(String::from),
        ..Default::default()
    };
    let report = |sessions| WorkerMsg::LiveSessions { sessions };
    let working = vec![
        at(&watched, LiveState::Working, None),
        at(&other, LiveState::Working, None),
    ];
    send(&mut worker, &report(working)).await;
    // Listed, so its bell may go on.
    loop {
        let (_, body) = get_json(&base, "/api/sessions", "-").await;
        if serde_json::from_str::<Vec<SessionRow>>(&body)
            .unwrap()
            .len()
            == 2
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let bell = ClientMsg::SessionBell {
        worker: wid,
        harness: "claude".into(),
        id: watched.clone(),
        on: true,
    };
    send(&mut client, &bell).await;
    // Handled in order: a Status answer means the bell went on.
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut client).await else {
        panic!("bell refused?")
    };
    let waiting = vec![
        at(&watched, LiveState::Waiting, Some("permission prompt")),
        at(&other, LiveState::Waiting, Some("permission prompt")),
    ];
    send(&mut worker, &report(waiting.clone())).await;
    let (headers, body) = next_push(&mut stub).await;
    assert_eq!(headers["urgency"], "high");
    let (ua, auth) = browser_keys();
    let auth = web_push_native::Auth::clone_from_slice(&auth);
    let plain = web_push_native::decrypt(body, &ua, &auth).unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    let tag = format!("session/{wid}/claude/{watched}");
    assert_eq!(
        payload,
        serde_json::json!({
            "title": "ci-fix is waiting for you",
            "body": "permission prompt",
            "tag": tag,
            "url": format!("/#{tag}"),
        })
    );
    // Still waiting: nothing more, and never for the other.
    send(&mut worker, &report(waiting)).await;
    no_more_pushes(&mut stub).await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// Minimal HTTP/1.1 request; returns the status, raw head and body bytes.
async fn http(base: &str, head: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("ws://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    // `Host: <addr>` unless `head` names one.
    let host = match head.to_ascii_lowercase().contains("\r\nhost:") {
        true => String::new(),
        false => format!("\r\nHost: {addr}"),
    };
    let req = format!(
        "{head}{host}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(&[req.as_bytes(), body].concat()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(out[..split].to_vec()).unwrap();
    let status = head[9..12].parse().unwrap();
    (status, head, out[split + 4..].to_vec())
}

/// POST `bytes` to `/api/upload`; the status and body.
async fn upload(base: &str, headers: &str, bytes: &[u8]) -> (u16, String) {
    let head = format!("POST /api/upload HTTP/1.1\r\n{headers}");
    let (status, _, body) = http(base, head.trim_end(), bytes).await;
    (status, String::from_utf8(body).unwrap())
}

fn image_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mis-e2e-images-{}", uuid::Uuid::new_v4()))
}

/// A JPEG-headed blob with every byte value, so encoding slips would show.
fn jpeg() -> Vec<u8> {
    let mut b = b"\xFF\xD8\xFF\xE0".to_vec();
    b.extend((0..300_000u32).map(|i| (i % 251) as u8));
    b
}

#[tokio::test]
async fn attached_image_reaches_the_agent_as_a_file() {
    let dir = image_dir();
    let store = mis_orchestrator::images::Images::open(dir.clone()).unwrap();
    let base = start_app(App::new(None).with_images(store)).await;
    let photo = jpeg();
    let (status, body) = upload(&base, "Content-Type: image/jpeg\r\n", &photo).await;
    assert_eq!(status, 200, "{body}");
    let id: uuid::Uuid = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // Magic bytes decide, not the header.
    let (status, _) = upload(&base, "Content-Type: image/jpeg\r\n", b"<html>").await;
    assert_eq!(status, 415);

    // Served back for thumbnails, typed and unsniffable; ids never become paths.
    let (status, head, served) = http(&base, &format!("GET /api/images/{id} HTTP/1.1"), b"").await;
    assert_eq!(status, 200);
    assert!(head.contains("content-type: image/jpeg"), "{head}");
    assert!(head.contains("x-content-type-options: nosniff"), "{head}");
    assert_eq!(served, photo);
    for (path, want) in [
        (format!("/api/images/{}", uuid::Uuid::new_v4()), 404),
        ("/api/images/..%2f..%2fetc%2fpasswd".into(), 400),
        ("/api/images/not-a-uuid".into(), 400),
    ] {
        let (status, ..) = http(&base, &format!("GET {path} HTTP/1.1"), b"").await;
        assert_eq!(status, want, "{path}");
    }

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let submit = |images| ClientMsg::Submit {
        prompt: "what is this?".into(),
        cwd: None,
        harness: Some("codex".into()),
        model: None,
        reply_to: None,
        images,
    };
    // A fifth image, or one never uploaded, is refused.
    for (images, why) in [
        (vec![id; 5], "at most 4 images"),
        (vec![id, uuid::Uuid::new_v4()], "unknown image"),
    ] {
        send(&mut client, &submit(images)).await;
        let ServerMsg::Error { message } = recv(&mut client).await else {
            panic!()
        };
        assert!(message.contains(why), "{message}");
    }
    send(&mut client, &submit(vec![id])).await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks[0].images, [id], "the task view lists its images");

    // The worker gets the bytes with the task, writes them out, and hands the
    // file to the agent: codex as `-i`, claude named in the prompt.
    let mut worker = join(&base, uuid::Uuid::new_v4(), "codex", vec![]).await;
    let OrchMsg::Assign {
        task: assigned,
        images,
        ..
    } = recv(&mut worker).await
    else {
        panic!()
    };
    assert_eq!(assigned.id, task);
    let task_dir = dir.join("worker-task");
    let paths = mis_agent::write_images(&task_dir, &images).unwrap();
    assert_eq!(
        paths,
        [task_dir.join(format!("{id}.jpg")).display().to_string()]
    );
    assert_eq!(std::fs::read(&paths[0]).unwrap(), photo);
    let opts = mis_agent::RunOpts {
        images: &paths,
        ..Default::default()
    };
    let codex = mis_agent::codex::CodexAgent::default().argv(&assigned.prompt, &opts);
    assert_eq!(
        &codex[codex.len() - 4..],
        ["-i", paths[0].as_str(), "--", "what is this?"]
    );
    let claude = mis_agent::claude::ClaudeAgent::default().argv(&assigned.prompt, &opts);
    assert_eq!(
        claude.last().unwrap(),
        &format!(
            "what is this?\n\nAttached images: {} (read them with the Read tool)",
            paths[0]
        )
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn read_token_cannot_upload_but_can_view() {
    let dir = image_dir();
    let store = mis_orchestrator::images::Images::open(dir.clone()).unwrap();
    let base = start_app(with_read_token().with_images(store)).await;
    // Refused before the body is read; with one unread the close would reset the socket.
    let as_ =
        |token: &str| format!("Content-Type: image/jpeg\r\nAuthorization: Bearer {token}\r\n");
    assert_eq!(upload(&base, &as_("r"), b"").await.0, 403);
    assert_eq!(
        upload(&base, "Content-Type: image/jpeg\r\n", b"").await.0,
        401
    );
    let (status, body) = upload(&base, &as_("s"), &jpeg()).await;
    assert_eq!(status, 200, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let get = format!("GET /api/images/{id} HTTP/1.1\r\nAuthorization: Bearer r");
    assert_eq!(http(&base, &get, b"").await.0, 200);
    let get = format!("GET /api/images/{id} HTTP/1.1");
    assert_eq!(http(&base, &get, b"").await.0, 401);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Serve `app` until the sender fires; the handle resolves once it has stopped.
async fn start_stoppable(
    app: App,
) -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let shutdown = async {
        let _ = stopped.await;
    };
    let served = tokio::spawn(mis_orchestrator::serve_until(listener, app, shutdown));
    (format!("ws://{addr}"), stop, served)
}

/// Ask for status until `done` holds: other sockets' messages race ours.
async fn status_until(client: &mut Ws, done: impl Fn(&[TaskSummary]) -> bool) -> Vec<TaskSummary> {
    for _ in 0..100 {
        send(client, &ClientMsg::Status).await;
        let ServerMsg::Status { tasks, .. } = recv(client).await else {
            panic!()
        };
        if done(&tasks) {
            return tasks;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("status never got there");
}

async fn submit_claude(client: &mut Ws, worker: &mut Ws, prompt: &str) -> TaskId {
    let submit = ClientMsg::Submit {
        prompt: prompt.into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(client, &submit).await;
    let ServerMsg::Submitted { task } = recv(client).await else {
        panic!()
    };
    let OrchMsg::Assign { task: assigned, .. } = recv(worker).await else {
        panic!()
    };
    assert_eq!(assigned.id, task);
    task
}

#[tokio::test]
async fn task_history_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-ledger-{}", uuid::Uuid::new_v4()));
    let app = || {
        App::new(None)
            .with_ledger_dir(dir.clone())
            .with_grace(std::time::Duration::from_secs(30))
    };
    let (base, stop, served) = start_stoppable(app()).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let wid = uuid::Uuid::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;

    // One task finishes with a PR, whose status the worker reads; another is mid-run.
    let done = submit_claude(&mut client, &mut worker, "open a PR").await;
    let url = "https://github.com/o/r/pull/42";
    for event in [
        AgentEvent::Text {
            text: format!("opened {url}"),
        },
        AgentEvent::Finished {
            ok: true,
            summary: Some("Opened the PR".into()),
        },
    ] {
        send(&mut worker, &WorkerMsg::Event { task: done, event }).await;
    }
    let OrchMsg::CheckPrs { .. } = recv(&mut worker).await else {
        panic!()
    };
    let pr = PrStatus {
        state: PrState::Merged,
        checks: Checks::Passing,
        failing: vec![],
        mergeable: None,
        review: None,
        head: "abc".into(),
        observed_at: 0,
    };
    let status = WorkerMsg::PrStatus {
        task: done,
        url: url.into(),
        status: pr,
    };
    send(&mut worker, &status).await;
    let running = submit_claude(&mut client, &mut worker, "take a while").await;
    let halfway = AgentEvent::Text {
        text: "halfway".into(),
    };
    let event = WorkerMsg::Event {
        task: running,
        event: halfway.clone(),
    };
    send(&mut worker, &event).await;
    let before = status_until(&mut client, |ts| {
        ts.len() == 2 && ts[0].links[0].pr.is_some() && ts[1].activity == "halfway"
    })
    .await;
    stop.send(()).unwrap();
    served.await.unwrap().unwrap();
    drop((client, worker));

    // A new orchestrator on the same dir: same tasks, same answers and evidence.
    let (base, stop, served) = start_stoppable(app()).await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, workers, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(tasks, before);
    assert_eq!(tasks[0].status, TaskStatus::Done);
    assert_eq!(tasks[0].summary.as_deref(), Some("Opened the PR"));
    assert_eq!(tasks[0].attention, Attention::Unread);
    assert_eq!(tasks[1].status, TaskStatus::Running);
    assert_eq!((workers[0].id, workers[0].online), (wid, false));
    send(&mut client, &ClientMsg::Show { task: done }).await;
    let ServerMsg::TaskDetail {
        events, truncated, ..
    } = recv(&mut client).await
    else {
        panic!()
    };
    assert_eq!(events.len(), 2);
    assert!(!truncated);

    // The worker comes back still running its task, and finishes it.
    let mut worker = join(&base, wid, "claude", vec![running]).await;
    let finished = AgentEvent::Finished {
        ok: true,
        summary: Some("took a while".into()),
    };
    let event = WorkerMsg::Event {
        task: running,
        event: finished,
    };
    send(&mut worker, &event).await;
    status_until(&mut client, |ts| ts[1].status == TaskStatus::Done).await;
    send(&mut client, &ClientMsg::Show { task: running }).await;
    let ServerMsg::TaskDetail { events, .. } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(events[0], halfway, "transcript from before the restart");
    stop.send(()).unwrap();
    served.await.unwrap().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn supervisor_reports_update_live_and_read_token_cannot() {
    let base = start_app(with_read_token()).await;
    let (mut page, _) = connect_async(client_req(&base, &[("authorization", "Bearer r")]))
        .await
        .unwrap();
    send(&mut page, &ClientMsg::Subscribe).await;
    send(&mut page, &ClientMsg::Status).await;
    let ServerMsg::Status { update, .. } = recv(&mut page).await else {
        panic!()
    };
    assert_eq!(update, Update::None);

    let post = |auth: &str| format!("POST /api/update HTTP/1.1\r\n{auth}");
    let pending = br#"{"state":"pending","sha":"a1b2c3d","subject":"fix the thing"}"#;
    assert_eq!(
        http(&base, &post("Authorization: Bearer r"), pending)
            .await
            .0,
        403
    );
    assert_eq!(http(&base, post("").trim_end(), pending).await.0, 401);
    let bad = br#"{"state":"pending","sha":"nope","subject":"x"}"#;
    assert_eq!(
        http(&base, &post("Authorization: Bearer s"), bad).await.0,
        400
    );
    assert_eq!(
        http(&base, &post("Authorization: Bearer s"), pending)
            .await
            .0,
        204
    );

    // The page sees it without asking.
    let ServerMsg::Status { update, .. } = recv(&mut page).await else {
        panic!()
    };
    let want = Update::Pending {
        sha: "a1b2c3d".into(),
        subject: "fix the thing".into(),
    };
    assert_eq!(update, want);
    // The supervisor polls for a gap; any admitted token may look.
    let (status, _, body) = http(
        &base,
        "GET /api/update HTTP/1.1\r\nAuthorization: Bearer r",
        b"",
    )
    .await;
    assert_eq!(status, 200);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["running"], 0);
    assert_eq!(body["update"]["state"], "pending");
    assert_eq!(http(&base, "GET /api/update HTTP/1.1", b"").await.0, 401);
}

#[tokio::test]
async fn worker_purpose_and_inventory_reach_status() {
    let base = start().await;
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    // A worker predating purpose/inventory still connects.
    let (mut old, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    let hello = r#"{"type":"hello","id":"00000000-0000-0000-0000-000000000001","running":[],
        "info":{"hostname":"old","os":"o","arch":"a","harnesses":["claude"]}}"#;
    old.send(Message::Text(hello.into())).await.unwrap();
    assert!(matches!(recv(&mut old).await, OrchMsg::Welcome { .. }));
    let w = only_worker(&mut client).await;
    assert_eq!(
        (w.info.purpose, w.info.inventory),
        (None, Inventory::default())
    );
    drop(old);

    let wid = uuid::Uuid::new_v4();
    let blender = WorkerInfo {
        purpose: Some("Blender + blender-mcp; 3D modelling, glTF export".into()),
        inventory: Inventory {
            mcp: [("claude".into(), vec!["blender".into()])].into(),
            apps: vec![InstalledApp {
                name: "Blender".into(),
                version: Some("4.2.3".into()),
            }],
            machine: Machine {
                cpus: 12,
                mem_gb: 32.0,
                gpu: Some("Apple M2 Max".into()),
                ..Default::default()
            },
        },
        ..info("claude")
    };
    let (mut ws, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    let hello = WorkerMsg::Hello {
        id: wid,
        info: blender.clone(),
        running: vec![],
    };
    send(&mut ws, &hello).await;
    assert!(matches!(recv(&mut ws).await, OrchMsg::Welcome { .. }));
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { workers, .. } = recv(&mut client).await else {
        panic!()
    };
    let w = workers.iter().find(|w| w.id == wid).unwrap();
    assert_eq!(w.info, blender);
}

#[tokio::test]
async fn planner_routes_to_the_named_worker() {
    let planner = FakePlanner(r#"{"harness":"claude","worker":"studio","reason":"needs Blender"}"#);
    let base = start_app(App::new(None).with_planner(std::sync::Arc::new(planner))).await;
    // An idle claude worker that is not the studio, joined first.
    let mut other = join(&base, uuid::Uuid::new_v4(), "claude", vec![]).await;
    let _codex = join(&base, uuid::Uuid::new_v4(), "codex", vec![]).await;
    let studio_id = uuid::Uuid::new_v4();
    let (mut studio, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    let info = WorkerInfo {
        hostname: "studio".into(),
        purpose: Some("Blender + blender-mcp; 3D assets".into()),
        ..info("claude")
    };
    let hello = WorkerMsg::Hello {
        id: studio_id,
        info,
        running: vec![],
    };
    send(&mut studio, &hello).await;
    assert!(matches!(recv(&mut studio).await, OrchMsg::Welcome { .. }));

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let submit = ClientMsg::Submit {
        prompt: "model a low-poly chair and export glTF".into(),
        cwd: None,
        harness: None,
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(&mut client, &submit).await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { task: assigned, .. } = recv(&mut studio).await else {
        panic!()
    };
    assert_eq!(assigned.id, task);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), other.next())
            .await
            .is_err(),
        "the other worker got it"
    );
}

/// Drafter stub: replies with a quoted note, as models sometimes do.
struct FakeDrafter;
impl mis_agent::Agent for FakeDrafter {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn models(&self) -> Vec<String> {
        vec![]
    }
    fn run<'a>(
        &'a self,
        _task: TaskId,
        prompt: &'a str,
        _opts: mis_agent::RunOpts<'a>,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> mis_agent::BoxFut<'a> {
        Box::pin(async move {
            assert!(prompt.contains("New: Houdini"), "{prompt}");
            let summary = Some("\"Blender + Houdini; 3D assets\"".into());
            tx.send(AgentEvent::Finished { ok: true, summary }).await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn purpose_edit_reaches_the_worker_and_suggestion_drafts_only() {
    let base = start_app(App::new(None).with_drafter(std::sync::Arc::new(FakeDrafter))).await;
    let wid = uuid::Uuid::new_v4();
    let hello = |apps: &[&str]| WorkerMsg::Hello {
        id: wid,
        info: WorkerInfo {
            purpose: Some("Blender; 3D assets".into()),
            inventory: Inventory {
                apps: (apps.iter())
                    .map(|a| InstalledApp {
                        name: a.to_string(),
                        version: None,
                    })
                    .collect(),
                ..Default::default()
            },
            ..info("claude")
        },
        running: vec![],
    };
    let (mut first, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    send(&mut first, &hello(&["Blender"])).await;
    assert!(matches!(recv(&mut first).await, OrchMsg::Welcome { .. }));
    drop(first);
    // Back with Houdini installed.
    let (mut worker, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    send(&mut worker, &hello(&["Blender", "Houdini"])).await;
    assert!(matches!(recv(&mut worker).await, OrchMsg::Welcome { .. }));
    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    let w = only_worker(&mut client).await;
    assert_eq!(w.new_tools, ["Houdini"]);

    // A suggestion is a draft for the user; nothing reaches the worker.
    send(&mut client, &ClientMsg::Subscribe).await;
    send(&mut client, &ClientMsg::SuggestPurpose { worker: wid }).await;
    let draft = loop {
        if let ServerMsg::PurposeDraft {
            worker,
            draft,
            error,
        } = recv(&mut client).await
        {
            assert_eq!((worker, error), (wid, None));
            break draft;
        }
    };
    assert_eq!(draft, "Blender + Houdini; 3D assets");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), worker.next())
            .await
            .is_err(),
        "suggesting changed the worker"
    );

    // The user accepts it: the worker is told, saves, confirms; then it shows.
    let set = ClientMsg::SetPurpose {
        worker: wid,
        purpose: draft.clone(),
    };
    send(&mut client, &set).await;
    let OrchMsg::SetPurpose { purpose } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!(purpose.as_deref(), Some(draft.as_str()));
    send(&mut worker, &WorkerMsg::Purpose { purpose }).await;
    let w = loop {
        if let ServerMsg::Status { workers, .. } = recv(&mut client).await {
            break workers.into_iter().next().unwrap();
        }
    };
    assert_eq!(w.info.purpose.as_deref(), Some(draft.as_str()));
    assert!(w.new_tools.is_empty());
}

#[tokio::test]
async fn read_token_cannot_edit_a_purpose() {
    let base = start_app(with_read_token()).await;
    let req = client_req(&base, &[("authorization", "Bearer r")]);
    let (mut client, _) = connect_async(req).await.unwrap();
    let set = ClientMsg::SetPurpose {
        worker: uuid::Uuid::nil(),
        purpose: "x".into(),
    };
    send(&mut client, &set).await;
    let ServerMsg::Error { message } = recv(&mut client).await else {
        panic!()
    };
    assert_eq!(message, "read-only token");
}

#[tokio::test]
async fn workers_learn_the_version_and_a_pending_update_takes_no_tasks() {
    let base = start_app(App::new(None).with_channel(Some(Channel::Edge))).await;
    let wid = uuid::Uuid::new_v4();
    let (mut worker, _) = connect_async(format!("{base}/ws/worker")).await.unwrap();
    let hello = WorkerMsg::Hello {
        id: wid,
        info: info("claude"),
        running: vec![],
    };
    send(&mut worker, &hello).await;
    let welcome: OrchMsg = recv(&mut worker).await;
    assert_eq!(
        welcome,
        OrchMsg::Welcome {
            id: wid,
            version: Some(mis_orchestrator::VERSION.into()),
            channel: Some(Channel::Edge),
        }
    );
    let pending = WorkerUpdate::Pending {
        version: "0.2.0+bbbbbbb".into(),
    };
    let update = WorkerMsg::Update {
        update: pending.clone(),
    };
    send(&mut worker, &update).await;

    let (mut client, _) = connect_async(format!("{base}/ws/client")).await.unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let submit = ClientMsg::Submit {
        prompt: "hi".into(),
        cwd: None,
        harness: Some("claude".into()),
        model: None,
        reply_to: None,
        images: vec![],
    };
    send(&mut client, &submit).await;
    let ServerMsg::Submitted { task } = recv(&mut client).await else {
        panic!()
    };
    send(&mut client, &ClientMsg::Status).await;
    let status = loop {
        if let ServerMsg::Status { workers, tasks, .. } = recv(&mut client).await {
            break (workers, tasks);
        }
    };
    assert_eq!(status.0[0].update, pending);
    assert_eq!(
        status.1[0].status,
        TaskStatus::Queued,
        "held off the updating worker"
    );
    let held: OrchMsg = recv(&mut worker).await;
    assert_eq!(
        held,
        OrchMsg::UpdateHeld,
        "acknowledged: no more tasks this connection"
    );
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), worker.next()).await;
    assert!(quiet.is_err(), "no assignment while its update is pending");

    // Back on its new build (a new connection), it takes the task.
    drop(worker);
    let mut worker = join(&base, wid, "claude", vec![]).await;
    let OrchMsg::Assign { task: assigned, .. } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!(assigned.id, task);
}

#[tokio::test]
async fn sessions_are_asked_of_the_worker_on_demand() {
    let app = App::new(None).with_query_timeout(std::time::Duration::from_millis(300));
    let base = start_app(app).await;
    let unknown = http_get(
        &base,
        &format!("/api/workers/{}/sessions", WorkerId::new_v4()),
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 404"), "{unknown}");

    let wid = WorkerId::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    let path = format!("/api/workers/{wid}/sessions");
    let get = tokio::spawn({
        let (base, path) = (base.clone(), path.clone());
        async move { http_get(&base, &path).await }
    });
    let OrchMsg::Query { req, query } = recv(&mut worker).await else {
        panic!("expected a query")
    };
    assert_eq!(query, Query::ListSessions { headless: false });
    let session = Session {
        harness: "codex".into(),
        id: "s-1".into(),
        title: Some("rename the crate".into()),
        updated_ms: 7,
        ..Default::default()
    };
    let sessions = vec![session];
    let answer = Answer::Sessions {
        sessions: sessions.clone(),
    };
    send(&mut worker, &WorkerMsg::Answer { req, answer }).await;
    let got = get.await.unwrap();
    assert!(got.starts_with("HTTP/1.1 200"), "{got}");
    let body = got.split("\r\n\r\n").nth(1).unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<Session>>(body).unwrap(),
        sessions
    );

    // Unanswered: 504 once the timeout passes.
    let silent = http_get(&base, &format!("{path}?headless=true")).await;
    assert!(silent.starts_with("HTTP/1.1 504"), "{silent}");
    let OrchMsg::Query { query, .. } = recv(&mut worker).await else {
        panic!("expected the unanswered query")
    };
    assert_eq!(query, Query::ListSessions { headless: true });

    // Disconnected mid-query: 503 at once, not after the timeout.
    let get = tokio::spawn({
        let (base, path) = (base.clone(), path.clone());
        async move { http_get(&base, &path).await }
    });
    let OrchMsg::Query { .. } = recv(&mut worker).await else {
        panic!("expected a query")
    };
    drop(worker);
    let gone = get.await.unwrap();
    assert!(gone.starts_with("HTTP/1.1 503"), "{gone}");
}

#[tokio::test]
async fn a_session_transcript_is_read_from_the_worker() {
    let base = start().await;
    let wid = WorkerId::new_v4();
    let mut worker = join(&base, wid, "claude", vec![]).await;
    let id = uuid::Uuid::new_v4();
    let bad = http_get(
        &base,
        &format!("/api/workers/{wid}/sessions/claude/..%2F..%2Fetc"),
    )
    .await;
    assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");

    let path = format!("/api/workers/{wid}/sessions/claude/{id}?limit=5");
    let get = tokio::spawn({
        let base = base.clone();
        async move { http_get(&base, &path).await }
    });
    let OrchMsg::Query { req, query } = recv(&mut worker).await else {
        panic!("expected a query")
    };
    let harness = "claude".to_string();
    assert_eq!(
        query,
        Query::ReadSession {
            harness,
            id,
            limit: 5
        }
    );
    let answer = Answer::Transcript {
        turns: vec![Turn {
            role: Role::Tool,
            text: "Bash: ls".into(),
            at: None,
        }],
        more: true,
        updated_ms: 7,
        active: false,
        open: false,
    };
    send(
        &mut worker,
        &WorkerMsg::Answer {
            req,
            answer: answer.clone(),
        },
    )
    .await;
    let got = get.await.unwrap();
    assert!(got.starts_with("HTTP/1.1 200"), "{got}");
    let body = got.split("\r\n\r\n").nth(1).unwrap();
    assert_eq!(serde_json::from_str::<Answer>(body).unwrap(), answer);

    // Not on the worker's disk: 404.
    let path = format!("/api/workers/{wid}/sessions/codex/{id}");
    let get = tokio::spawn({
        let base = base.clone();
        async move { http_get(&base, &path).await }
    });
    let OrchMsg::Query { req, query } = recv(&mut worker).await else {
        panic!("expected a query")
    };
    assert!(
        matches!(query, Query::ReadSession { limit: 30, .. }),
        "default limit"
    );
    let answer = Answer::NotFound;
    send(&mut worker, &WorkerMsg::Answer { req, answer }).await;
    let got = get.await.unwrap();
    assert!(got.starts_with("HTTP/1.1 404"), "{got}");
}

/// POST a JSON `body` with bearer `auth`; the status and body.
async fn post_json(base: &str, path: &str, auth: &str, body: &str) -> (u16, String) {
    let head = format!(
        "POST {path} HTTP/1.1\r\nAuthorization: Bearer {auth}\r\nContent-Type: application/json"
    );
    let (status, _, body) = http(base, &head, body.as_bytes()).await;
    (status, String::from_utf8(body).unwrap())
}

#[tokio::test]
async fn messaging_a_session_runs_a_task_on_its_worker() {
    let base = start_app(with_read_token()).await;
    let wid = WorkerId::new_v4();
    let mut req = format!("{base}/ws/worker").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer s".parse().unwrap());
    let (mut worker, _) = connect_async(req).await.unwrap();
    let info = WorkerInfo {
        deliver: true,
        ..info("claude")
    };
    let hello = WorkerMsg::Hello {
        id: wid,
        info,
        running: vec![],
    };
    send(&mut worker, &hello).await;
    let OrchMsg::Welcome { .. } = recv(&mut worker).await else {
        panic!("welcome")
    };

    let session = uuid::Uuid::new_v4();
    let path = format!("/api/workers/{wid}/sessions/claude/{session}/messages");
    let body = r#"{"text":"also add coyote time","title":"Tune jump physics"}"#;
    // Never into a copy.
    let fork = r#"{"text":"also add coyote time","mode":"fork"}"#;
    assert_eq!(post_json(&base, &path, "s", fork).await.0, 400);
    // Acting needs the full token.
    assert_eq!(post_json(&base, &path, "r", body).await.0, 403);
    assert_eq!(post_json(&base, &path, "nope", body).await.0, 401);
    let other = format!(
        "/api/workers/{}/sessions/claude/{session}/messages",
        WorkerId::new_v4()
    );
    assert_eq!(post_json(&base, &other, "s", body).await.0, 404);
    let codex = format!("/api/workers/{wid}/sessions/codex/{session}/messages");
    assert_eq!(post_json(&base, &codex, "s", body).await.0, 400);

    let (status, ok) = post_json(&base, &path, "s", body).await;
    assert_eq!(status, 200, "{ok}");
    let OrchMsg::Assign { task, .. } = recv(&mut worker).await else {
        panic!("assign")
    };
    assert!(ok.contains(&task.id.to_string()), "{ok}");
    assert_eq!(
        (task.harness.as_str(), task.prompt.as_str()),
        ("claude", "also add coyote time")
    );
    assert_eq!(task.resume, Some(session.to_string()));
    let to = SendTo {
        session,
        title: Some("Tune jump physics".into()),
        mode: SendMode::Deliver,
    };
    assert_eq!(task.send_to, Some(to));
}

/// GET `path` with bearer `auth`; the status and body.
async fn get_json(base: &str, path: &str, auth: &str) -> (u16, String) {
    let head = format!("GET {path} HTTP/1.1\r\nAuthorization: Bearer {auth}");
    let (status, _, body) = http(base, &head, b"").await;
    (status, String::from_utf8(body).unwrap())
}

/// Wait until a subscribed `client` sees `task` finish.
async fn finished(client: &mut Ws, task: TaskId) {
    loop {
        if let ServerMsg::Event {
            task: t,
            event: AgentEvent::Finished { .. },
            ..
        } = recv(client).await
        {
            if t == task {
                return;
            }
        }
    }
}

#[tokio::test]
async fn routine_files_run_as_tagged_tasks_run_now_and_pause() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-routines-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |file: &str, text: &str| std::fs::write(dir.join(file), text).unwrap();
    write(
        "count.md",
        "---\nevery: 1h\nmodel: haiku\n---\nCount the files.\n",
    );
    write(
        "nightly.md",
        "---\ncron: 0 3 * * *\npaused: true\n---\nSweep.\n",
    );
    write("bad.md", "no frontmatter");
    let every = std::time::Duration::from_millis(100);
    let base = start_app(with_read_token().with_routines(dir.clone(), every)).await;
    let mut req = format!("{base}/ws/worker").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer s".parse().unwrap());
    let (mut worker, _) = connect_async(req).await.unwrap();
    let hello = WorkerMsg::Hello {
        id: WorkerId::new_v4(),
        info: info("claude"),
        running: vec![],
    };
    send(&mut worker, &hello).await;
    let OrchMsg::Welcome { .. } = recv(&mut worker).await else {
        panic!("welcome")
    };
    let finish = |task| WorkerMsg::Event {
        task,
        event: AgentEvent::Finished {
            ok: true,
            summary: Some("12 files".into()),
        },
    };

    // A new `every` routine is due at once: a report-only task, tagged with its name.
    let OrchMsg::Assign { task, .. } = recv(&mut worker).await else {
        panic!("assign")
    };
    assert!(task
        .prompt
        .starts_with("Routine \"count\" (scheduled run, report only)"));
    assert!(task.prompt.ends_with("\n\nCount the files."));
    assert_eq!(
        (task.harness.as_str(), task.model.as_deref()),
        ("claude", Some("haiku"))
    );
    let (mut client, _) = connect_async(client_req(&base, &[("authorization", "Bearer r")]))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { tasks, .. } = recv(&mut client).await else {
        panic!("status")
    };
    assert_eq!(tasks[0].routine.as_deref(), Some("count"));
    send(&mut client, &ClientMsg::Subscribe).await;

    // Listed with read access: schedule, last run, and the bad file's error.
    let (status, body) = get_json(&base, "/api/routines", "r").await;
    assert_eq!(status, 200, "{body}");
    let list: Vec<RoutineView> = serde_json::from_str(&body).unwrap();
    let names: Vec<_> = list.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["bad", "count", "nightly"]);
    assert!(list[0].error.as_deref().unwrap().contains("frontmatter"));
    let count = &list[1];
    assert_eq!(count.trigger.as_deref(), Some("every 1h"));
    assert_eq!(count.last_task, Some(task.id));
    assert_eq!(count.last_status, Some(TaskStatus::Running));
    assert!(list[2].paused && list[2].next_at.is_none() && list[2].last_at.is_none());

    // Run now: acting needs the full token, and waits for the last run to finish.
    let run = "/api/routines/count/run";
    assert_eq!(post_json(&base, run, "r", "").await.0, 403);
    assert_eq!(post_json(&base, run, "s", "").await.0, 409);
    assert_eq!(
        post_json(&base, "/api/routines/nope/run", "s", "").await.0,
        404
    );
    assert_eq!(
        post_json(&base, "/api/routines/bad/run", "s", "").await.0,
        400
    );
    send(&mut worker, &finish(task.id)).await;
    finished(&mut client, task.id).await;
    let (status, body) = post_json(&base, run, "s", "").await;
    assert_eq!(status, 200, "{body}");
    let OrchMsg::Assign { task: again, .. } = recv(&mut worker).await else {
        panic!("assign")
    };
    assert!(body.contains(&again.id.to_string()), "{body}");
    send(&mut worker, &finish(again.id)).await;
    finished(&mut client, again.id).await;

    // Resume overrides the file's `paused`; pause stops the next run.
    let resume = "/api/routines/nightly/resume";
    assert_eq!(post_json(&base, resume, "s", "").await.0, 204);
    let (_, body) = get_json(&base, "/api/routines", "r").await;
    let list: Vec<RoutineView> = serde_json::from_str(&body).unwrap();
    assert!(!list[2].paused && list[2].next_at.is_some());
    assert_eq!(
        post_json(&base, "/api/routines/count/pause", "s", "")
            .await
            .0,
        204
    );
    let (_, body) = get_json(&base, "/api/routines", "r").await;
    let list: Vec<RoutineView> = serde_json::from_str(&body).unwrap();
    assert!(list[1].paused && list[1].next_at.is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn new_routine_is_written_paused() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-new-routine-{}", uuid::Uuid::new_v4()));
    let every = std::time::Duration::from_secs(60);
    let base = start_app(with_read_token().with_routines(dir.clone(), every)).await;
    let body = r#"{"name":"deps","schedule":"1d","prompt":"Check deps."}"#;

    assert_eq!(post_json(&base, "/api/routines", "r", body).await.0, 403);
    let (status, reply) = post_json(&base, "/api/routines", "s", body).await;
    assert_eq!(status, 201, "{reply}");
    let text = std::fs::read_to_string(dir.join("deps.md")).unwrap();
    assert_eq!(text, "---\nevery: 1d\npaused: true\n---\nCheck deps.\n");
    let (_, list) = get_json(&base, "/api/routines", "r").await;
    let list: Vec<RoutineView> = serde_json::from_str(&list).unwrap();
    assert!(list[0].name == "deps" && list[0].paused && list[0].mode == Some(RoutineMode::Report));

    // Taken names and bad schedules are refused, and nothing is overwritten.
    assert_eq!(post_json(&base, "/api/routines", "s", body).await.0, 409);
    let bad = r#"{"name":"x","schedule":"soon","prompt":"Hi."}"#;
    let (status, reply) = post_json(&base, "/api/routines", "s", bad).await;
    assert!(
        status == 400 && reply.contains("schedule"),
        "{status} {reply}"
    );
    assert!(!dir.join("x.md").exists());

    // A name reused after its file is deleted starts paused, not as the old one was left.
    let resume = "/api/routines/deps/resume";
    assert_eq!(post_json(&base, resume, "s", "").await.0, 204);
    std::fs::remove_file(dir.join("deps.md")).unwrap();
    assert_eq!(post_json(&base, "/api/routines", "s", body).await.0, 201);
    let (_, list) = get_json(&base, "/api/routines", "r").await;
    let list: Vec<RoutineView> = serde_json::from_str(&list).unwrap();
    assert!(list[0].paused && list[0].next_at.is_none(), "{list:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn settings_take_defaults_save_and_need_full_access() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-settings-{}", uuid::Uuid::new_v4()));
    let base = start_app(with_read_token().with_ledger_dir(dir.clone())).await;
    let (status, body) = get_json(&base, "/api/settings", "s").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Settings>(&body).unwrap(),
        Settings::default()
    );

    let mut s = Settings::default();
    s.apps.model = " sonnet ".into();
    let new = serde_json::to_string(&s).unwrap();
    assert_eq!(post_json(&base, "/api/settings", "s", &new).await.0, 204);
    let (_, body) = get_json(&base, "/api/settings", "s").await;
    let got: Settings = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (got.apps.model.as_str(), got.apps.harness.as_str()),
        ("sonnet", "claude")
    );
    assert!(dir.join("settings.json").exists());

    // Read-only tokens neither see nor change them; a blank value is refused, nothing saved.
    assert_eq!(get_json(&base, "/api/settings", "r").await.0, 403);
    assert_eq!(post_json(&base, "/api/settings", "r", &new).await.0, 403);
    let (status, why) = post_json(&base, "/api/settings", "s", r#"{"apps":{"repo":" "}}"#).await;
    assert!(status == 400 && why.contains("apps repo"), "{status} {why}");
    let (_, body) = get_json(&base, "/api/settings", "s").await;
    assert_eq!(serde_json::from_str::<Settings>(&body).unwrap(), got);
    // The running ledger may still be saving into it.
    let _ = std::fs::remove_dir_all(dir);
}

/// A worker running claude, joined with the full token `s`.
async fn join_as(base: &str) -> (WorkerId, Ws) {
    let mut req = format!("{base}/ws/worker").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer s".parse().unwrap());
    let (mut ws, _) = connect_async(req).await.unwrap();
    let id = WorkerId::new_v4();
    let hello = WorkerMsg::Hello {
        id,
        info: info("claude"),
        running: vec![],
    };
    send(&mut ws, &hello).await;
    let OrchMsg::Welcome { .. } = recv(&mut ws).await else {
        panic!("welcome")
    };
    (id, ws)
}

#[tokio::test]
async fn new_app_task_runs_the_apps_guide_in_the_settings_repo_and_its_replies_never_merge() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-app-task-{}", uuid::Uuid::new_v4()));
    let base = start_app(with_read_token().with_ledger_dir(dir.clone())).await;
    let mut s = Settings::default();
    s.apps.repo = "~/code/household".into();
    let s = serde_json::to_string(&s).unwrap();
    assert_eq!(post_json(&base, "/api/settings", "s", &s).await.0, 204);
    let (_, mut worker) = join_as(&base).await;

    let body =
        r#"{"request":"Track pool chlorine.","slug":"pool","harness":"claude","model":"opus"}"#;
    assert_eq!(post_json(&base, "/api/apps/tasks", "r", body).await.0, 403);
    let (status, created) = post_json(&base, "/api/apps/tasks", "s", body).await;
    assert_eq!(status, 201, "{created}");
    let id: TaskId = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(&created).unwrap()["task"].clone(),
    )
    .unwrap();
    let OrchMsg::Assign { task, .. } = recv(&mut worker).await else {
        panic!("assign")
    };
    assert_eq!(task.id, id);
    assert!(task
        .prompt
        .starts_with("Build a new Make It So app, slug `pool`."));
    assert!(task.prompt.contains("Request:\nTrack pool chlorine.\n"));
    assert!(task
        .prompt
        .ends_with(mis_types::guides::guide("apps").unwrap()));
    assert_eq!(task.cwd.as_deref(), Some("~/code/household"));
    assert_eq!(
        (task.harness.as_str(), task.model.as_deref()),
        ("claude", Some("opus"))
    );
    assert_eq!(task.merge, MergeRule::Never);

    // A reply in its thread resumes the session, and can't merge either.
    let (mut client, _) = connect_async(client_req(&base, &[("authorization", "Bearer s")]))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    // Subscribed once Status answers: the worker's events come on another socket.
    send(&mut client, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut client).await else {
        panic!("status")
    };
    for event in [
        AgentEvent::Started {
            session: Some("sess-1".into()),
            model: None,
        },
        AgentEvent::Finished {
            ok: true,
            summary: Some("PR opened".into()),
        },
    ] {
        send(&mut worker, &WorkerMsg::Event { task: id, event }).await;
        let _: ServerMsg = recv(&mut client).await;
    }
    let again = reply(&mut client, id, "Looks good, merge it.").await;
    let OrchMsg::Assign { task, .. } = recv(&mut worker).await else {
        panic!("assign")
    };
    assert_eq!((task.id, task.resume.as_deref()), (again, Some("sess-1")));
    assert_eq!(task.merge, MergeRule::Never);

    // A blank request or an unknown harness starts nothing.
    for bad in [
        r#"{"request":" ","harness":"claude"}"#,
        r#"{"request":"x","harness":"gpt"}"#,
    ] {
        assert_eq!(
            post_json(&base, "/api/apps/tasks", "s", bad).await.0,
            400,
            "{bad}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn change_app_task_runs_only_on_the_picked_worker() {
    let base = start_app(with_read_token()).await;
    let (_, mut first) = join_as(&base).await;
    let (picked, mut second) = join_as(&base).await;
    let body = serde_json::json!({
        "request": "Add a pH chart.", "slug": "pool", "change": true,
        "harness": "claude", "worker": picked,
    });
    let (status, reply) = post_json(&base, "/api/apps/tasks", "s", &body.to_string()).await;
    assert_eq!(status, 201, "{reply}");
    let OrchMsg::Assign { task, .. } = recv(&mut second).await else {
        panic!("assign")
    };
    assert!(task
        .prompt
        .starts_with("Change the Make It So app `pool` (`apps/pool/`)."));
    // No state dir: the default repo; no model: the harness's.
    assert_eq!(
        (task.cwd.as_deref(), task.model),
        (Some("~/code/mis-notes"), None)
    );

    // Another for the busy picked worker waits for it, though the other one is idle.
    let (status, reply) = post_json(&base, "/api/apps/tasks", "s", &body.to_string()).await;
    assert_eq!(status, 201, "{reply}");
    let other = tokio::time::timeout(std::time::Duration::from_millis(200), recv(&mut first));
    if let Ok(OrchMsg::Assign { .. }) = other.await {
        panic!("the other worker got it");
    }
    let done = AgentEvent::Finished {
        ok: true,
        summary: None,
    };
    send(
        &mut second,
        &WorkerMsg::Event {
            task: task.id,
            event: done,
        },
    )
    .await;
    let OrchMsg::Assign { task: next, .. } = recv(&mut second).await else {
        panic!("assign")
    };
    assert_ne!(next.id, task.id);

    // A worker that isn't connected can't be picked.
    let gone = serde_json::json!({ "request": "x", "slug": "pool", "change": true,
        "harness": "claude", "worker": WorkerId::new_v4() });
    let (status, why) = post_json(&base, "/api/apps/tasks", "s", &gone.to_string()).await;
    assert!(status == 400 && why.contains("offline"), "{status} {why}");
}

#[tokio::test]
async fn acting_routine_runs_dry_until_switched_live() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-act-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |file: &str, text: &str| std::fs::write(dir.join(file), text).unwrap();
    write("fix.md", "---\nevery: 1h\nact: true\n---\nFix lint.\n");
    write("look.md", "---\nevery: 1h\npaused: true\n---\nLook.\n");
    let every = std::time::Duration::from_millis(100);
    let base = start_app(with_read_token().with_routines(dir.clone(), every)).await;
    let mut req = format!("{base}/ws/worker").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer s".parse().unwrap());
    let (mut worker, _) = connect_async(req).await.unwrap();
    let hello = WorkerMsg::Hello {
        id: WorkerId::new_v4(),
        info: info("claude"),
        running: vec![],
    };
    send(&mut worker, &hello).await;
    let OrchMsg::Welcome { .. } = recv(&mut worker).await else {
        panic!("welcome")
    };
    let (mut client, _) = connect_async(client_req(&base, &[("authorization", "Bearer r")]))
        .await
        .unwrap();
    send(&mut client, &ClientMsg::Subscribe).await;
    let finish = |task| WorkerMsg::Event {
        task,
        event: AgentEvent::Finished {
            ok: true,
            summary: None,
        },
    };

    // `act: true` alone: a dry run, read-only.
    let OrchMsg::Assign { task, .. } = recv(&mut worker).await else {
        panic!("assign")
    };
    assert!(task
        .prompt
        .starts_with("Routine \"fix\" (scheduled run, dry run)"));
    assert!(task.read_only);
    send(&mut worker, &finish(task.id)).await;
    finished(&mut client, task.id).await;
    let mode = |name: &'static str| {
        let base = base.clone();
        async move {
            let (_, body) = get_json(&base, "/api/routines", "r").await;
            let list: Vec<RoutineView> = serde_json::from_str(&body).unwrap();
            list.into_iter().find(|r| r.name == name).unwrap().mode
        }
    };
    assert_eq!(mode("fix").await, Some(RoutineMode::DryRun));
    assert_eq!(mode("look").await, Some(RoutineMode::Report));

    // Going live: full token only, and only for a file that says `act: true`.
    let live = "/api/routines/fix/live";
    assert_eq!(post_json(&base, live, "r", "").await.0, 403);
    let (status, body) = post_json(&base, "/api/routines/look/live", "s", "").await;
    assert_eq!((status, body.contains("act: true")), (400, true), "{body}");
    let dry = "/api/routines/look/dry-run";
    assert_eq!(post_json(&base, dry, "s", "").await.0, 400);
    // Only with the pin of the version reviewed.
    let (status, body) = post_json(&base, live, "s", "").await;
    assert_eq!((status, body.contains("pin")), (400, true), "{body}");
    let (_, body) = get_json(&base, "/api/routines", "r").await;
    let list: Vec<RoutineView> = serde_json::from_str(&body).unwrap();
    let pin = list
        .iter()
        .find(|r| r.name == "fix")
        .unwrap()
        .pin
        .clone()
        .unwrap();
    let pin = serde_json::json!({ "pin": pin }).to_string();
    assert_eq!(post_json(&base, live, "s", &pin).await.0, 204);
    assert_eq!(mode("fix").await, Some(RoutineMode::Live));

    // A live run may act, and its task says so.
    let (status, body) = post_json(&base, "/api/routines/fix/run", "s", "").await;
    assert_eq!(status, 200, "{body}");
    let OrchMsg::Assign { task, .. } = recv(&mut worker).await else {
        panic!("assign")
    };
    assert!(task
        .prompt
        .starts_with("Routine \"fix\" (scheduled run, live)"));
    assert!(!task.read_only);
    send(&mut client, &ClientMsg::Status).await;
    let tasks = loop {
        if let ServerMsg::Status { tasks, .. } = recv(&mut client).await {
            break tasks;
        }
    };
    let modes = |id| tasks.iter().find(|t| t.id == id).unwrap().routine_mode;
    assert_eq!(modes(task.id), Some(RoutineMode::Live));

    assert_eq!(
        post_json(&base, "/api/routines/fix/dry-run", "s", "")
            .await
            .0,
        204
    );
    assert_eq!(mode("fix").await, Some(RoutineMode::DryRun));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn going_live_approves_the_file_as_read_and_an_edit_ends_it() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-pin-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = |policy: &str| {
        format!(
            "---\nevery: 1h\npaused: true\nact: true\nmerge:\n  repos: [o/r]\n  authors: [me]\n  \
             max_files: 5\n  max_lines: 50\n  approve: {policy}\n---\nMerge green PRs.\n"
        )
    };
    std::fs::write(dir.join("deps.md"), file("phone")).unwrap();
    let every = std::time::Duration::from_secs(3600);
    let base = start_app(with_read_token().with_routines(dir.clone(), every)).await;
    let view = || {
        let base = base.clone();
        async move {
            let (_, body) = get_json(&base, "/api/routines", "r").await;
            let list: Vec<RoutineView> = serde_json::from_str(&body).unwrap();
            list.into_iter().next().unwrap()
        }
    };
    let seen = view().await;
    let m = seen.merge.clone().unwrap();
    assert_eq!(
        (m.repos, m.approve),
        (vec!["o/r".to_string()], MergeApprove::Phone)
    );
    // Edited after the page read it: going live with what the page saw is refused.
    std::fs::write(dir.join("deps.md"), file("none")).unwrap();
    let live = "/api/routines/deps/live";
    let pin = |p: &str| serde_json::json!({ "pin": p }).to_string();
    let (status, body) = post_json(&base, live, "s", &pin(seen.pin.as_deref().unwrap())).await;
    assert_eq!(
        (status, body.contains("changed since you read it")),
        (409, true),
        "{body}"
    );
    assert_eq!(view().await.mode, Some(RoutineMode::DryRun));
    let now = view().await;
    assert_eq!(
        post_json(&base, live, "s", &pin(now.pin.as_deref().unwrap()))
            .await
            .0,
        204
    );
    assert_eq!(view().await.mode, Some(RoutineMode::Live));
    // Edited while live: back to dry run, marked changed until it goes live again.
    std::fs::write(dir.join("deps.md"), file("phone")).unwrap();
    let v = view().await;
    assert_eq!((v.mode, v.changed), (Some(RoutineMode::DryRun), true));
    assert_eq!(v.prompt.as_deref(), Some("Merge green PRs."));
    assert_eq!(post_json(&base, live, "s", "").await.0, 400, "no pin");
    assert_eq!(
        post_json(&base, live, "s", &pin(v.pin.as_deref().unwrap()))
            .await
            .0,
        204
    );
    let v = view().await;
    assert_eq!((v.mode, v.changed), (Some(RoutineMode::Live), false));
    std::fs::remove_dir_all(dir).unwrap();
}

/// The next sessions push that changes `seen`. A watcher may get its view twice: the
/// liveness tick's diff can land after a `WatchSessions` snapshot it doesn't know of.
async fn next_sessions(ws: &mut Ws, seen: &[SessionRow]) -> Vec<SessionRow> {
    loop {
        let ServerMsg::Sessions { sessions } = recv(ws).await else {
            panic!("pushed")
        };
        if sessions != seen {
            return sessions;
        }
    }
}

#[tokio::test]
async fn live_sessions_need_full_access_and_outlast_their_worker() {
    let base = start_app(with_read_token()).await;
    let wid = WorkerId::new_v4();
    let mut req = format!("{base}/ws/worker").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer s".parse().unwrap());
    let (mut worker, _) = connect_async(req).await.unwrap();
    let hello = WorkerMsg::Hello {
        id: wid,
        info: info("claude"),
        running: vec![],
    };
    send(&mut worker, &hello).await;
    let OrchMsg::Welcome { .. } = recv(&mut worker).await else {
        panic!("welcome")
    };
    // A full client watches; a read-only one may not.
    let (mut full, _) = connect_async(client_req(&base, &[("authorization", "Bearer s")]))
        .await
        .unwrap();
    send(&mut full, &ClientMsg::WatchSessions).await;
    let ServerMsg::Sessions { sessions } = recv(&mut full).await else {
        panic!("snapshot")
    };
    assert!(sessions.is_empty());
    let (mut read, _) = connect_async(client_req(&base, &[("authorization", "Bearer r")]))
        .await
        .unwrap();
    send(&mut read, &ClientMsg::Subscribe).await;
    send(&mut read, &ClientMsg::WatchSessions).await;
    let ServerMsg::Error { message } = recv(&mut read).await else {
        panic!("refused")
    };
    assert_eq!(message, "read-only token");

    let s = LiveSession {
        harness: "claude".into(),
        id: "s-1".into(),
        title: Some("Tune jump physics".into()),
        state: LiveState::Waiting,
        waiting_for: Some("permission prompt".into()),
        // Just written: an offline worker's sessions are kept for its live window.
        updated_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        ..Default::default()
    };
    let report = WorkerMsg::LiveSessions {
        sessions: vec![s.clone()],
    };
    send(&mut worker, &report).await;
    let sessions = next_sessions(&mut full, &sessions).await;
    assert_eq!((&sessions[0].session, sessions[0].online), (&s, true));

    assert_eq!(get_json(&base, "/api/sessions", "r").await.0, 403);
    assert_eq!(get_json(&base, "/api/sessions", "nope").await.0, 401);
    for old in [
        format!("/api/workers/{wid}/sessions"),
        format!(
            "/api/workers/{wid}/sessions/claude/{}",
            uuid::Uuid::new_v4()
        ),
    ] {
        assert_eq!(get_json(&base, &old, "r").await.0, 403, "{old}");
    }
    // Gone: its sessions stay, offline, and watchers hear of it.
    drop(worker);
    let sessions = next_sessions(&mut full, &sessions).await;
    assert!(!sessions[0].online);
    let (status, body) = get_json(&base, "/api/sessions", "s").await;
    assert_eq!(status, 200, "{body}");
    let rows: Vec<SessionRow> = serde_json::from_str(&body).unwrap();
    assert_eq!(rows, sessions);
    assert_eq!((rows[0].worker, rows[0].worker_name.as_str()), (wid, "w"));
    // The read-only client never saw a session.
    send(&mut read, &ClientMsg::Status).await;
    let ServerMsg::Status { .. } = recv(&mut read).await else {
        panic!("only status")
    };
}

/// Parts of a capture form: `(name, file name, bytes)`.
type Parts<'a> = [(&'a str, Option<&'a str>, &'a [u8])];

/// A `multipart/form-data` body and its content type.
fn form(parts: &Parts) -> (String, Vec<u8>) {
    let boundary = "mis-test-boundary";
    let mut body = Vec::new();
    for (name, file, bytes) in parts {
        let file = file
            .map(|f| format!("; filename=\"{f}\""))
            .unwrap_or_default();
        let head = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"{file}");
        body.extend(format!("{head}\r\n\r\n").bytes());
        body.extend_from_slice(bytes);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// [`http`] with header pairs and an optional `(content type, body)`; status and body.
async fn call(
    base: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<(String, Vec<u8>)>,
) -> (u16, String) {
    let mut head = format!("{method} {path} HTTP/1.1");
    let (content_type, bytes) = body.unwrap_or_default();
    let typed = (!content_type.is_empty()).then_some(("content-type", content_type.as_str()));
    for (k, v) in headers.iter().copied().chain(typed) {
        head += &format!("\r\n{k}: {v}");
    }
    let (status, _, body) = http(base, &head, &bytes).await;
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn capture(base: &str, headers: &[(&str, &str)], parts: &Parts<'_>) -> (u16, String) {
    call(base, "POST", "/api/capture", headers, Some(form(parts))).await
}

/// Full token `s`, read token `r`, capture token `c`, inbox under `dir`.
fn capturing(dir: &std::path::Path) -> App {
    let inbox = mis_orchestrator::capture::Inbox::open(dir.join("captures/inbox")).unwrap();
    App::new(Some("s".into()))
        .with_read_tokens(vec!["r".into()])
        .with_capture_tokens(vec!["c".into()])
        .with_captures(inbox)
}

/// Every authed route but `/api/capture` refuses these credentials: HTTP 401/403,
/// `/api/info` reports no scope, and both WebSockets refuse to upgrade.
async fn refused_everywhere_else(base: &str, headers: &[(&'static str, &str)]) {
    let id = uuid::Uuid::new_v4();
    let routes = [
        ("POST", "/mcp".to_string()),
        ("POST", "/api/upload".into()),
        ("GET", format!("/api/images/{id}")),
        ("GET", format!("/api/evidence/{id}")),
        ("GET", "/api/update".into()),
        ("POST", "/api/update".into()),
        ("POST", format!("/api/tasks/{id}/cancel")),
        ("GET", format!("/api/tasks/{id}/diff")),
        ("GET", "/api/routines".into()),
        ("POST", "/api/routines/r/run".into()),
        ("GET", "/api/notes/sync".into()),
        ("GET", "/api/notes/tree".into()),
        ("GET", "/api/notes/file?path=inbox/a.md".into()),
        ("GET", "/api/projects".into()),
        ("GET", "/api/apps".into()),
        ("POST", "/api/apps/a/releases".into()),
        ("POST", "/api/apps/a/activate".into()),
        ("POST", "/api/apps/a/members".into()),
        ("GET", "/api/apps/a/backups".into()),
        ("POST", "/api/apps/a/backups".into()),
        ("POST", "/api/apps/a/restore".into()),
        ("GET", "/api/sessions".into()),
        ("GET", format!("/api/workers/{id}/sessions")),
        ("GET", format!("/api/workers/{id}/sessions/claude/{id}")),
        (
            "POST",
            format!("/api/workers/{id}/sessions/claude/{id}/messages"),
        ),
    ];
    for (method, path) in routes {
        let (status, body) = call(base, method, &path, headers, None).await;
        assert!(
            matches!(status, 401 | 403),
            "{method} {path}: {status} {body}"
        );
    }
    let (_, info) = call(base, "GET", "/api/info", headers, None).await;
    assert!(info.contains(r#""scope":null"#), "{info}");
    assert_eq!(client_status(base, headers).await, 401);
    let mut worker = format!("{base}/ws/worker").into_client_request().unwrap();
    for (k, v) in headers {
        worker.headers_mut().insert(*k, v.parse().unwrap());
    }
    let err = connect_async(worker).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(resp) = err else {
        panic!("{err}")
    };
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn capture_is_kept_once_and_its_token_opens_nothing_else() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-capture-{}", uuid::Uuid::new_v4()));
    let base = start_app(capturing(&dir)).await;
    let bearer = ("authorization", "Bearer c");
    let jpeg: &[u8] = b"\xFF\xD8\xFF\xE0 photo";
    let parts: [(&str, Option<&str>, &[u8]); 3] = [
        ("url", None, b"https://example.com/liminal"),
        ("text", None, b"liminal inspiration for amf #amf"),
        ("image", Some("IMG_0001.jpg"), jpeg),
    ];
    let (status, body) = capture(&base, &[bearer], &parts).await;
    assert_eq!(status, 201, "{body}");
    let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
    let file = saved["file"].as_str().unwrap();
    assert!(file.ends_with("-liminal-inspiration-for-amf.md"), "{file}");
    let inbox = dir.join("captures/inbox");
    let md = std::fs::read_to_string(inbox.join(file)).unwrap();
    assert!(md.contains("project: \"amf\"\n"), "{md}");
    assert!(
        md.ends_with("---\nliminal inspiration for amf #amf"),
        "{md}"
    );
    let image = file.replace(".md", "-1.jpg");
    assert_eq!(std::fs::read(inbox.join(&image)).unwrap(), jpeg);
    // A resend, also after a restart, gets the first capture back.
    let (status, again) = capture(&base, &[bearer], &parts).await;
    assert_eq!((status, again.as_str()), (200, body.as_str()));
    let restarted = start_app(capturing(&dir)).await;
    let (status, again) = capture(&restarted, &[bearer], &parts).await;
    assert_eq!((status, again.as_str()), (200, body.as_str()));
    assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 2);
    // Refused: SVG, no content, unknown fields, read or no token. Full access may capture.
    let svg: [(&str, Option<&str>, &[u8]); 1] = [("image", Some("x.svg"), b"<svg xmlns=")];
    assert_eq!(capture(&base, &[bearer], &svg).await.0, 400);
    // What a Shortcut sends when its image step is skipped: kept, placeholder left out.
    let placeholder: [(&str, Option<&str>, &[u8]); 2] = [
        ("shared", None, b"https://example.com/p"),
        ("image", Some("URL.txt"), b"URL"),
    ];
    let (status, body) = capture(&base, &[bearer], &placeholder).await;
    assert_eq!(status, 201, "{body}");
    let via: [(&str, Option<&str>, &[u8]); 1] = [("via", None, b"shortcut")];
    assert_eq!(capture(&base, &[bearer], &via).await.0, 400);
    let prompt: [(&str, Option<&str>, &[u8]); 1] = [("prompt", None, b"rm -rf")];
    assert_eq!(capture(&base, &[bearer], &prompt).await.0, 400);
    let text: [(&str, Option<&str>, &[u8]); 1] = [("text", None, b"another")];
    let read = ("authorization", "Bearer r");
    assert_eq!(capture(&base, &[read], &text).await.0, 401);
    assert_eq!(capture(&base, &[], &text).await.0, 401);
    let full = ("authorization", "Bearer s");
    assert_eq!(capture(&base, &[full], &text).await.0, 201);
    refused_everywhere_else(&base, &[bearer]).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_capture_is_published_to_the_notes_repo_soon_after() {
    use std::time::Duration;
    let dir = std::env::temp_dir().join(format!("mis-e2e-notes-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let human = [
            "-c",
            "user.name=Human",
            "-c",
            "user.email=human@example.com",
        ];
        let out = std::process::Command::new("git")
            .current_dir(&dir)
            .args(human)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?}: {err}");
        String::from_utf8(out.stdout).unwrap()
    };
    // A bare remote with one commit, as the real repo has.
    git(&["init", "--quiet", "--bare", "-b", "main", "remote.git"]);
    git(&["clone", "--quiet", "remote.git", "human"]);
    std::fs::write(dir.join("human/README.md"), "# notes\n").unwrap();
    git(&["-C", "human", "add", "README.md"]);
    git(&["-C", "human", "commit", "--quiet", "-m", "init"]);
    git(&["-C", "human", "push", "--quiet", "origin", "HEAD:main"]);
    let state = dir.join("state");
    let notes =
        mis_orchestrator::notes::Notes::new(dir.join("remote.git").display().to_string(), &state)
            .with_timing(Duration::from_secs(1), Duration::from_secs(3600));
    let base = start_app(capturing(&state).with_notes(notes)).await;
    let sync = || async {
        let (status, body) = get_json(&base, "/api/notes/sync", "r").await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<NotesSync>(&body).unwrap()
    };
    let text: [(&str, Option<&str>, &[u8]); 1] = [("text", None, b"publish me #amf")];
    let (status, body) = capture(&base, &[("authorization", "Bearer c")], &text).await;
    assert_eq!(status, 201, "{body}");
    let file = serde_json::from_str::<serde_json::Value>(&body).unwrap()["file"]
        .as_str()
        .unwrap()
        .to_string();
    let s = sync().await;
    assert_eq!(
        (s.state, s.pending),
        (SyncState::Pending, 1),
        "waits a moment"
    );
    let mut s = s;
    // A capture kept mid-run may be counted twice until the next run: wait for that too.
    for _ in 0..300 {
        if s.last_push.is_some() && s.state == SyncState::Idle {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        s = sync().await;
    }
    assert_eq!(
        (s.state, s.pending, &s.last_error),
        (SyncState::Idle, 0, &None)
    );
    assert!(s.last_push.is_some());
    let log = git(&[
        "--git-dir",
        "remote.git",
        "log",
        "--format=%s",
        "--name-only",
        "main",
    ]);
    assert_eq!(
        log,
        format!("capture: 1 item\n\ninbox/{file}\ninit\n\nREADME.md\n")
    );
    // The capture token can't see it; without a remote it is off.
    let (status, _) = get_json(&base, "/api/notes/sync", "c").await;
    assert!(matches!(status, 401 | 403), "{status}");
    let off = start_app(capturing(&dir.join("off"))).await;
    let (status, body) = get_json(&off, "/api/notes/sync", "r").await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<NotesSync>(&body).unwrap(),
        NotesSync::OFF
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_notes_view_reads_the_clone_for_full_scope_only() {
    use std::time::Duration;
    let dir = std::env::temp_dir().join(format!("mis-e2e-notes-view-{}", uuid::Uuid::new_v4()));
    let human = dir.join("human");
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&dir)
            .args([
                "-c",
                "user.name=Human",
                "-c",
                "user.email=human@example.com",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?}: {err}");
    };
    git(&["init", "--quiet", "--bare", "-b", "main", "remote.git"]);
    git(&["clone", "--quiet", "remote.git", "human"]);
    // No line-ending conversion on checkout (Windows runners set core.autocrlf), so sizes
    // and bytes are as written here.
    let files: [(&str, &[u8]); 8] = [
        (".gitattributes", b"* -text\n"),
        ("README.md", b"# notes\n"),
        (
            "inbox/2026-11-08-a.md",
            b"---\nvia: \"shortcut\"\n---\nhello\n",
        ),
        ("inbox/2026-11-08-a-1.jpg", b"\xFF\xD8\xFF\xE0 photo"),
        ("inbox/fake.png", b"<html><script>alert(1)</script>"),
        ("inbox/logo.svg", b"<svg onload=alert(1)>"),
        ("projects/amf/README.md", b"# All Must Fall\n"),
        ("apps/todo/index.html", b"<h1>app</h1>"),
    ];
    for (path, bytes) in files {
        let at = human.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, bytes).unwrap();
    }
    git(&["-C", "human", "add", "-A"]);
    git(&["-C", "human", "commit", "--quiet", "-m", "init"]);
    git(&["-C", "human", "push", "--quiet", "origin", "HEAD:main"]);
    let state = dir.join("state");
    let notes =
        mis_orchestrator::notes::Notes::new(dir.join("remote.git").display().to_string(), &state)
            .with_timing(Duration::from_secs(3600), Duration::from_secs(3600));
    let base = start_app(capturing(&state).with_notes(notes)).await;
    // The first run, at start, clones it.
    let mut tree = None;
    for _ in 0..100 {
        let (status, body) = get_json(&base, "/api/notes/tree", "s").await;
        assert_eq!(status, 200, "{body}");
        let t: NotesTree = serde_json::from_str(&body).unwrap();
        if !t.entries.is_empty() {
            tree = Some(t);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let tree = tree.expect("cloned");
    assert_eq!(tree.state, SyncState::Idle);
    let got: Vec<(&str, NoteKind, u64)> = (tree.entries.iter())
        .map(|e| (e.path.as_str(), e.kind, e.size))
        .collect();
    assert_eq!(
        got,
        [
            ("inbox", NoteKind::Dir, 0),
            ("inbox/2026-11-08-a-1.jpg", NoteKind::Image, 10),
            ("inbox/2026-11-08-a.md", NoteKind::Md, 30),
            ("inbox/fake.png", NoteKind::Image, 31),
            ("inbox/logo.svg", NoteKind::Other, 21),
            ("projects", NoteKind::Dir, 0),
            ("projects/amf", NoteKind::Dir, 0),
            ("projects/amf/README.md", NoteKind::Md, 16),
        ]
    );
    let file = |path: &str, token: &str| {
        let head =
            format!("GET /api/notes/file?path={path} HTTP/1.1\r\nAuthorization: Bearer {token}");
        let base = base.clone();
        async move { http(&base, &head, b"").await }
    };
    let (status, head, body) = file("inbox/2026-11-08-a.md", "s").await;
    assert_eq!(status, 200, "{head}");
    assert!(
        head.contains("content-type: text/markdown; charset=utf-8"),
        "{head}"
    );
    assert!(head.contains("x-content-type-options: nosniff"), "{head}");
    assert_eq!(body, files[2].1);
    let (status, head, body) = file("inbox/2026-11-08-a-1.jpg", "s").await;
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("content-type: image/jpeg"), "{head}");
    assert_eq!(body, files[3].1);
    // HTML named .png, and SVG, are never served.
    assert_eq!(file("inbox/fake.png", "s").await.0, 415);
    assert_eq!(file("inbox/logo.svg", "s").await.0, 415);
    for path in [
        "README.md",
        "apps/todo/index.html",
        "inbox/../README.md",
        "inbox%2F..%2FREADME.md",
        "/etc/hosts",
        ".git/config",
        "inbox",
        "inbox/missing.md",
        "",
    ] {
        assert_eq!(file(path, "s").await.0, 404, "{path}");
    }
    // A read token may not; a capture token reaches nothing but capturing.
    for token in ["r", "c"] {
        let (status, _) = get_json(&base, "/api/notes/tree", token).await;
        assert!(matches!(status, 401 | 403), "{token}: {status}");
        let status = file("inbox/2026-11-08-a.md", token).await.0;
        assert!(matches!(status, 401 | 403), "{token}: {status}");
    }
    // Without a remote: off.
    let off = start_app(capturing(&dir.join("off"))).await;
    let (status, body) = get_json(&off, "/api/notes/tree", "s").await;
    assert_eq!(status, 200);
    let want = NotesTree {
        state: SyncState::Off,
        entries: vec![],
    };
    assert_eq!(serde_json::from_str::<NotesTree>(&body).unwrap(), want);
    let head = "GET /api/notes/file?path=inbox/a.md HTTP/1.1\r\nAuthorization: Bearer s";
    assert_eq!(http(&off, head, b"").await.0, 404);
    // The first run's `git fetch` may still be in the clone (Windows won't delete it).
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn projects_are_read_from_the_notes_clone_for_full_scope_only() {
    use std::time::Duration;
    let dir = std::env::temp_dir().join(format!("mis-e2e-projects-{}", uuid::Uuid::new_v4()));
    let human = dir.join("human");
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["-c", "user.name=Human", "-c", "user.email=h@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?}: {err}");
    };
    git(&["init", "--quiet", "--bare", "-b", "main", "remote.git"]);
    git(&["clone", "--quiet", "remote.git", "human"]);
    let files = [
        (".gitattributes", "* -text\n"),
        (
            "projects/make-it-so/README.md",
            "---\nschema: 1\nid: make-it-so\ntitle: Make It So\naliases: [mis]\nstatus: active\n\
             repos: [tommy-xr/MakeItSo]\nnotes: [~/notes/projects/make-it-so/]\n---\nShort.\n",
        ),
        ("projects/make-it-so/plan.md", "# plan\n"),
        (
            "projects/amf/README.md",
            "---\ntitle: All Must Fall\naliases: [Make-It-So, amf2]\n---\n",
        ),
        ("projects/broken/README.md", "no frontmatter\n"),
        (
            "archive/old/README.md",
            "---\ntitle: Old\nstatus: done\n---\nDone.\n",
        ),
    ];
    for (path, text) in files {
        let at = human.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, text).unwrap();
    }
    git(&["-C", "human", "add", "-A"]);
    git(&["-C", "human", "commit", "--quiet", "-m", "init"]);
    git(&["-C", "human", "push", "--quiet", "origin", "HEAD:main"]);
    let state = dir.join("state");
    let notes =
        mis_orchestrator::notes::Notes::new(dir.join("remote.git").display().to_string(), &state)
            .with_timing(Duration::from_secs(3600), Duration::from_secs(3600));
    let base = start_app(capturing(&state).with_notes(notes)).await;
    // The first run, at start, clones it.
    let mut got = None;
    for _ in 0..100 {
        let (status, body) = get_json(&base, "/api/projects", "s").await;
        assert_eq!(status, 200, "{body}");
        let p: Projects = serde_json::from_str(&body).unwrap();
        if !p.projects.is_empty() {
            got = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let got = got.expect("cloned");
    assert_eq!(got.state, SyncState::Idle);
    let names: Vec<(&str, &str, &[String], bool)> = (got.projects.iter())
        .map(|p| (p.id.as_str(), p.title.as_str(), &p.aliases[..], p.archived))
        .collect();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let (mis, amf2) = (s(&["mis"]), s(&["amf2"]));
    assert_eq!(
        names,
        [
            ("amf", "All Must Fall", &amf2[..], false),
            ("make-it-so", "Make It So", &mis[..], false),
            ("old", "Old", &[][..], true),
        ]
    );
    let mis = &got.projects[1];
    assert_eq!(mis.repos, ["tommy-xr/MakeItSo"]);
    assert_eq!(mis.notes, ["~/notes/projects/make-it-so/"]);
    assert_eq!(mis.description, "Short.");
    // Listed, not fatal: a malformed record, and an alias that is another's id.
    let errors: Vec<(&str, &str)> = (got.errors.iter())
        .map(|e| (e.path.as_str(), e.error.as_str()))
        .collect();
    assert_eq!(
        errors,
        [
            (
                "projects/broken/README.md",
                "must start with a `---` frontmatter line"
            ),
            (
                "projects/amf/README.md",
                "alias `Make-It-So` is taken by projects/make-it-so/README.md"
            ),
        ]
    );
    // A read token may not; a capture token reaches nothing but capturing.
    for token in ["r", "c"] {
        let (status, _) = get_json(&base, "/api/projects", token).await;
        assert!(matches!(status, 401 | 403), "{token}: {status}");
    }
    // Without a remote: off.
    let off = start_app(capturing(&dir.join("off"))).await;
    let (status, body) = get_json(&off, "/api/projects", "s").await;
    assert_eq!(status, 200);
    let want = Projects {
        state: SyncState::Off,
        projects: vec![],
        errors: vec![],
    };
    assert_eq!(serde_json::from_str::<Projects>(&body).unwrap(), want);
    // The running app may still hold files in it (Windows refuses to delete them).
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn capture_takes_the_url_encoded_form_shortcuts_send_without_a_file() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-capture-{}", uuid::Uuid::new_v4()));
    let base = start_app(capturing(&dir)).await;
    let bearer = [("authorization", "Bearer c")];
    let post = |body: &str| {
        let body = (
            "application/x-www-form-urlencoded".to_string(),
            body.as_bytes().to_vec(),
        );
        call(&base, "POST", "/api/capture", &bearer, Some(body))
    };
    // Byte for byte what iOS Shortcuts sent for a shared page with a note.
    let (status, body) =
        post("url=https%3A%2F%2Fexample.com%2Fpage&text=debug+note+%23amf&via=shortcut").await;
    assert_eq!(status, 201, "{body}");
    let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
    let md = std::fs::read_to_string(
        dir.join("captures/inbox")
            .join(saved["file"].as_str().unwrap()),
    )
    .unwrap();
    assert!(md.contains("url: \"https://example.com/page\"\n"), "{md}");
    assert!(md.ends_with("---\ndebug note #amf"), "{md}");
    // A Chrome share: Shortcuts can't type the link, so it comes as the shared item's text.
    let (status, body) =
        post("shared=https%3A%2F%2Fexample.com%2Fnews%2Fa&text=&via=shortcut").await;
    assert_eq!(status, 201, "{body}");
    let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
    let md = std::fs::read_to_string(
        dir.join("captures/inbox")
            .join(saved["file"].as_str().unwrap()),
    )
    .unwrap();
    assert!(
        md.contains("url: \"https://example.com/news/a\"\n") && !md.contains("shared:"),
        "{md}"
    );
    // An empty file field comes through as `image=`; a value there isn't a file.
    assert_eq!(post("text=two&image=").await.0, 201);
    assert_eq!(post("text=three&image=aGk%3D").await.0, 400);
    assert_eq!(post("text=a&text=b").await.0, 400);
    assert_eq!(post("prompt=rm").await.0, 400);
    assert_eq!(post("url=&text=&via=shortcut").await.0, 400);
    let read = [("authorization", "Bearer r")];
    let body = (
        "application/x-www-form-urlencoded".to_string(),
        b"text=x".to_vec(),
    );
    assert_eq!(
        call(&base, "POST", "/api/capture", &read, Some(body))
            .await
            .0,
        401
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn capture_takes_a_raw_email() {
    use sha2::Digest;
    let dir = std::env::temp_dir().join(format!("mis-e2e-capture-{}", uuid::Uuid::new_v4()));
    let base = start_app(capturing(&dir)).await;
    let inbox = dir.join("captures/inbox");
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/email");
    let post = |headers: &'static [(&'static str, &'static str)], raw: Vec<u8>| {
        let body = Some(("message/rfc822".to_string(), raw));
        call(&base, "POST", "/api/capture", headers, body)
    };
    let bearer: &[(&str, &str)] = &[("authorization", "Bearer c")];
    // Synthetic fixtures (see make.py): url, note, shared text (the body), project, kept
    // images as (extension, first 16 hex of sha256, from Python's own decoding).
    type Row<'a> = (
        &'a str,
        Option<&'a str>,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        &'a [(&'a str, &'a str)],
    );
    // iOS bodies hold only the link or "Sent from my iPhone": nothing left as shared text.
    let iphone = None;
    let rows: [Row; 6] = [
        (
            "a-ios-safari-link",
            Some("https://example.com/articles/liminal-spaces"),
            "Liminal spaces: empty malls at night",
            iphone,
            None,
            &[],
        ),
        ("b-ios-photo", None, "", iphone, None, &[("jpg", "8c5d478a22f67f64")]),
        (
            "c-ios-five-photos",
            None,
            "Kitchen ideas #home",
            iphone,
            Some("home"),
            &[
                ("jpg", "e86a831e769f3f95"),
                ("jpg", "1065ba3c53df2ba4"),
                ("jpg", "966ba070f120d9d1"),
                ("jpg", "9031bdff68ecd061"),
            ],
        ),
        (
            "d-outlook-html",
            Some("https://example.com/news/great-article?utm=mail&id=7"),
            "FW: Great article #amf",
            Some("Worth a read: Great article\nDemo User"),
            Some("amf"),
            &[("png", "ae4c625d0e4302fb")],
        ),
        (
            "e-utf8-qp",
            Some("https://example.com/places/cafe-with-a-rather-long-name-that-wraps?ref=notes&lang=en"),
            "\u{1F4CC} Café notes #mis",
            Some("Notes from the café — try the cortado.\nhttps://example.com/places/cafe-with-a-rather-long-name-that-wraps?ref=notes&lang=en"),
            Some("mis"),
            &[],
        ),
        ("f-ios-heic", None, "Sunset", iphone, None, &[]),
    ];
    for (name, url, text, shared, project, assets) in rows {
        let raw = std::fs::read(fixtures.join(format!("{name}.eml"))).unwrap();
        let (status, body) = post(bearer, raw).await;
        assert_eq!(status, 201, "{name}: {body}");
        let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
        let file = saved["file"].as_str().unwrap();
        let md = std::fs::read_to_string(inbox.join(file)).unwrap();
        assert!(md.contains("via: \"email\"\n"), "{name}: {md}");
        let line = |key: &str, v: Option<&str>| v.map(|v| format!("{key}: {v:?}\n"));
        for (key, want) in [("url", url), ("shared", shared), ("project", project)] {
            match line(key, want) {
                Some(l) => assert!(md.contains(&l), "{name}: {md}"),
                None => assert!(!md.contains(&format!("\n{key}: ")), "{name}: {md}"),
            }
        }
        assert!(md.ends_with(&format!("---\n{text}")), "{name}: {md}");
        let stem = file.trim_end_matches(".md");
        for (i, (ext, sha)) in assets.iter().enumerate() {
            let bytes = std::fs::read(inbox.join(format!("{stem}-{}.{ext}", i + 1))).unwrap();
            let hex: String = (sha2::Sha256::digest(&bytes).iter())
                .map(|b| format!("{b:02x}"))
                .collect();
            assert!(hex.starts_with(sha), "{name} image {}", i + 1);
        }
        let kept = std::fs::read_dir(&inbox).unwrap().flatten();
        let names: Vec<_> = kept
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&format!("{stem}-")) || n == file)
            .collect();
        assert_eq!(names.len(), 1 + assets.len(), "{name}: {names:?}");
    }
    // Refused: no capture token, or nothing to keep.
    let raw = std::fs::read(fixtures.join("a-ios-safari-link.eml")).unwrap();
    assert_eq!(post(&[("authorization", "Bearer r")], raw).await.0, 401);
    let (status, body) = post(bearer, b"Subject: \r\n\r\n\r\n".to_vec()).await;
    assert_eq!((status, body.as_str()), (400, "send a url, text or image"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn capture_app_jwt_opens_only_the_capture_route() {
    let dir = std::env::temp_dir().join(format!("mis-e2e-capture-{}", uuid::Uuid::new_v4()));
    let jwks = serde_json::from_str(include_str!("fixtures/access-test-jwks.json")).unwrap();
    let gate = mis_orchestrator::access_gate::AccessGate::with_keys(
        "team".into(),
        ACCESS_AUD.into(),
        jwks,
    )
    .with_capture_aud("capture-aud".into());
    let base = start_app(capturing(&dir).with_access(gate)).await;
    let jwt = access_jwt("capture-aud");
    let tunnel = [
        ("cf-ray", "8f00-SJC"),
        ("cf-access-jwt-assertion", jwt.as_str()),
    ];
    let with = |token| [tunnel[0], tunnel[1], ("authorization", token)];
    let text: [(&str, Option<&str>, &[u8]); 1] = [("text", None, b"from the phone")];
    assert_eq!(capture(&base, &with("Bearer c"), &text).await.0, 201);
    // The JWT alone can't capture, and opens nothing else, whatever token rides along.
    assert_eq!(capture(&base, &tunnel, &text).await.0, 401);
    refused_everywhere_else(&base, &tunnel).await;
    for token in ["Bearer c", "Bearer s", "Bearer r"] {
        refused_everywhere_else(&base, &with(token)).await;
    }
    // A main-app login still opens everything, but not for a capture token.
    let main = access_jwt(ACCESS_AUD);
    let login = [
        ("cf-ray", "8f00-SJC"),
        ("cf-access-jwt-assertion", main.as_str()),
    ];
    assert_eq!(client_status(&base, &login).await, 101);
    let login_with_capture = [login[0], login[1], ("authorization", "Bearer c")];
    refused_everywhere_else(&base, &login_with_capture).await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// The orchestrator with apps kept in a fresh temp dir, and its apps listener: their bases
/// (`ws://` like the rest) and the dir.
async fn start_with_apps(app: App) -> (String, String, std::path::PathBuf) {
    start_with_apps_auth(app, Default::default()).await
}

/// [`start_with_apps`] with the apps listener checking `auth`.
async fn start_with_apps_auth(
    app: App,
    auth: mis_orchestrator::apps_http::Auth,
) -> (String, String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("mis-e2e-apps-{}", uuid::Uuid::new_v4()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = mis_orchestrator::apps::base_url(None, Some(addr));
    let apps = std::sync::Arc::new(mis_orchestrator::apps::Apps::new(dir.clone(), base));
    tokio::spawn(mis_orchestrator::serve_apps(listener, apps.clone(), auth));
    let mis = start_app(app.with_apps(apps)).await;
    (mis, format!("ws://{addr}"), dir)
}

/// A `.tar.gz` of `(path, bytes)` files, paths written raw (so `../x` gets through).
fn bundle(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut h = tar::Header::new_old();
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, *bytes).unwrap();
    }
    gzip(&b.into_inner().unwrap())
}

fn gzip(tar: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(tar).unwrap();
    e.finish().unwrap()
}

/// A demo app whose page says `page`.
fn demo(page: &str) -> Vec<u8> {
    bundle(&[
        (
            "app.json",
            br#"{"title":"Demo","clients":{"web":"index.html"}}"#,
        ),
        ("index.html", page.as_bytes()),
        ("js/app.js", b"console.log(1)"),
    ])
}

async fn publish(base: &str, path: &str, auth: &str, tar_gz: &[u8]) -> (u16, String) {
    let head = format!("POST {path} HTTP/1.1\r\nAuthorization: Bearer {auth}");
    let (status, _, body) = http(base, &head, tar_gz).await;
    (status, String::from_utf8(body).unwrap())
}

/// GET `path` from the apps listener: status, head, body.
async fn app_get(apps: &str, path: &str, extra: &str) -> (u16, String, String) {
    let head = format!("GET {path} HTTP/1.1{extra}");
    let (status, head, body) = http(apps, &head, b"").await;
    (status, head, String::from_utf8(body).unwrap())
}

#[tokio::test]
async fn apps_publish_activate_and_serve() {
    let (mis, apps, dir) = start_with_apps(with_read_token()).await;
    let (status, body) = publish(&mis, "/api/apps/demo/releases", "s", &demo("v1")).await;
    assert_eq!((status, body.as_str()), (201, r#"{"release":1}"#));
    // Staged, not live.
    assert_eq!(app_get(&apps, "/a/demo/", "").await.0, 404);
    let activate = |n: u32| format!(r#"{{"release":{n}}}"#);
    let (status, _) = post_json(&mis, "/api/apps/demo/activate", "s", &activate(1)).await;
    assert_eq!(status, 204);
    let (status, head, body) = app_get(&apps, "/a/demo/", "").await;
    assert_eq!((status, body.as_str()), (200, "v1"));
    let head = head.to_ascii_lowercase();
    assert!(
        head.contains("content-type: text/html; charset=utf-8"),
        "{head}"
    );
    let csp = format!(
        "content-security-policy: {}",
        mis_orchestrator::apps_http::CSP
    );
    assert!(head.contains(&csp), "{head}");
    assert!(head.contains("x-content-type-options: nosniff"), "{head}");
    assert!(head.contains("cache-control: no-cache"), "{head}");
    let (status, head, body) = app_get(&apps, "/a/demo/js/app.js", "").await;
    assert_eq!((status, body.as_str()), (200, "console.log(1)"));
    assert!(head.contains("text/javascript"), "{head}");
    // An SPA route gets the entry; a missing file doesn't.
    assert_eq!(app_get(&apps, "/a/demo/scores/2026", "").await.2, "v1");
    assert_eq!(app_get(&apps, "/a/demo/missing.png", "").await.0, 404);
    assert_eq!(app_get(&apps, "/a/demo/.release.json", "").await.0, 404);
    // `..` never leaves the release: an extensionless miss is just the entry.
    assert_eq!(
        app_get(&apps, "/a/demo/%2e%2e/%2e%2e/active", "").await.2,
        "v1"
    );
    let up = "/a/demo/%2e%2e/1/.release.json";
    assert_eq!(app_get(&apps, up, "").await.0, 404);
    let (status, head, _) = app_get(&apps, "/a/demo", "").await;
    assert_eq!(status, 308);
    assert!(head.contains("location: /a/demo/"), "{head}");
    // Published live at once, then reverted.
    let (status, body) = publish(
        &mis,
        "/api/apps/demo/releases?activate=true",
        "s",
        &demo("v2"),
    )
    .await;
    assert_eq!((status, body.as_str()), (201, r#"{"release":2}"#));
    assert_eq!(app_get(&apps, "/a/demo/", "").await.2, "v2");
    post_json(&mis, "/api/apps/demo/activate", "s", &activate(1)).await;
    assert_eq!(app_get(&apps, "/a/demo/", "").await.2, "v1");
    let (status, _) = post_json(&mis, "/api/apps/demo/activate", "s", &activate(9)).await;
    assert_eq!(status, 404);
    let (status, body) = get_json(&mis, "/api/apps", "s").await;
    assert_eq!(status, 200);
    let list: Vec<AppView> = serde_json::from_str(&body).unwrap();
    let a = &list[0];
    assert_eq!(
        (a.slug.as_str(), a.title.as_str(), a.active),
        ("demo", "Demo", Some(1))
    );
    assert_eq!(
        a.url.as_deref(),
        Some(format!("http://{}/a/demo/", &apps[5..]).as_str())
    );
    let ns: Vec<u32> = a.releases.iter().map(|r| r.n).collect();
    assert_eq!(ns, [1, 2]);
    assert_ne!(a.releases[0].sha256, a.releases[1].sha256);
    // The directory links live apps, each with its live release's icon or a letter.
    let pics = bundle(&[
        (
            "app.json",
            br#"{"title":"Pics","icon":"img/my icon.png","clients":{"web":"index.html"}}"#,
        ),
        ("index.html", b"pics"),
        ("img/my icon.png", b"png"),
    ]);
    let live = "/api/apps/pics/releases?activate=true";
    assert_eq!(publish(&mis, live, "s", &pics).await.0, 201);
    let staged = "/api/apps/draft/releases";
    assert_eq!(publish(&mis, staged, "s", &demo("draft")).await.0, 201);
    let (_, _, page) = app_get(&apps, "/a/", "").await;
    assert!(page.contains(r#"<a href="/a/demo/"><span class="icon" aria-hidden="true">D</span><span class="name">Demo</span></a>"#), "{page}");
    let icon = "/a/pics/img/my%20icon.png";
    assert!(
        page.contains(&format!(r#"<img class="icon" src="{icon}" alt="">"#)),
        "{page}"
    );
    assert!(!page.contains("/a/draft/"), "{page}");
    assert_eq!(app_get(&apps, icon, "").await.2, "png");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn apps_refuse_traversal_and_link_archives() {
    let (mis, _, dir) = start_with_apps(App::new(None)).await;
    let app_json: &[u8] = br#"{"title":"Demo","clients":{"web":"index.html"}}"#;
    let page = ("index.html", &b"x"[..]);
    let escape = bundle(&[("app.json", app_json), page, ("../../escaped", b"x")]);
    let (status, body) = publish(&mis, "/api/apps/demo/releases", "", &escape).await;
    assert_eq!(status, 400);
    assert!(body.contains("`..`"), "{body}");
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_path("index.html").unwrap();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_link_name("/etc/passwd").unwrap();
    h.set_size(0);
    h.set_cksum();
    b.append(&h, &[][..]).unwrap();
    let link = gzip(&b.into_inner().unwrap());
    let (status, body) = publish(&mis, "/api/apps/demo/releases", "", &link).await;
    assert_eq!(status, 400);
    assert!(body.contains("Symlink"), "{body}");
    let (status, _) = publish(&mis, "/api/apps/Bad_Name/releases", "", &demo("x")).await;
    assert_eq!(status, 400);
    // Nothing was written, anywhere.
    assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().count() == 0);
    assert!(!dir.parent().unwrap().join("escaped").exists());
}

#[tokio::test]
async fn apps_port_has_no_orchestrator_routes() {
    let (mis, apps, dir) = start_with_apps(App::new(None)).await;
    assert_eq!(get_json(&mis, "/api/info", "").await.0, 200);
    for path in ["/api/info", "/api/apps", "/ws/client", "/ws/worker"] {
        assert_eq!(app_get(&apps, path, "").await.0, 404, "{path}");
    }
    // `/` sends you to the apps directory, never the orchestrator's page.
    assert_eq!(app_get(&apps, "/", "").await.0, 307);
    let (status, _, _) = http(&apps, "POST /mcp HTTP/1.1", b"{}").await;
    assert_eq!(status, 404);
    let (status, _, _) = http(&apps, "POST /api/apps/demo/releases HTTP/1.1", &demo("x")).await;
    assert_eq!(status, 404);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn read_token_lists_apps_but_cannot_publish_or_activate() {
    let (mis, _, dir) = start_with_apps(with_read_token()).await;
    assert_eq!(
        publish(&mis, "/api/apps/demo/releases", "s", &demo("v1"))
            .await
            .0,
        201
    );
    let (status, body) = get_json(&mis, "/api/apps", "r").await;
    assert_eq!(status, 200);
    assert!(body.contains(r#""slug":"demo""#), "{body}");
    assert_eq!(get_json(&mis, "/api/apps", "nope").await.0, 401);
    let (status, body) = publish(&mis, "/api/apps/demo/releases", "r", &demo("v2")).await;
    assert_eq!((status, body.as_str()), (403, "read-only token"));
    let (status, _) = post_json(&mis, "/api/apps/demo/activate", "r", r#"{"release":1}"#).await;
    assert_eq!(status, 403);
    let (_, body) = get_json(&mis, "/api/apps", "r").await;
    let list: Vec<AppView> = serde_json::from_str(&body).unwrap();
    assert_eq!((list[0].active, list[0].releases.len()), (None, 1));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn apps_port_refuses_cloudflare_traffic_without_a_jwt() {
    let (mis, apps, dir) = start_with_apps(App::new(None)).await;
    publish(
        &mis,
        "/api/apps/demo/releases?activate=true",
        "",
        &demo("v1"),
    )
    .await;
    assert_eq!(app_get(&apps, "/a/demo/", "").await.0, 200);
    for h in [
        "Cf-Access-Jwt-Assertion: x",
        "CF-Ray: 1",
        "CF-Connecting-IP: 203.0.113.9",
    ] {
        for path in ["/a/demo/", "/a/", "/nope"] {
            let (status, head, _) = app_get(&apps, path, &format!("\r\n{h}")).await;
            assert_eq!(status, 401, "{h} {path}");
            assert!(head.contains("nosniff"), "{head}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

const APPS_AUD: &str = "apps-aud";
const HOME: &str = "home.example.test";

/// The apps listener's auth: the fixture key's JWTs for [`APPS_AUD`], public host [`HOME`].
fn apps_auth() -> mis_orchestrator::apps_http::Auth {
    let jwks = serde_json::from_str(include_str!("fixtures/access-test-jwks.json")).unwrap();
    let gate =
        mis_orchestrator::access_gate::AccessGate::with_keys("team".into(), APPS_AUD.into(), jwks);
    mis_orchestrator::apps_http::Auth {
        gate: Some(gate),
        public_host: Some(HOME.into()),
        dev: false,
    }
}

/// Request-head lines for a request through the tunnel to [`HOME`], signed in as `jwt`.
fn via_home(jwt: &str) -> String {
    format!("\r\nHost: {HOME}\r\nCF-Ray: 8f00-SJC\r\nCf-Access-Jwt-Assertion: {jwt}")
}

#[tokio::test]
async fn apps_open_to_members_only_through_their_own_access_app() {
    let (mis, apps, dir) = start_with_apps_auth(behind_access(), apps_auth()).await;
    let page = "/api/apps/demo/releases?activate=true";
    assert_eq!(publish(&mis, page, "s", &demo("v1")).await.0, 201);
    let share = r#"{"email":"Me@Example.com","role":"editor"}"#;
    let (status, body) = post_json(&mis, "/api/apps/demo/members", "s", share).await;
    assert_eq!(
        (status, body.as_str()),
        (200, r#"[{"email":"me@example.com","role":"editor"}]"#)
    );
    // A member, through the tunnel.
    let me = via_home(&access_jwt_for(APPS_AUD, "me@example.com"));
    let (status, _, page) = app_get(&apps, "/a/demo/", &me).await;
    assert_eq!((status, page.as_str()), (200, "v1"));
    let (status, _, who) = app_get(&apps, "/a/demo/api/v1/me", &me).await;
    let who: mis_types::AppMe = serde_json::from_str(&who).unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        (who.user.as_str(), who.role, who.app.as_str()),
        ("me@example.com", mis_types::AppRole::Editor, "demo")
    );
    assert!(app_get(&apps, "/a/", &me).await.2.contains("/a/demo/"));
    // Signed in, but not a member: refused, and not listed.
    let other = via_home(&access_jwt_for(APPS_AUD, "other@example.com"));
    for path in [
        "/a/demo/",
        "/a/demo/app.json",
        "/a/demo/api/v1/me",
        "/a/demo",
        "/a/nope/",
    ] {
        assert_eq!(app_get(&apps, path, &other).await.0, 403, "{path}");
    }
    let (status, _, list) = app_get(&apps, "/a/", &other).await;
    assert_eq!(status, 200);
    assert!(!list.contains("demo"), "{list}");
    assert!(list.contains("No apps are shared with you yet."), "{list}");
    // The directory's own manifest and icons are for anyone signed in.
    for (path, ct) in [
        ("/a/_/apps.webmanifest", "application/manifest+json"),
        ("/a/_/apps-icon.svg", "image/svg+xml"),
        ("/a/_/apps-touch-icon.png", "image/png"),
    ] {
        let (status, head, _) = http(&apps, &format!("GET {path} HTTP/1.1{other}"), b"").await;
        assert_eq!(status, 200, "{path}");
        assert!(head.contains(ct), "{head}");
    }
    // The bare hostname lands on the directory.
    for path in ["/", "/a"] {
        let (status, head, _) = app_get(&apps, path, &other).await;
        assert_eq!(status, 307, "{path}");
        assert!(head.to_lowercase().contains("location: /a/"), "{head}");
    }
    // The MIS app's JWT doesn't open apps; the apps JWT doesn't open MIS.
    let mis_jwt = via_home(&access_jwt_for(ACCESS_AUD, "me@example.com"));
    assert_eq!(app_get(&apps, "/a/demo/", &mis_jwt).await.0, 401);
    let apps_jwt = access_jwt_for(APPS_AUD, "me@example.com");
    let tunnel = [
        ("cf-ray", "8f00-SJC"),
        ("cf-access-jwt-assertion", apps_jwt.as_str()),
    ];
    assert_eq!(client_status(&mis, &tunnel).await, 401);
    assert_eq!(call(&mis, "GET", "/api/apps", &tunnel, None).await.0, 401);
    // Another Host (a DNS-rebound name), even with a member's JWT.
    let rebound = me.replace(HOME, "evil.example");
    assert_eq!(app_get(&apps, "/a/demo/", &rebound).await.0, 421);
    // Through the tunnel with no JWT: Access isn't in front, so nobody gets in.
    let bare = format!("\r\nHost: {HOME}\r\nCF-Ray: 8f00-SJC");
    assert_eq!(app_get(&apps, "/a/demo/", &bare).await.0, 401);
    // Direct on loopback, no Cloudflare headers: `local`, owner of every app.
    let (status, _, who) = app_get(&apps, "/a/demo/api/v1/me", "").await;
    assert_eq!(
        (status, who.as_str()),
        (200, r#"{"user":"local","role":"owner","app":"demo"}"#)
    );
    // `/api/` is the server's: no entry-page fallback, GET only for /me.
    for path in [
        "/a/demo/api/v1/nope",
        "/a/demo/api",
        "/a/demo/api/",
        "/a/demo/%61pi/v1/x",
    ] {
        assert_eq!(app_get(&apps, path, "").await.0, 404, "{path}");
    }
    let (status, _, _) = http(&apps, "PUT /a/demo/api/v1/me HTTP/1.1", b"").await;
    assert_eq!(status, 405);
    // Unshared again: refused.
    let unshare = r#"{"email":"me@example.com","role":null}"#;
    assert_eq!(
        post_json(&mis, "/api/apps/demo/members", "s", unshare).await,
        (200, "[]".into())
    );
    assert_eq!(app_get(&apps, "/a/demo/", &me).await.0, 403);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn apps_refuse_lan_requests_without_a_jwt() {
    use axum::extract::connect_info::MockConnectInfo;
    let dir = std::env::temp_dir().join(format!("mis-e2e-apps-{}", uuid::Uuid::new_v4()));
    let apps = std::sync::Arc::new(mis_orchestrator::apps::Apps::new(dir.clone(), None));
    let bundle = mis_orchestrator::apps::unpack(&demo("v1")).unwrap();
    apps.publish("demo", &bundle, 0, true).unwrap();
    apps.set_member("demo", "me@example.com", Some(mis_types::AppRole::Viewer))
        .unwrap();
    // Every request seems to come from a LAN peer.
    let lan = std::net::SocketAddr::from(([192, 168, 1, 9], 50000));
    let router = mis_orchestrator::apps_http::router(apps, std::sync::Arc::new(apps_auth()))
        .layer(MockConnectInfo(lan));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let host = format!("\r\nHost: {HOME}");
    assert_eq!(app_get(&base, "/a/demo/", "").await.0, 401);
    assert_eq!(app_get(&base, "/a/demo/", &host).await.0, 401);
    // With the apps JWT it is let in.
    let jwt = access_jwt_for(APPS_AUD, "me@example.com");
    let signed = format!("{host}\r\nCf-Access-Jwt-Assertion: {jwt}");
    assert_eq!(app_get(&base, "/a/demo/", &signed).await.0, 200);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn read_token_sees_members_but_cannot_share() {
    let (mis, _, dir) = start_with_apps(with_read_token()).await;
    assert_eq!(
        publish(&mis, "/api/apps/demo/releases", "s", &demo("v1"))
            .await
            .0,
        201
    );
    let share = r#"{"email":"k@example.com","role":"viewer"}"#;
    let (status, _) = post_json(&mis, "/api/apps/demo/members", "r", share).await;
    assert_eq!(status, 403);
    assert_eq!(
        post_json(&mis, "/api/apps/demo/members", "s", share)
            .await
            .0,
        200
    );
    let (_, body) = get_json(&mis, "/api/apps", "r").await;
    let list: Vec<AppView> = serde_json::from_str(&body).unwrap();
    assert_eq!(list[0].members[0].email, "k@example.com");
    let bad = r#"{"email":"not-an-email","role":"viewer"}"#;
    assert_eq!(
        post_json(&mis, "/api/apps/demo/members", "s", bad).await.0,
        400
    );
    let (status, _) = post_json(&mis, "/api/apps/nope/members", "s", share).await;
    assert_eq!(status, 404);
    std::fs::remove_dir_all(dir).unwrap();
}

/// POST `body` (JSON) to `path` on the apps listener with extra head lines: status, body.
async fn app_post(apps: &str, path: &str, extra: &str, body: &str) -> (u16, String) {
    let head = format!("POST {path} HTTP/1.1\r\nContent-Type: application/json{extra}");
    let (status, _, body) = http(apps, &head, body.as_bytes()).await;
    (status, String::from_utf8(body).unwrap())
}

const MUTATIONS: &str = "/a/demo/api/v1/mutations";

fn put_op(space: &str, id: &str, rev: u64, data: serde_json::Value) -> String {
    serde_json::json!({"op": "put", "space": space, "collection": "notes", "id": id, "rev": rev, "data": data})
        .to_string()
}

/// The `q` parameter for `query`.
fn q(query: serde_json::Value) -> String {
    let raw = query.to_string();
    let enc: String = (raw.bytes())
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("?q={enc}")
}

/// A live demo app on a fresh apps listener (`app` takes full token `s`); loopback requests
/// are its owner.
async fn live_demo(
    app: App,
    auth: mis_orchestrator::apps_http::Auth,
) -> (String, String, std::path::PathBuf) {
    let (mis, apps, dir) = start_with_apps_auth(app, auth).await;
    let page = "/api/apps/demo/releases?activate=true";
    assert_eq!(publish(&mis, page, "s", &demo("v1")).await.0, 201);
    (mis, apps, dir)
}

#[tokio::test]
async fn records_put_get_list_and_page() {
    let (_, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    for (id, score) in [("a", 3), ("b", 1), ("c", 2), ("d", 3), ("e", 5)] {
        let op = put_op(
            "shared",
            id,
            0,
            serde_json::json!({"score": score, "odd": score % 2 == 1}),
        );
        let (status, body) = app_post(&apps, MUTATIONS, "", &op).await;
        assert_eq!(status, 200, "{body}");
        let w: Written = serde_json::from_str(&body).unwrap();
        let e = w.record.unwrap();
        assert_eq!((e.id.as_str(), e.rev, e.by.as_str()), (id, 1, "local"));
    }
    let (status, _, body) = app_get(&apps, "/a/demo/api/v1/records/shared/notes/c", "").await;
    assert_eq!(status, 200);
    let e: Envelope = serde_json::from_str(&body).unwrap();
    assert_eq!(e.data, serde_json::json!({"score": 2, "odd": false}));
    assert_eq!(
        app_get(&apps, "/a/demo/api/v1/records/shared/notes/zz", "")
            .await
            .0,
        404
    );
    // Odd scores, highest first, two a page: e, then a and d (tied, by id descending), then b.
    let query = serde_json::json!({"where": {"odd": {"eq": true}}, "order_by": ["score", "desc"], "limit": 2});
    let mut seen = vec![];
    let mut next = query.clone();
    let mut pages = 0;
    loop {
        let path = format!("/a/demo/api/v1/records/shared/notes{}", q(next.clone()));
        let (status, _, body) = app_get(&apps, &path, "").await;
        assert_eq!(status, 200, "{body}");
        let page: Page = serde_json::from_str(&body).unwrap();
        pages += 1;
        seen.extend(page.records.into_iter().map(|e| e.id));
        match page.cursor {
            Some(c) => next["cursor"] = c.into(),
            None => break,
        }
    }
    assert_eq!(
        (seen, pages),
        (vec!["e".to_string(), "d".into(), "a".into(), "b".into()], 2)
    );
    // Refused queries, names and spaces.
    for path in [
        format!(
            "/a/demo/api/v1/records/shared/notes{}",
            q(serde_json::json!({"limit": 101}))
        ),
        format!(
            "/a/demo/api/v1/records/shared/notes{}",
            q(serde_json::json!({"where": {"a') OR 1=1 --": {"eq": 1}}}))
        ),
        "/a/demo/api/v1/records/shared/notes?q=nope".into(),
        "/a/demo/api/v1/records/shared/Notes".into(),
        "/a/demo/api/v1/records/everyone/notes".into(),
    ] {
        assert_eq!(app_get(&apps, &path, "").await.0, 400, "{path}");
    }
    let (status, body) = app_post(&apps, MUTATIONS, "", r#"{"op":"put","space":"shared"}"#).await;
    assert_eq!(status, 400, "{body}");
    let big = put_op("shared", "big", 0, serde_json::json!("x".repeat(70_000)));
    assert_eq!(app_post(&apps, MUTATIONS, "", &big).await.0, 413);
    // An app with no live release has no records.
    let (status, _) = app_post(
        &apps,
        "/a/nope/api/v1/mutations",
        "",
        &put_op("shared", "a", 0, 1.into()),
    )
    .await;
    assert_eq!(status, 404);
    // GET and POST only.
    let (status, _, _) = http(
        &apps,
        "PUT /a/demo/api/v1/mutations HTTP/1.1\r\nContent-Type: application/json",
        b"{}",
    )
    .await;
    assert_eq!(status, 405);
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn records_conflict_once_and_retries_succeed() {
    let (_, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let first = put_op("shared", "a", 0, serde_json::json!({"n": 0}));
    assert_eq!(app_post(&apps, MUTATIONS, "", &first).await.0, 200);
    // Two writers read rev 1 and write at once: exactly one wins.
    let (w1, w2) = (
        put_op("shared", "a", 1, serde_json::json!({"n": 1})),
        put_op("shared", "a", 1, serde_json::json!({"n": 2})),
    );
    let (one, two) = tokio::join!(
        app_post(&apps, MUTATIONS, "", &w1),
        app_post(&apps, MUTATIONS, "", &w2),
    );
    let mut statuses = [one.0, two.0];
    statuses.sort();
    assert_eq!(statuses, [200, 409], "{one:?} {two:?}");
    let lost = if one.0 == 409 { &one.1 } else { &two.1 };
    let c: Conflict = serde_json::from_str(lost).unwrap();
    assert_eq!(c.current.as_ref().unwrap().rev, 2);
    // The winner's write, sent again (its response lost): 200 with the same record.
    let won = c.current.unwrap();
    let again = put_op("shared", "a", 1, won.data.clone());
    let (status, body) = app_post(&apps, MUTATIONS, "", &again).await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Written>(&body).unwrap().record,
        Some(won)
    );
    // Create-only on an existing record with other data: 409.
    assert_eq!(
        app_post(&apps, MUTATIONS, "", &first.replace("0}", "9}"))
            .await
            .0,
        409
    );
    // Removes: stale is 409, then done, then done again.
    let rm = |rev: u64| {
        serde_json::json!({"op": "remove", "space": "shared", "collection": "notes", "id": "a", "rev": rev}).to_string()
    };
    assert_eq!(app_post(&apps, MUTATIONS, "", &rm(1)).await.0, 409);
    assert_eq!(
        app_post(&apps, MUTATIONS, "", &rm(2)).await,
        (200, r#"{"record":null}"#.into())
    );
    assert_eq!(
        app_post(&apps, MUTATIONS, "", &rm(2)).await,
        (200, r#"{"record":null}"#.into())
    );
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn records_follow_roles_and_me_stays_private() {
    let (mis, apps, dir) = live_demo(behind_access(), apps_auth()).await;
    for (email, role) in [
        ("ed@example.com", "editor"),
        ("vi@example.com", "viewer"),
        ("ow@example.com", "owner"),
    ] {
        let share = format!(r#"{{"email":"{email}","role":"{role}"}}"#);
        assert_eq!(
            post_json(&mis, "/api/apps/demo/members", "s", &share)
                .await
                .0,
            200
        );
    }
    let who = |email: &str| {
        format!(
            "{}\r\nOrigin: https://{HOME}",
            via_home(&access_jwt_for(APPS_AUD, email))
        )
    };
    let (ed, vi, ow) = (
        who("ed@example.com"),
        who("vi@example.com"),
        who("ow@example.com"),
    );
    let note = put_op("shared", "n", 0, serde_json::json!({"t": "all"}));
    assert_eq!(app_post(&apps, MUTATIONS, &ed, &note).await.0, 200);
    let draft = put_op("me", "d", 0, serde_json::json!({"t": "mine"}));
    assert_eq!(app_post(&apps, MUTATIONS, &ed, &draft).await.0, 200);
    // Viewers read, but can't write, not even their own `me` records.
    assert_eq!(
        app_get(&apps, "/a/demo/api/v1/records/shared/notes/n", &vi)
            .await
            .0,
        200
    );
    let (status, body) = app_post(&apps, MUTATIONS, &vi, &put_op("shared", "v", 0, 1.into())).await;
    assert_eq!(
        (status, body.as_str()),
        (403, "a viewer can't do that here")
    );
    assert_eq!(
        app_post(&apps, MUTATIONS, &vi, &put_op("me", "v", 0, 1.into()))
            .await
            .0,
        403
    );
    // The editor's `me` record: theirs alone, not even the owner's to see.
    assert_eq!(
        app_get(&apps, "/a/demo/api/v1/records/me/notes/d", &ed)
            .await
            .0,
        200
    );
    for other in [&vi, &ow] {
        assert_eq!(
            app_get(&apps, "/a/demo/api/v1/records/me/notes/d", other)
                .await
                .0,
            404
        );
        let (_, _, body) = app_get(&apps, "/a/demo/api/v1/records/me/notes", other).await;
        assert_eq!(serde_json::from_str::<Page>(&body).unwrap().records, []);
        let (_, _, body) = app_get(&apps, "/a/demo/api/v1/export", other).await;
        let x: Export = serde_json::from_str(&body).unwrap();
        assert_eq!((x.version, x.shared["notes"].len(), x.me.len()), (1, 1, 0));
        assert!(!body.contains("mine"), "{body}");
    }
    // The owner writing the same `me` id makes their own record, not the editor's.
    assert_eq!(app_post(&apps, MUTATIONS, &ow, &draft).await.0, 200);
    let (status, _, body) = app_get(&apps, "/a/demo/api/v1/export", &ed).await;
    assert_eq!(status, 200);
    let x: Export = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (x.app.as_str(), x.user.as_str()),
        ("demo", "ed@example.com")
    );
    assert_eq!(x.me["notes"][0].by, "ed@example.com");
    // Non-members: still 403 before any record.
    let stranger = who("x@example.com");
    assert_eq!(
        app_get(&apps, "/a/demo/api/v1/export", &stranger).await.0,
        403
    );
    assert_eq!(app_post(&apps, MUTATIONS, &stranger, &note).await.0, 403);
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

fn batch(ops: &[String]) -> String {
    format!(r#"{{"ops":[{}]}}"#, ops.join(","))
}

#[tokio::test]
async fn record_batches_land_whole_or_not_at_all() {
    let (mis, apps, dir) = live_demo(behind_access(), apps_auth()).await;
    let get = |id: &str| format!("/a/demo/api/v1/records/shared/notes/{id}?missing=null");
    let first = put_op("shared", "a", 0, serde_json::json!({"n": 1}));
    assert_eq!(app_post(&apps, MUTATIONS, "", &first).await.0, 200);
    // A conflict at ops[1]: 409 naming it, with the stored record; ops[0] isn't kept.
    let stale = batch(&[
        put_op("shared", "b", 0, 2.into()),
        put_op("shared", "a", 0, 9.into()),
    ]);
    let (status, body) = app_post(&apps, MUTATIONS, "", &stale).await;
    assert_eq!(status, 409, "{body}");
    let c: Conflict = serde_json::from_str(&body).unwrap();
    assert_eq!((c.index, c.current.map(|e| e.rev)), (Some(1), Some(1)));
    assert_eq!(app_get(&apps, &get("b"), "").await.2, "null");
    // At the stored revs: every record back, in order.
    let ok = batch(&[
        put_op("shared", "b", 0, 2.into()),
        put_op("shared", "a", 1, 9.into()),
        put_op("me", "a", 0, 3.into()),
    ]);
    let (status, body) = app_post(&apps, MUTATIONS, "", &ok).await;
    assert_eq!(status, 200, "{body}");
    let w: BatchWritten = serde_json::from_str(&body).unwrap();
    let revs: Vec<_> = w.records.iter().map(|e| e.as_ref().unwrap().rev).collect();
    assert_eq!(revs, [1, 2, 1]);
    // Sent again after a lost answer: the same 200.
    assert_eq!(app_post(&apps, MUTATIONS, "", &ok).await, (200, body));
    // Caps: a record too big, too many ops, one id twice: refused by index, nothing kept.
    let big = put_op("shared", "big", 0, serde_json::json!("x".repeat(70_000)));
    let (status, why) = app_post(
        &apps,
        MUTATIONS,
        "",
        &batch(&[put_op("shared", "c", 0, 1.into()), big]),
    )
    .await;
    assert_eq!(status, 413);
    assert!(why.starts_with("ops[1]: data is"), "{why}");
    let many: Vec<_> = (0..=100)
        .map(|i| put_op("shared", &format!("m{i}"), 0, 1.into()))
        .collect();
    assert_eq!(app_post(&apps, MUTATIONS, "", &batch(&many)).await.0, 400);
    // A full batch of near-cap records fits the body limit.
    let fat: Vec<_> = (0..100)
        .map(|i| put_op("shared", &format!("f{i}"), 0, "x".repeat(60_000).into()))
        .collect();
    assert_eq!(app_post(&apps, MUTATIONS, "", &batch(&fat)).await.0, 200);
    let twice = batch(&[
        put_op("shared", "d", 0, 1.into()),
        put_op("shared", "d", 1, 2.into()),
    ]);
    let (status, why) = app_post(&apps, MUTATIONS, "", &twice).await;
    assert_eq!(
        (status, why.as_str()),
        (400, "ops[1]: shared/notes/d is already in this batch")
    );
    for id in ["c", "m0", "d"] {
        assert_eq!(app_get(&apps, &get(id), "").await.2, "null", "{id}");
    }
    // Roles: a viewer's batch is refused whole.
    let share = r#"{"email":"vi@example.com","role":"viewer"}"#;
    assert_eq!(
        post_json(&mis, "/api/apps/demo/members", "s", share)
            .await
            .0,
        200
    );
    let vi = format!(
        "{}\r\nOrigin: https://{HOME}",
        via_home(&access_jwt_for(APPS_AUD, "vi@example.com"))
    );
    let mine = batch(&[put_op("me", "v", 0, 1.into())]);
    assert_eq!(app_post(&apps, MUTATIONS, &vi, &mine).await.0, 403);
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_missing_record_can_answer_null_and_sdk_types_are_served() {
    let (_, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let path = "/a/demo/api/v1/records/shared/notes/zz";
    assert_eq!(app_get(&apps, path, "").await.0, 404);
    let (status, _, body) = app_get(&apps, &format!("{path}?missing=null"), "").await;
    assert_eq!((status, body.as_str()), (200, "null"));
    assert_eq!(
        app_get(&apps, &format!("{path}?missing=nope"), "").await.0,
        400
    );
    // Still 404 for an app that isn't live.
    let gone = "/a/nope/api/v1/records/shared/notes/zz?missing=null";
    assert_eq!(app_get(&apps, gone, "").await.0, 404);
    let (status, _, body) = app_get(&apps, "/a/_/mis.d.ts", "").await;
    assert_eq!(status, 200);
    assert!(body.contains("interface Mis"), "{body}");
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn dev_role_header_only_under_apps_dev_and_only_for_local() {
    let me = "/a/demo/api/v1/me";
    let as_viewer = "\r\nX-Mis-Dev-Role: viewer";
    // Without --apps-dev the header is ignored: loopback stays owner.
    let (_, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let (_, _, body) = app_get(&apps, me, as_viewer).await;
    assert!(body.contains(r#""role":"owner""#), "{body}");
    let _ = std::fs::remove_dir_all(dir);
    let dev = mis_orchestrator::apps_http::Auth {
        dev: true,
        ..apps_auth()
    };
    let (mis, apps, dir) = live_demo(behind_access(), dev).await;
    let (_, _, body) = app_get(&apps, me, as_viewer).await;
    assert!(body.contains(r#""role":"viewer""#), "{body}");
    let op = put_op("shared", "a", 0, 1.into());
    assert_eq!(app_post(&apps, MUTATIONS, as_viewer, &op).await.0, 403);
    let as_editor = "\r\nX-Mis-Dev-Role: editor";
    assert_eq!(app_post(&apps, MUTATIONS, as_editor, &op).await.0, 200);
    assert_eq!(app_get(&apps, me, "\r\nX-Mis-Dev-Role: owner").await.0, 400);
    // A signed-in member can't raise themselves with it.
    let share = r#"{"email":"vi@example.com","role":"viewer"}"#;
    assert_eq!(
        post_json(&mis, "/api/apps/demo/members", "s", share)
            .await
            .0,
        200
    );
    let vi = via_home(&access_jwt_for(APPS_AUD, "vi@example.com"));
    let (_, _, body) = app_get(&apps, me, &format!("{vi}{as_editor}")).await;
    assert!(body.contains(r#""role":"viewer""#), "{body}");
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn record_writes_need_json_from_the_apps_own_origin() {
    let (_, apps, dir) = live_demo(behind_access(), apps_auth()).await;
    let op = put_op("shared", "a", 0, 1.into());
    let addr = &apps[5..];
    // Loopback is owner, so a page on another origin must not write through it.
    for origin in [
        "https://evil.example",
        "http://localhost:9",
        "null",
        &format!("http://{HOME}"),
    ] {
        let (status, body) =
            app_post(&apps, MUTATIONS, &format!("\r\nOrigin: {origin}"), &op).await;
        assert_eq!(
            (status, body.as_str()),
            (403, "writes take JSON from this app's own page"),
            "{origin}"
        );
    }
    // No JSON content type: what an HTML form or a no-preflight fetch sends.
    for ct in ["text/plain", "application/x-www-form-urlencoded"] {
        let head = format!("POST {MUTATIONS} HTTP/1.1\r\nContent-Type: {ct}");
        assert_eq!(http(&apps, &head, op.as_bytes()).await.0, 403, "{ct}");
    }
    let (status, _, _) = http(&apps, &format!("POST {MUTATIONS} HTTP/1.1"), op.as_bytes()).await;
    assert_eq!(status, 403);
    assert_eq!(
        app_get(&apps, "/a/demo/api/v1/records/shared/notes/a", "")
            .await
            .0,
        404
    );
    // Its own page, on loopback or through the public name; or no browser at all.
    let own = format!("\r\nOrigin: http://{addr}");
    assert_eq!(app_post(&apps, MUTATIONS, &own, &op).await.0, 200);
    let member = via_home(&access_jwt_for(APPS_AUD, "me@example.com"));
    let public = format!("{member}\r\nOrigin: https://{HOME}");
    let op = put_op("shared", "b", 0, 1.into());
    assert_eq!(app_post(&apps, MUTATIONS, &public, &op).await.0, 403); // not a member yet
    assert_eq!(
        app_post(&apps, MUTATIONS, "", &put_op("shared", "c", 0, 1.into()))
            .await
            .0,
        200
    );
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn records_survive_a_restart() {
    let (_, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let op = put_op("shared", "a", 0, serde_json::json!({"kept": true}));
    assert_eq!(app_post(&apps, MUTATIONS, "", &op).await.0, 200);
    // A second listener over the same folder: a restarted orchestrator.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let again = format!("ws://{}", listener.local_addr().unwrap());
    let store = std::sync::Arc::new(mis_orchestrator::apps::Apps::new(dir.clone(), None));
    tokio::spawn(mis_orchestrator::serve_apps(
        listener,
        store,
        Default::default(),
    ));
    let (status, _, body) = app_get(&again, "/a/demo/api/v1/records/shared/notes/a", "").await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Envelope>(&body).unwrap().data,
        serde_json::json!({"kept": true})
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("demo/data.sqlite"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Best effort: on Windows the still-running server holds data.sqlite open.
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn mis_credentials_cannot_reach_records() {
    let (mis, apps, dir) = live_demo(behind_access(), apps_auth()).await;
    let path = "/a/demo/api/v1/export";
    // The full token, the MIS Access app's JWT: refused on the apps listener.
    let tunnel = format!("\r\nHost: {HOME}\r\nCF-Ray: 8f00-SJC");
    let bearer = format!("{tunnel}\r\nAuthorization: Bearer s");
    assert_eq!(app_get(&apps, path, &bearer).await.0, 401);
    let mis_jwt = via_home(&access_jwt_for(ACCESS_AUD, "me@example.com"));
    assert_eq!(app_get(&apps, path, &mis_jwt).await.0, 401);
    let op = put_op("shared", "a", 0, 1.into());
    assert_eq!(
        app_post(
            &apps,
            MUTATIONS,
            &format!("{mis_jwt}\r\nOrigin: https://{HOME}"),
            &op
        )
        .await
        .0,
        401
    );
    // And the orchestrator has no records routes.
    assert_eq!(get_json(&mis, path, "s").await.0, 404);
    assert_eq!(post_json(&mis, MUTATIONS, "s", &op).await.0, 404);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_sdk_is_served_to_anyone_signed_in() {
    let (_, apps, dir) = live_demo(behind_access(), apps_auth()).await;
    // Not a member of any app, but signed in.
    let someone = via_home(&access_jwt_for(APPS_AUD, "x@example.com"));
    for path in ["/a/_/mis.js", "/a/_/mis-mock.js"] {
        let (status, head, body) = app_get(&apps, path, &someone).await;
        assert_eq!(status, 200, "{path}");
        assert!(head.contains("text/javascript"), "{head}");
        assert!(body.contains("MisError"), "{path}");
    }
    let bare = format!("\r\nHost: {HOME}\r\nCF-Ray: 8f00-SJC");
    assert_eq!(app_get(&apps, "/a/_/mis.js", &bare).await.0, 401);
    // Anything else under `_` is no app's: refused like one not shared, never a 500.
    for path in ["/a/_/nope.js", "/a/_/api/v1/me", "/a/_/api/v1/export"] {
        assert_eq!(app_get(&apps, path, &someone).await.0, 403, "{path}");
        assert_eq!(app_get(&apps, path, "").await.0, 404, "{path}");
    }
    // A decoded slug that walks back to a real app gets nothing either.
    let sneaky = "/a/demo%2F..%2Fdemo/api/v1/export";
    assert_eq!(app_get(&apps, sneaky, "").await.0, 404);
    std::fs::remove_dir_all(dir).unwrap();
}

/// `demo`'s shared notes on the apps listener: (id, data).
async fn demo_notes(apps: &str) -> Vec<(String, serde_json::Value)> {
    let (status, _, body) = app_get(apps, "/a/demo/api/v1/records/shared/notes", "").await;
    assert_eq!(status, 200, "{body}");
    let page: Page = serde_json::from_str(&body).unwrap();
    (page.records.into_iter()).map(|e| (e.id, e.data)).collect()
}

async fn restore_demo(mis: &str, backup: &str, with_policy: bool) -> Restored {
    let body = serde_json::json!({"backup": backup, "with_policy": with_policy}).to_string();
    let (status, body) = post_json(mis, "/api/apps/demo/restore", "s", &body).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn app_backups_restore_and_the_pre_restore_one_undoes_it() {
    let (mis, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let write = |id: &'static str, rev, v: i64| {
        let apps = apps.clone();
        async move {
            let op = put_op("shared", id, rev, v.into());
            assert_eq!(app_post(&apps, MUTATIONS, "", &op).await.0, 200);
        }
    };
    // No records yet: nothing to back up.
    assert_eq!(
        post_json(&mis, "/api/apps/demo/backups", "s", "").await.0,
        409
    );
    write("a", 0, 1).await;
    let (status, body) = post_json(&mis, "/api/apps/demo/backups", "s", "").await;
    assert_eq!(status, 201, "{body}");
    let snap: AppBackup = serde_json::from_str(&body).unwrap();
    assert_eq!(snap.kind, BackupKind::Manual);
    let taken = demo_notes(&apps).await;
    write("a", 1, 2).await;
    write("b", 0, 3).await;
    let newer = demo_notes(&apps).await;
    let restored = restore_demo(&mis, &snap.name, false).await;
    assert_eq!(demo_notes(&apps).await, taken);
    // The snapshot taken first puts the newer records back.
    let pre = restored.pre_restore.unwrap();
    assert_eq!(pre.kind, BackupKind::PreRestore);
    restore_demo(&mis, &pre.name, false).await;
    assert_eq!(demo_notes(&apps).await, newer);
    let (status, body) = get_json(&mis, "/api/apps/demo/backups", "r").await;
    assert_eq!(status, 200);
    let list: Vec<AppBackup> = serde_json::from_str(&body).unwrap();
    let kinds: Vec<_> = list.iter().map(|b| b.kind).collect();
    use BackupKind::*;
    assert_eq!(kinds, [Manual, PreRestore, PreRestore]);
    assert!(list.iter().all(|b| b.size > 0));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn restore_brings_back_members_only_when_asked() {
    let (mis, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let op = put_op("shared", "a", 0, 1.into());
    assert_eq!(app_post(&apps, MUTATIONS, "", &op).await.0, 200);
    let share = r#"{"email": "x@example.com", "role": "editor"}"#;
    assert_eq!(
        post_json(&mis, "/api/apps/demo/members", "s", share)
            .await
            .0,
        200
    );
    let (_, body) = post_json(&mis, "/api/apps/demo/backups", "s", "").await;
    let snap: AppBackup = serde_json::from_str(&body).unwrap();
    let unshare = r#"{"email": "x@example.com", "role": null}"#;
    assert_eq!(
        post_json(&mis, "/api/apps/demo/members", "s", unshare)
            .await
            .0,
        200
    );
    let members = |mis: String| async move {
        let (_, body) = get_json(&mis, "/api/apps", "s").await;
        serde_json::from_str::<Vec<AppView>>(&body).unwrap()[0]
            .members
            .len()
    };
    restore_demo(&mis, &snap.name, false).await;
    assert_eq!(members(mis.clone()).await, 0);
    restore_demo(&mis, &snap.name, true).await;
    assert_eq!(members(mis.clone()).await, 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn activating_a_release_backs_up_first() {
    let (mis, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let op = put_op("shared", "a", 0, 1.into());
    assert_eq!(app_post(&apps, MUTATIONS, "", &op).await.0, 200);
    assert_eq!(
        publish(&mis, "/api/apps/demo/releases", "s", &demo("v2"))
            .await
            .0,
        201
    );
    let (status, _) = post_json(&mis, "/api/apps/demo/activate", "s", r#"{"release": 2}"#).await;
    assert_eq!(status, 204);
    let page = "/api/apps/demo/releases?activate=true";
    assert_eq!(publish(&mis, page, "s", &demo("v3")).await.0, 201);
    // Activating the live release again takes none.
    let (status, _) = post_json(&mis, "/api/apps/demo/activate", "s", r#"{"release": 3}"#).await;
    assert_eq!(status, 204);
    let (_, body) = get_json(&mis, "/api/apps/demo/backups", "s").await;
    let list: Vec<AppBackup> = serde_json::from_str(&body).unwrap();
    let names: Vec<_> = list.iter().map(|b| &b.name[..7]).collect();
    assert_eq!(names, ["pre-v2-", "pre-v3-"]);
    assert!(list.iter().all(|b| b.kind == BackupKind::PreActivation));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn read_token_lists_backups_but_cannot_take_or_restore_and_bad_names_are_refused() {
    let (mis, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let op = put_op("shared", "a", 0, 1.into());
    assert_eq!(app_post(&apps, MUTATIONS, "", &op).await.0, 200);
    let (_, body) = post_json(&mis, "/api/apps/demo/backups", "s", "").await;
    let snap: AppBackup = serde_json::from_str(&body).unwrap();
    assert_eq!(get_json(&mis, "/api/apps/demo/backups", "r").await.0, 200);
    assert_eq!(
        post_json(&mis, "/api/apps/demo/backups", "r", "").await.0,
        403
    );
    let body = serde_json::json!({"backup": snap.name}).to_string();
    assert_eq!(
        post_json(&mis, "/api/apps/demo/restore", "r", &body)
            .await
            .0,
        403
    );
    for bad in [
        "../data",
        "../../demo/data",
        "manual-x",
        &format!("{}/../../data", snap.name),
        &format!("{}.sqlite", snap.name),
    ] {
        let body = serde_json::json!({"backup": bad}).to_string();
        let (status, _) = post_json(&mis, "/api/apps/demo/restore", "s", &body).await;
        assert_eq!(status, 400, "{bad}");
    }
    let body = serde_json::json!({"backup": "daily-20200101T000000.000Z"}).to_string();
    assert_eq!(
        post_json(&mis, "/api/apps/demo/restore", "s", &body)
            .await
            .0,
        404
    );
    assert_eq!(get_json(&mis, "/api/apps/nope/backups", "s").await.0, 404);
    assert_eq!(
        post_json(&mis, "/api/apps/nope/backups", "s", "").await.0,
        404
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn writes_during_a_restore_are_kept_or_backed_up_never_lost() {
    let (mis, apps, dir) = live_demo(with_read_token(), Default::default()).await;
    let op = put_op("shared", "seed", 0, 0.into());
    assert_eq!(app_post(&apps, MUTATIONS, "", &op).await.0, 200);
    let (_, body) = post_json(&mis, "/api/apps/demo/backups", "s", "").await;
    let snap: AppBackup = serde_json::from_str(&body).unwrap();
    let writer = {
        let apps = apps.clone();
        tokio::spawn(async move {
            for i in 0..60 {
                let op = put_op("shared", &format!("w{i:02}"), 0, i.into());
                let (status, body) = app_post(&apps, MUTATIONS, "", &op).await;
                assert_eq!(status, 200, "{body}");
            }
        })
    };
    while demo_notes(&apps).await.len() < 10 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let pre = restore_demo(&mis, &snap.name, false)
        .await
        .pre_restore
        .unwrap();
    writer.await.unwrap();
    let ids = |notes: Vec<(String, serde_json::Value)>| -> Vec<String> {
        notes
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| id != "seed")
            .collect()
    };
    let after = ids(demo_notes(&apps).await);
    restore_demo(&mis, &pre.name, false).await;
    let before = ids(demo_notes(&apps).await);
    // Each write is in one of them: in order, before the restore or after it.
    let all: Vec<String> = (0..60).map(|i| format!("w{i:02}")).collect();
    assert_eq!([before.clone(), after].concat(), all);
    assert!(before.len() >= 10);
    let _ = std::fs::remove_dir_all(dir);
}

/// Start an app task (`body`, with `headers`) and take its assignment: task and token.
async fn app_task(
    base: &str,
    worker: &mut Ws,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) -> (Task, String) {
    let body = Some((
        "application/json".to_string(),
        body.to_string().into_bytes(),
    ));
    let (status, reply) = call(base, "POST", "/api/apps/tasks", headers, body).await;
    assert_eq!(status, 201, "{reply}");
    let OrchMsg::Assign { task, token, .. } = recv(worker).await else {
        panic!("assign")
    };
    (task, token.expect("an app task carries a token"))
}

/// End `task` on `worker` as done, in session `sess`; returns once its token is refused.
async fn finish_app_task(mis: &str, worker: &mut Ws, task: TaskId, token: &str) {
    for event in [
        AgentEvent::Started {
            session: Some("sess".into()),
            model: None,
        },
        AgentEvent::Finished {
            ok: true,
            summary: None,
        },
    ] {
        send(worker, &WorkerMsg::Event { task, event }).await;
    }
    for _ in 0..100 {
        if publish(mis, "/api/apps/pool/releases", token, &demo("x"))
            .await
            .0
            == 401
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the token outlived its task");
}

#[tokio::test]
async fn a_change_task_token_stages_only_its_app_until_the_task_ends() {
    let (mis, _, dir) = start_with_apps(with_read_token()).await;
    for slug in ["pool", "budget"] {
        let path = format!("/api/apps/{slug}/releases");
        assert_eq!(publish(&mis, &path, "s", &demo("v1")).await.0, 201);
    }
    let (_, mut worker) = join_as(&mis).await;
    let change = serde_json::json!({"request": "Add a chart.", "slug": "pool", "change": true,
        "harness": "claude"});
    let auth = [("authorization", "Bearer s")];
    let (task, token) = app_task(&mis, &mut worker, &auth, change).await;

    // Its app, staged: yes. Live, another app, or any other route: no.
    let (status, body) = publish(&mis, "/api/apps/pool/releases", &token, &demo("v2")).await;
    assert_eq!((status, body.as_str()), (201, r#"{"release":2}"#));
    let live = "/api/apps/pool/releases?activate=true";
    let (status, why) = publish(&mis, live, &token, &demo("v3")).await;
    assert!(status == 403 && why.contains("owner"), "{status} {why}");
    let (status, why) = publish(&mis, "/api/apps/budget/releases", &token, &demo("v2")).await;
    assert!(status == 403 && why.contains("pool"), "{status} {why}");
    let bearer = format!("Bearer {token}");
    refused_everywhere_else(&mis, &[("authorization", bearer.as_str())]).await;
    let (status, _) = post_json(&mis, "/api/apps/pool/activate", &token, r#"{"release":2}"#).await;
    assert_eq!(status, 401);
    let (_, list) = get_json(&mis, "/api/apps", "s").await;
    let list: Vec<AppView> = serde_json::from_str(&list).unwrap();
    let pool = list.iter().find(|a| a.slug == "pool").unwrap();
    assert_eq!((pool.active, pool.releases.len()), (None, 2));

    // Ended: refused. A reply gets a token of its own, bound the same way.
    finish_app_task(&mis, &mut worker, task.id, &token).await;
    let (mut client, _) = connect_async(client_req(&mis, &[("authorization", "Bearer s")]))
        .await
        .unwrap();
    reply(&mut client, task.id, "Bigger.").await;
    let OrchMsg::Assign {
        token: Some(again), ..
    } = recv(&mut worker).await
    else {
        panic!("assign with a token")
    };
    assert_ne!(again, token);
    assert_eq!(
        publish(&mis, "/api/apps/pool/releases", &again, &demo("v3"))
            .await
            .0,
        201
    );
    assert_eq!(
        publish(&mis, "/api/apps/budget/releases", &again, &demo("v3"))
            .await
            .0,
        403
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_new_app_task_token_creates_one_app_owned_by_whoever_asked() {
    let (mis, _, dir) = start_with_apps(behind_access()).await;
    assert_eq!(
        publish(&mis, "/api/apps/pool/releases", "s", &demo("v1"))
            .await
            .0,
        201
    );
    let (_, mut worker) = join_as(&mis).await;
    // Never started: a new app named like one that exists, a change to one that doesn't.
    for (body, why) in [
        (
            r#"{"request":"x","slug":"pool","harness":"claude"}"#,
            "exists",
        ),
        (
            r#"{"request":"x","slug":"nope","change":true,"harness":"claude"}"#,
            "no app",
        ),
    ] {
        let (status, got) = post_json(&mis, "/api/apps/tasks", "s", body).await;
        assert!(status == 400 && got.contains(why), "{status} {got}");
    }
    let jwt = access_jwt_for(ACCESS_AUD, "Me@Example.com");
    let new = serde_json::json!({"request": "Chores.", "harness": "claude"});
    let (_, token) = app_task(
        &mis,
        &mut worker,
        &[("cf-access-jwt-assertion", jwt.as_str())],
        new,
    )
    .await;

    // Not over an app that exists; one new one, then only that one.
    let (status, why) = publish(&mis, "/api/apps/pool/releases", &token, &demo("x")).await;
    assert!(
        status == 409 && why.contains("pool exists"),
        "{status} {why}"
    );
    let (status, body) = publish(&mis, "/api/apps/chores/releases", &token, &demo("v1")).await;
    assert_eq!((status, body.as_str()), (201, r#"{"release":1}"#));
    let (status, why) = publish(&mis, "/api/apps/dishes/releases", &token, &demo("v1")).await;
    assert!(status == 403 && why.contains("chores"), "{status} {why}");
    let (status, body) = publish(&mis, "/api/apps/chores/releases", &token, &demo("v2")).await;
    assert_eq!((status, body.as_str()), (201, r#"{"release":2}"#));

    let (_, list) = get_json(&mis, "/api/apps", "s").await;
    let list: Vec<AppView> = serde_json::from_str(&list).unwrap();
    let chores = list.iter().find(|a| a.slug == "chores").unwrap();
    assert_eq!(chores.active, None);
    let owner = AppMember {
        email: "me@example.com".into(),
        role: AppRole::Owner,
    };
    assert_eq!(chores.members, vec![owner]);
    assert!(!list.iter().any(|a| a.slug == "dishes"));

    // Asked with the full token, not a login: the new app is shared with no one.
    let new = serde_json::json!({"request": "Dishes.", "slug": "dishes", "harness": "claude"});
    let auth = [("authorization", "Bearer s")];
    // The worker is busy with the first: a second worker takes it.
    let (_, mut second) = join_as(&mis).await;
    let (_, token) = app_task(&mis, &mut second, &auth, new).await;
    assert_eq!(
        publish(&mis, "/api/apps/chores/releases", &token, &demo("x"))
            .await
            .0,
        403
    );
    assert_eq!(
        publish(&mis, "/api/apps/dishes/releases", &token, &demo("v1"))
            .await
            .0,
        201
    );
    let (_, list) = get_json(&mis, "/api/apps", "s").await;
    let list: Vec<AppView> = serde_json::from_str(&list).unwrap();
    assert!(list
        .iter()
        .find(|a| a.slug == "dishes")
        .unwrap()
        .members
        .is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn with_no_full_token_set_an_app_task_token_is_still_held_to_its_app() {
    let (mis, _, dir) = start_with_apps(App::new(None)).await;
    assert_eq!(
        publish(&mis, "/api/apps/pool/releases", "", &demo("v1"))
            .await
            .0,
        201
    );
    let (_, mut worker) = join_as(&mis).await;
    let change = serde_json::json!({"request": "x", "slug": "pool", "change": true,
        "harness": "claude"});
    let (_, token) = app_task(&mis, &mut worker, &[], change).await;
    let live = "/api/apps/pool/releases?activate=true";
    assert_eq!(publish(&mis, live, &token, &demo("v2")).await.0, 403);
    let (status, _) = post_json(&mis, "/api/apps/pool/activate", &token, r#"{"release":1}"#).await;
    assert_eq!(status, 401);
    assert_eq!(
        publish(
            &mis,
            "/api/apps/pool/releases",
            "mis-app-ended",
            &demo("v2")
        )
        .await
        .0,
        401
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A PNG's magic and a byte: enough to be sniffed as one.
const PNG: [u8; 9] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1];

/// Answer the next query `worker` gets, a draft merge, with `result`; the query.
async fn answer_merge(worker: &mut Ws, result: Result<String, String>) -> Query {
    let OrchMsg::Query { req, query } = recv(worker).await else {
        panic!("query")
    };
    let answer = Answer::Merged { result };
    send(worker, &WorkerMsg::Answer { req, answer }).await;
    query
}

/// Publish or discard pool's draft `id` as Access login `jwt`.
async fn decide(mis: &str, action: &str, id: uuid::Uuid, jwt: &str) -> (u16, String) {
    let body = Some((
        "application/json".into(),
        format!(r#"{{"id":"{id}"}}"#).into_bytes(),
    ));
    let path = format!("/api/apps/pool/draft/{action}");
    call(
        mis,
        "POST",
        &path,
        &[("cf-access-jwt-assertion", jwt)],
        body,
    )
    .await
}

async fn pool_view(mis: &str, headers: &[(&str, &str)]) -> AppView {
    let (status, list) = call(mis, "GET", "/api/apps", headers, None).await;
    assert_eq!(status, 200, "{list}");
    let list: Vec<AppView> = serde_json::from_str(&list).unwrap();
    list.into_iter().find(|a| a.slug == "pool").unwrap()
}

#[tokio::test]
async fn an_app_task_proposes_a_draft_that_only_the_owner_publishes_or_discards() {
    use base64ct::{Base64, Encoding};
    let (mis, _, dir) = start_with_apps(behind_access()).await;
    let live = "/api/apps/pool/releases?activate=true";
    assert_eq!(publish(&mis, live, "s", &demo("v1")).await.0, 201);
    for (email, role) in [("me@example.com", "owner"), ("jen@example.com", "editor")] {
        let body = format!(r#"{{"email":"{email}","role":"{role}"}}"#);
        assert_eq!(
            post_json(&mis, "/api/apps/pool/members", "s", &body)
                .await
                .0,
            200
        );
    }
    let (_, mut worker) = join_as(&mis).await;
    let change = serde_json::json!({"request": "Add a chart.", "slug": "pool", "change": true,
        "harness": "claude"});
    let auth = [("authorization", "Bearer s")];
    let (task, token) = app_task(&mis, &mut worker, &auth, change).await;
    assert_eq!(
        publish(&mis, "/api/apps/pool/releases", &token, &demo("v2"))
            .await
            .0,
        201
    );

    // Proposed with the task's token: a staged release, the PR, screenshots.
    const PR: &str = "https://github.com/tommy-xr/mis-notes/pull/7";
    let head = "d68ea1f8a9b4b0890914ce16e5539e480e68c07b";
    let propose = |release: u32, before: &[u8]| {
        serde_json::json!({"release": release, "pr": PR, "head": head, "note": " Try the chart. ",
            "before": Base64::encode_string(before), "after": Base64::encode_string(&PNG)})
        .to_string()
    };
    let path = "/api/apps/pool/draft";
    let (status, why) = post_json(&mis, path, &token, &propose(1, &PNG)).await;
    assert!(status == 400 && why.contains("live"), "{status} {why}");
    let (status, why) = post_json(&mis, path, &token, &propose(2, b"<svg/>")).await;
    assert!(status == 400 && why.contains("PNG"), "{status} {why}");
    let (status, why) = post_json(&mis, "/api/apps/budget/draft", &token, &propose(2, &PNG)).await;
    assert!(status == 403 && why.contains("pool"), "{status} {why}");
    let (status, body) = post_json(&mis, path, &token, &propose(2, &PNG)).await;
    assert_eq!(status, 201, "{body}");
    let draft = pool_view(&mis, &auth).await.draft.unwrap();
    assert_eq!(
        (draft.release, draft.pr.as_str(), draft.task),
        (2, PR, Some(task.id))
    );
    assert_eq!(
        (draft.note.as_deref(), draft.before, draft.after),
        (Some("Try the chart."), true, true)
    );
    let (status, png) = call(&mis, "GET", "/api/apps/pool/draft/after", &auth, None).await;
    assert!(status == 200 && png.contains("PNG"), "{status}");

    // An editor sees it, read-only; the owner's publish merges the PR first.
    let jen = access_jwt_for(ACCESS_AUD, "jen@example.com");
    let me = access_jwt_for(ACCESS_AUD, "me@example.com");
    let as_jen = [("cf-access-jwt-assertion", jen.as_str())];
    let as_me = [("cf-access-jwt-assertion", me.as_str())];
    assert_eq!(pool_view(&mis, &as_jen).await.role, Some(AppRole::Editor));
    assert_eq!(pool_view(&mis, &as_me).await.role, Some(AppRole::Owner));
    let id = draft.id;
    let (status, why) = decide(&mis, "publish", id, &jen).await;
    assert!(status == 403 && why.contains("owner"), "{status} {why}");
    assert_eq!(
        decide(&mis, "publish", uuid::Uuid::new_v4(), &me).await.0,
        409
    );
    // A screenshot is served for its own draft only.
    let stale = format!("/api/apps/pool/draft/after?id={}", uuid::Uuid::new_v4());
    assert_eq!(call(&mis, "GET", &stale, &auth, None).await.0, 404);

    // A PR that won't merge leaves nothing live, and says why on the draft.
    let (got, query) = tokio::join!(
        decide(&mis, "publish", id, &me),
        answer_merge(&mut worker, Err("checks failed: build".into()))
    );
    assert!(got.0 == 409 && got.1.contains("checks failed"), "{got:?}");
    let Query::MergeDraft {
        cwd,
        repo,
        number,
        head: h,
    } = query
    else {
        panic!("{query:?}")
    };
    assert_eq!(
        (cwd.as_str(), repo.as_str(), number),
        ("~/code/mis-notes", "tommy-xr/mis-notes", 7)
    );
    assert_eq!(h, head);
    let pool = pool_view(&mis, &as_me).await;
    assert_eq!(pool.active, Some(1));
    assert_eq!(
        pool.draft.unwrap().error.as_deref(),
        Some("checks failed: build")
    );
    // Merged: live, and the draft is done.
    let (got, _) = tokio::join!(
        decide(&mis, "publish", id, &me),
        answer_merge(&mut worker, Ok("merged".into()))
    );
    assert_eq!(got.0, 204, "{got:?}");
    let pool = pool_view(&mis, &as_me).await;
    assert_eq!((pool.active, pool.draft), (Some(2), None));

    // Discarded: gone, nothing live changes, no merge.
    assert_eq!(
        publish(&mis, "/api/apps/pool/releases", &token, &demo("v3"))
            .await
            .0,
        201
    );
    assert_eq!(
        post_json(&mis, path, &token, &propose(3, &PNG)).await.0,
        201
    );
    let id = pool_view(&mis, &auth).await.draft.unwrap().id;
    assert_eq!(decide(&mis, "discard", id, &jen).await.0, 403);
    assert_eq!(decide(&mis, "discard", id, &me).await.0, 204);
    let pool = pool_view(&mis, &as_me).await;
    assert_eq!((pool.active, pool.draft), (Some(2), None));
    assert_eq!(
        call(&mis, "GET", "/api/apps/pool/draft/after", &auth, None)
            .await
            .0,
        404
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_owner_tries_a_draft_on_a_copy_of_the_records_that_goes_with_it() {
    let dev = mis_orchestrator::apps_http::Auth {
        dev: true,
        ..Default::default()
    };
    let (mis, apps, dir) = live_demo(with_read_token(), dev).await;
    let op = |id: &str| put_op("shared", id, 0, serde_json::json!({"n": 1}));
    assert_eq!(app_post(&apps, MUTATIONS, "", &op("real")).await.0, 200);
    assert_eq!(publish(&mis, "/api/apps/demo/releases", "s", &demo("v2")).await.0, 201);
    let propose = r#"{"release":2,"pr":"https://github.com/o/r/pull/1","head":"d68ea1f8a9b4b0890914ce16e5539e480e68c07b"}"#;
    assert_eq!(post_json(&mis, "/api/apps/demo/draft", "s", propose).await.0, 201);

    // Only the owner gets in, for the current draft.
    let editor = "\r\nX-Mis-Dev-Role: editor";
    assert_eq!(app_get(&apps, "/a/demo/?draft=2", editor).await.0, 404);
    assert_eq!(app_get(&apps, "/a/demo/?draft=3", "").await.0, 404);
    let (status, head, _) = app_get(&apps, "/a/demo/?draft=2", "").await;
    assert_eq!(status, 303, "{head}");
    assert!(head.contains("set-cookie: mis-draft-demo=2; Path=/a/demo/"), "{head}");
    let cookie = "\r\nCookie: mis-draft-demo=2";

    // In it: the draft's page and a copy of the records; writes stay in the copy.
    assert_eq!(app_get(&apps, "/a/demo/", cookie).await.2, "v2");
    assert_eq!(app_get(&apps, "/a/demo/", "").await.2, "v1");
    let me = "/a/demo/api/v1/me";
    assert!(app_get(&apps, me, cookie).await.2.contains(r#""draft":2"#));
    let one = "/a/demo/api/v1/records/shared/notes/real";
    assert_eq!(app_get(&apps, one, cookie).await.0, 200, "the copy has the real record");
    assert_eq!(app_post(&apps, MUTATIONS, cookie, &op("trial")).await.0, 200);
    let trial = "/a/demo/api/v1/records/shared/notes/trial";
    assert_eq!(app_get(&apps, trial, cookie).await.0, 200);
    assert_eq!(app_get(&apps, trial, "").await.0, 404, "never in the real records");
    // An editor with the cookie gets the live app.
    assert_eq!(app_get(&apps, "/a/demo/", &format!("{cookie}{editor}")).await.2, "v1");

    // Discarded: the cookie is stale (dropped), the copy gone; a new draft starts afresh.
    let discard = r#"{"release":2}"#;
    assert_eq!(post_json(&mis, "/api/apps/demo/draft/discard", "s", discard).await.0, 204);
    let (_, head, body) = app_get(&apps, "/a/demo/", cookie).await;
    assert_eq!(body, "v1");
    assert!(head.contains("set-cookie: mis-draft-demo=;"), "{head}");
    assert!(!dir.join("demo/draft").exists());
    assert_eq!(post_json(&mis, "/api/apps/demo/draft", "s", propose).await.0, 201);
    assert_eq!(app_get(&apps, "/a/demo/?draft=2", "").await.0, 303);
    assert_eq!(app_get(&apps, trial, cookie).await.0, 404);
    let (status, head, _) = app_get(&apps, "/a/demo/?draft=off", cookie).await;
    assert!(status == 303 && head.contains("Max-Age=0"), "{head}");
    std::fs::remove_dir_all(dir).unwrap();
}

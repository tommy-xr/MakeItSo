//! Fake worker + client drive the orchestrator end to end over real WebSockets.

use futures_util::{SinkExt, StreamExt};
use mis_orchestrator::App;
use mis_types::*;
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
    let OrchMsg::Welcome { id: got } = recv(&mut ws).await else {
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
    let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else {
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
            },
        )
        .await;
        let ServerMsg::Submitted { task } = recv(&mut client).await else {
            panic!()
        };
        let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else {
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
    // Voice: mic buttons on both composers, readback controls.
    assert_eq!(
        page.matches(r#"class="mic""#).count(),
        2,
        "mic on composer and reply"
    );
    assert!(page.contains(r#"id="autoread""#));
    assert!(page.contains(r#"id="speak""#));
    let info = http_get(&base, "/api/info").await;
    assert!(
        info.contains(r#"{"access":false,"auth":true,"version":"#),
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
}

const ACCESS_AUD: &str = "aud-tag";

/// Orchestrator behind (simulated) Cloudflare Access, with test keys injected.
async fn start_behind_access() -> String {
    let jwks = serde_json::from_str(include_str!("fixtures/access-test-jwks.json")).unwrap();
    let gate = mis_orchestrator::access_gate::AccessGate::with_keys(
        "team".into(),
        ACCESS_AUD.into(),
        jwks,
    );
    start_app(App::new(Some("s".into())).with_access(gate)).await
}

/// RS256 Access JWT signed by the fixture key, valid for an hour.
fn access_jwt(aud: &str) -> String {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut h = Header::new(Algorithm::RS256);
    h.kid = Some("test-kid".into());
    let claims = serde_json::json!({
        "aud": [aud], "iss": "https://team.cloudflareaccess.com",
        "exp": now + 3600, "iat": now, "email": "me@example.com",
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
        info.contains(r#"{"access":true,"auth":false,"version":"#),
        "{info}"
    );
    let info = http_get(&base, "/api/info").await;
    assert!(
        info.contains(r#"{"access":false,"auth":true,"version":"#),
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
        },
    )
    .await;
    let ServerMsg::Submitted { task: next } = recv(&mut client).await else {
        panic!()
    };
    let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else {
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
    let OrchMsg::Assign { task: assigned } = recv(&mut codex).await else {
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
    let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else {
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
    let OrchMsg::Assign { task: sent } = recv(&mut a).await else {
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
    let OrchMsg::Assign { task: sent } = recv(&mut worker).await else {
        panic!()
    };
    assert_eq!((sent.id, sent.resume.as_deref()), (task, None));
    assert!(sent.prompt.starts_with("now show it in a file\n"));
    assert!(sent
        .prompt
        .ends_with("Earlier request:\nwhat is 2+2?\n\nEarlier answer:\n4"));
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
        };
        send(&mut client, &submit).await;
        let ServerMsg::Submitted { task } = recv(&mut client).await else {
            panic!()
        };
        let OrchMsg::Assign { task: assigned } = recv(&mut worker).await else {
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

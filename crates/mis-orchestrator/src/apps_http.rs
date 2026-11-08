//! The apps listener (`--apps-bind`): serves each app's live release under `/a/<slug>/`
//! to its members. No orchestrator route exists here. Who may ask: [`crate::app_access`].

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use mis_types::{AppMe, Conflict, Export, Mutation, RecordQuery, Space, Written};

use crate::access_gate::{AccessGate, JWT_HEADER};
use crate::app_access::{self, Action, Member, Subject};
use crate::apps::{self, Apps};
use crate::records::{self, Refusal};
use crate::records_db::Records;

/// How the apps listener checks who is asking.
#[derive(Default)]
pub struct Auth {
    /// Verifies JWTs of the home Access app (its own AUD). None: only direct loopback
    /// requests get in.
    pub gate: Option<AccessGate>,
    /// [`app_access::public_authority`] of `--apps-public-url`: the one non-loopback Host
    /// answered.
    pub public_host: Option<String>,
}

/// Pages load scripts, styles and fonts only from, and connect only to, this origin. Every app
/// shares it (and its storage): apps are not isolated from each other.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; \
    base-uri 'none'; form-action 'self'";

/// The SDK every app may load from `/a/_/mis.js` (`_` is no app's slug), and its in-memory twin.
const SDK: &str = include_str!("../static/mis.js");
const SDK_MOCK: &str = include_str!("../static/mis-mock.mjs");

/// Needs [`ConnectInfo`]: serve with `into_make_service_with_connect_info::<SocketAddr>()`.
/// Each app's records are kept beside its releases ([`Records`]).
pub fn router(apps: Arc<Apps>, auth: Arc<Auth>) -> Router {
    let records = apps.records();
    Router::new()
        .route("/a/", get(directory))
        .route("/a/_/mis.js", get(|| async { js(SDK) }))
        .route("/a/_/mis-mock.js", get(|| async { js(SDK_MOCK) }))
        .route("/a/{slug}", get(add_slash))
        .route("/a/{slug}/", get(entry))
        .route("/a/{slug}/api/v1/me", get(me))
        .route(
            "/a/{slug}/api/v1/mutations",
            post(mutate).layer(axum::middleware::from_fn_with_state(
                auth.clone(),
                from_own_page,
            )),
        )
        .route("/a/{slug}/api/v1/records/{space}/{collection}", get(list))
        .route(
            "/a/{slug}/api/v1/records/{space}/{collection}/{id}",
            get(get_record),
        )
        .route("/a/{slug}/api/v1/export", get(export))
        .route("/a/{slug}/{*path}", get(file))
        .layer(Extension(records))
        .layer(axum::middleware::from_fn_with_state(
            (apps.clone(), auth),
            guard,
        ))
        .with_state(apps)
}

fn js(body: &'static str) -> Response {
    let ct = "text/javascript; charset=utf-8";
    ([(header::CONTENT_TYPE, ct)], body).into_response()
}

/// Every POST route's layer: see [`app_access::write_allowed`].
async fn from_own_page(State(auth): State<Arc<Auth>>, req: Request, next: Next) -> Response {
    let ok = {
        let h = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
        let (ct, origin, host) = (h(header::CONTENT_TYPE), h(header::ORIGIN), h(header::HOST));
        app_access::write_allowed(ct, origin, host, auth.public_host.as_deref())
    };
    if !ok {
        let why = "writes take JSON from this app's own page";
        return (StatusCode::FORBIDDEN, why).into_response();
    }
    next.run(req).await
}

/// Admits members only ([`admit`]), and sets the security headers on every response.
/// `no-cache`: app URLs aren't versioned, so an activation shows on reload.
async fn guard(
    State((apps, auth)): State<(Arc<Apps>, Arc<Auth>)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let mut resp = match admit(&apps, &auth, peer, &mut req).await {
        Ok(()) => next.run(req).await,
        Err(refused) => refused.into_response(),
    };
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // Other sites can't embed app files (members' cookies would ride along).
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    resp
}

/// Host check, then who is asking; under `/a/<slug>` also their role there. Puts a
/// [`Member`] (under an app) or the [`Subject`] (elsewhere) in the request's extensions.
async fn admit(
    apps: &Apps,
    auth: &Auth,
    peer: SocketAddr,
    req: &mut Request,
) -> Result<(), (StatusCode, &'static str)> {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !app_access::host_allowed(host, auth.public_host.as_deref()) {
        return Err((StatusCode::MISDIRECTED_REQUEST, "unknown host"));
    }
    let jwt = match (headers.get(JWT_HEADER), &auth.gate) {
        (None, _) => None,
        (Some(v), Some(gate)) => Some(match v.to_str() {
            Ok(jwt) => gate.verify(jwt).await.map(|id| id.subject).map_err(drop),
            Err(_) => Err(()),
        }),
        (Some(_), None) => Some(Err(())),
    };
    let loopback = peer.ip().to_canonical().is_loopback();
    let subject = app_access::identify(jwt, crate::via_cloudflare(headers), loopback).ok_or((
        StatusCode::UNAUTHORIZED,
        "sign in through Cloudflare Access",
    ))?;
    // The SDK is for anyone signed in.
    let sdk = matches!(req.uri().path(), "/a/_/mis.js" | "/a/_/mis-mock.js");
    let Some(slug) = app_access::app_of(req.uri().path()).filter(|_| !sdk) else {
        req.extensions_mut().insert(subject);
        return Ok(());
    };
    // Not shared with you and no such app look alike.
    let policy = apps::valid_slug(slug).then(|| apps.policy(slug));
    let role = app_access::role_of(&policy.unwrap_or_default(), &subject)
        .ok_or((StatusCode::FORBIDDEN, "this app isn't shared with you"))?;
    req.extensions_mut().insert(Member { subject, role });
    Ok(())
}

/// Links to every live app shared with the asker.
async fn directory(
    State(apps): State<Arc<Apps>>,
    Extension(subject): Extension<Subject>,
) -> Html<String> {
    let items: String = (apps.list().iter())
        .filter(|a| a.active.is_some())
        .filter(|a| app_access::role_of(&apps.policy(&a.slug), &subject).is_some())
        .map(|a| {
            format!(
                "<li><a href=\"/a/{}/\">{}</a></li>",
                a.slug,
                escape(&a.title)
            )
        })
        .collect();
    Html(format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
         <title>Apps</title><style>body{{font:16px system-ui;margin:2rem}}li{{margin:.5rem 0}}</style>\
         <h1>Apps</h1><ul>{items}</ul>"
    ))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `/a/pool` -> `/a/pool/`, so the app's relative URLs resolve inside it.
async fn add_slash(Path(slug): Path<String>) -> Response {
    if !apps::valid_slug(&slug) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Redirect::permanent(&format!("/a/{slug}/")).into_response()
}

/// Who is asking and their role, for the app's own UI (`GET` only, like all of `/api/v1`,
/// so clients that can only GET and POST work).
async fn me(
    State(apps): State<Arc<Apps>>,
    Extension(m): Extension<Member>,
    Path(slug): Path<String>,
) -> Response {
    if let Some(r) = denied(&apps, &slug, &m, Action::Read) {
        return r;
    }
    Json(AppMe {
        user: m.subject.name().into(),
        role: m.role,
        app: slug,
    })
    .into_response()
}

/// `f` off the async workers: SQLite and the app's files block.
async fn off_thread(f: impl FnOnce() -> Response + Send + 'static) -> Response {
    let done = tokio::task::spawn_blocking(f).await;
    done.unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Why `m` may not do `action` in `slug`: it isn't live, or their role doesn't allow it.
fn denied(apps: &Apps, slug: &str, m: &Member, action: Action) -> Option<Response> {
    // Checked again: the path's slug was, but this one is decoded (`a%2F..%2Fa`).
    if !apps::valid_slug(slug) || apps.live(slug).is_none() {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    if !app_access::allows(m.role, action) {
        let why = format!("a {} can't do that here", m.role.as_str());
        return Some((StatusCode::FORBIDDEN, why).into_response());
    }
    None
}

fn refused(r: Refusal) -> Response {
    match r {
        Refusal::Conflict(current) => {
            (StatusCode::CONFLICT, Json(Conflict { current })).into_response()
        }
        Refusal::TooLarge(why) => (StatusCode::PAYLOAD_TOO_LARGE, why).into_response(),
        Refusal::Invalid(why) => (StatusCode::BAD_REQUEST, why).into_response(),
    }
}

fn failed(slug: &str, e: anyhow::Error) -> Response {
    tracing::error!("apps/{slug} records: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, "records unavailable").into_response()
}

/// `POST /a/{slug}/api/v1/mutations`: a [`Mutation`]; 200 [`Written`], 409 [`Conflict`].
async fn mutate(
    State(apps): State<Arc<Apps>>,
    Extension(records): Extension<Arc<Records>>,
    Extension(m): Extension<Member>,
    Path(slug): Path<String>,
    body: Bytes,
) -> Response {
    off_thread(move || {
        if let Some(r) = denied(&apps, &slug, &m, Action::Write) {
            return r;
        }
        let op: Mutation = match serde_json::from_slice(&body) {
            Ok(op) => op,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("bad mutation: {e}")).into_response()
            }
        };
        match records.mutate(&slug, &op, m.subject.name(), crate::unix_ms()) {
            Ok(Ok(record)) => Json(Written { record }).into_response(),
            Ok(Err(r)) => refused(r),
            Err(e) => failed(&slug, e),
        }
    })
    .await
}

#[derive(serde::Deserialize)]
struct ListParams {
    /// A [`RecordQuery`] as JSON.
    q: Option<String>,
}

/// `GET /a/{slug}/api/v1/records/{space}/{collection}?q=`: a [`mis_types::Page`].
async fn list(
    State(apps): State<Arc<Apps>>,
    Extension(records): Extension<Arc<Records>>,
    Extension(m): Extension<Member>,
    Path((slug, space, collection)): Path<(String, Space, String)>,
    Query(params): Query<ListParams>,
) -> Response {
    off_thread(move || {
        if let Some(r) = denied(&apps, &slug, &m, Action::Read) {
            return r;
        }
        let q: RecordQuery = match params.q.as_deref().map(serde_json::from_str).transpose() {
            Ok(q) => q.unwrap_or_default(),
            Err(e) => return (StatusCode::BAD_REQUEST, format!("q: {e}")).into_response(),
        };
        let owner = records::owner(space, m.subject.name());
        let c = match records::compile(space, owner, &collection, &q) {
            Ok(c) => c,
            Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
        };
        match records.list(&slug, &q, &c) {
            Ok(page) => Json(page).into_response(),
            Err(e) => failed(&slug, e),
        }
    })
    .await
}

/// `GET /a/{slug}/api/v1/records/{space}/{collection}/{id}`: an [`mis_types::Envelope`].
async fn get_record(
    State(apps): State<Arc<Apps>>,
    Extension(records): Extension<Arc<Records>>,
    Extension(m): Extension<Member>,
    Path((slug, space, collection, id)): Path<(String, Space, String, String)>,
) -> Response {
    off_thread(move || {
        if let Some(r) = denied(&apps, &slug, &m, Action::Read) {
            return r;
        }
        if !records::valid_collection(&collection) || !records::valid_id(&id) {
            return (StatusCode::BAD_REQUEST, "bad collection or id").into_response();
        }
        let owner = records::owner(space, m.subject.name());
        match records.get(&slug, space, owner, &collection, &id) {
            Ok(Some(e)) => Json(e).into_response(),
            Ok(None) => (StatusCode::NOT_FOUND, "no such record").into_response(),
            Err(e) => failed(&slug, e),
        }
    })
    .await
}

/// `GET /a/{slug}/api/v1/export`: an [`Export`] of what the asker can see.
async fn export(
    State(apps): State<Arc<Apps>>,
    Extension(records): Extension<Arc<Records>>,
    Extension(m): Extension<Member>,
    Path(slug): Path<String>,
) -> Response {
    off_thread(move || {
        if let Some(r) = denied(&apps, &slug, &m, Action::Read) {
            return r;
        }
        let user = m.subject.name();
        match records.visible(&slug, user) {
            Ok((shared, me)) => Json(Export {
                version: 1,
                app: slug,
                user: user.into(),
                at: crate::unix_ms(),
                shared,
                me,
            })
            .into_response(),
            Err(e) => failed(&slug, e),
        }
    })
    .await
}

async fn entry(State(apps): State<Arc<Apps>>, Path(slug): Path<String>) -> Response {
    blocking(apps, slug, String::new()).await
}

async fn file(
    State(apps): State<Arc<Apps>>,
    Path((slug, path)): Path<(String, String)>,
) -> Response {
    blocking(apps, slug, path).await
}

/// [`serve`] off the async workers: reads may be large, and the orchestrator shares them.
async fn blocking(apps: Arc<Apps>, slug: String, path: String) -> Response {
    off_thread(move || serve(&apps, &slug, &path)).await
}

/// `path` (empty: the entry) from `slug`'s live release; an extensionless miss gets the entry.
fn serve(apps: &Apps, slug: &str, path: &str) -> Response {
    let live = apps::valid_slug(slug).then(|| apps.live(slug)).flatten();
    // `api/…` is the server's: unknown API paths 404 rather than get the entry page.
    let Some(live) = live.filter(|_| !apps::reserved(path)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let read = |rel: &'_ str| Some((rel.to_string(), apps::read_file(&live.dir, rel)?));
    let web = live.manifest.clients.web.as_str();
    let found = apps::clean_path(path.trim_end_matches('/'))
        .and_then(read)
        .or_else(|| {
            apps::wants_entry_fallback(path)
                .then(|| read(web))
                .flatten()
        });
    match found {
        Some((rel, bytes)) => {
            ([(header::CONTENT_TYPE, apps::content_type(&rel))], bytes).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

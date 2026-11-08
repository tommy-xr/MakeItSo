//! The apps listener (`--apps-bind`): serves each app's live release under `/a/<slug>/`
//! to its members. No orchestrator route exists here. Who may ask: [`crate::app_access`].

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, get};
use axum::{Extension, Json, Router};
use mis_types::AppMe;

use crate::access_gate::{AccessGate, JWT_HEADER};
use crate::app_access::{self, Member, Subject};
use crate::apps::{self, Apps};

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

/// Needs [`ConnectInfo`]: serve with `into_make_service_with_connect_info::<SocketAddr>()`.
pub fn router(apps: Arc<Apps>, auth: Arc<Auth>) -> Router {
    Router::new()
        .route("/a/", get(directory))
        .route("/a/{slug}", get(add_slash))
        .route("/a/{slug}/", get(entry))
        .route("/a/{slug}/api/v1/me", get(me))
        // The server's, not the bundle's: unknown API paths 404 rather than get the entry page.
        .route(
            "/a/{slug}/api/{*rest}",
            any(|| async { StatusCode::NOT_FOUND }),
        )
        .route("/a/{slug}/{*path}", get(file))
        .layer(axum::middleware::from_fn_with_state(
            (apps.clone(), auth),
            guard,
        ))
        .with_state(apps)
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
    let Some(slug) = app_access::app_of(req.uri().path()) else {
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
    if apps.live(&slug).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(AppMe {
        user: m.subject.name().into(),
        role: m.role,
        app: slug,
    })
    .into_response()
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
    let served = tokio::task::spawn_blocking(move || serve(&apps, &slug, &path));
    served.await.expect("serving doesn't panic")
}

/// `path` (empty: the entry) from `slug`'s live release; an extensionless miss gets the entry.
fn serve(apps: &Apps, slug: &str, path: &str) -> Response {
    let live = apps::valid_slug(slug).then(|| apps.live(slug)).flatten();
    let Some(live) = live else {
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

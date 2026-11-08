//! The apps listener (`--apps-bind`): serves each app's live release under `/a/<slug>/`.
//! No orchestrator route exists here. Until it checks who is asking, it refuses anything
//! that came through Cloudflare.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

use crate::apps::{self, Apps};

/// Apps load only their own files and reach only their own origin.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; \
    base-uri 'none'; form-action 'self'";

pub fn router(apps: Arc<Apps>) -> Router {
    Router::new()
        .route("/a/", get(directory))
        .route("/a/{slug}", get(add_slash))
        .route("/a/{slug}/", get(entry))
        .route("/a/{slug}/{*path}", get(file))
        .layer(axum::middleware::from_fn(guard))
        .with_state(apps)
}

/// Refuses Cloudflare traffic (no identity check yet), and sets the security headers on
/// every response. `no-cache`: app URLs aren't versioned, so an activation shows on reload.
async fn guard(req: Request, next: Next) -> Response {
    let mut resp = if crate::via_cloudflare(req.headers()) {
        let why = "apps aren't served through Cloudflare yet";
        (StatusCode::FORBIDDEN, why).into_response()
    } else {
        next.run(req).await
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

/// Links to every app with a live release.
async fn directory(State(apps): State<Arc<Apps>>) -> Html<String> {
    let items: String = (apps.list().iter())
        .filter(|a| a.active.is_some())
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

async fn entry(State(apps): State<Arc<Apps>>, Path(slug): Path<String>) -> Response {
    serve(&apps, &slug, "")
}

async fn file(
    State(apps): State<Arc<Apps>>,
    Path((slug, path)): Path<(String, String)>,
) -> Response {
    serve(&apps, &slug, &path)
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

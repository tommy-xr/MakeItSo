//! The apps listener (`--apps-bind`): serves each app's live release under `/a/<slug>/`
//! to its members. No orchestrator route exists here. Who may ask: [`crate::app_access`].

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use mis_types::{AppMe, BatchWritten, Conflict, Export, RecordQuery, Space, Written};

use crate::access_gate::{AccessGate, JWT_HEADER};
use crate::app_access::{self, Action, Member, Serve, Subject};
use crate::apps::{self, Apps};
use crate::records::{self, Refusal, Writes};
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
    /// `--apps-dev`: `local` requests may take a lesser role ([`app_access::DEV_ROLE`]).
    pub dev: bool,
}

/// Pages load scripts, styles and fonts only from, and connect only to, this origin. Every app
/// shares it (and its storage): apps are not isolated from each other.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; \
    base-uri 'none'; form-action 'self'";

/// The SDK every app may load from `/a/_/mis.js` (`_` is no app's slug), and its in-memory twin.
const SDK: &str = include_str!("../static/mis.js");
const SDK_MOCK: &str = include_str!("../static/mis-mock.mjs");
/// TypeScript types for both.
const SDK_TYPES: &str = include_str!("../static/mis.d.ts");

/// A full batch of the largest records, with room for keys and whitespace.
const MUTATIONS_BODY: usize = 8 << 20;

/// Needs [`ConnectInfo`]: serve with `into_make_service_with_connect_info::<SocketAddr>()`.
/// Each app's records are kept beside its releases ([`Records`]).
pub fn router(apps: Arc<Apps>, auth: Arc<Auth>) -> Router {
    let records = apps.records();
    Router::new()
        // The bare hostname and `/a` land on the directory.
        .route("/", get(|| async { Redirect::temporary("/a/") }))
        .route("/a", get(|| async { Redirect::temporary("/a/") }))
        .route("/a/", get(directory))
        .route("/a/_/mis.js", get(|| async { sdk_file(JS, SDK) }))
        .route("/a/_/mis-mock.js", get(|| async { sdk_file(JS, SDK_MOCK) }))
        .route("/a/_/mis.d.ts", get(|| async { sdk_file(TEXT, SDK_TYPES) }))
        .route(
            "/a/_/apps.webmanifest",
            get(|| async { sdk_file(MANIFEST, DIRECTORY_MANIFEST) }),
        )
        .route(
            "/a/_/apps-icon.svg",
            get(|| async { sdk_file(SVG, DIRECTORY_ICON) }),
        )
        .route(
            "/a/_/apps-touch-icon.png",
            get(|| async {
                ([(header::CONTENT_TYPE, "image/png")], DIRECTORY_TOUCH_ICON).into_response()
            }),
        )
        .route("/a/{slug}", get(add_slash))
        .route("/a/{slug}/", get(entry))
        .route("/a/{slug}/api/v1/me", get(me))
        .route(
            "/a/{slug}/api/v1/mutations",
            post(mutate)
                .layer(axum::extract::DefaultBodyLimit::max(MUTATIONS_BODY))
                .layer(axum::middleware::from_fn_with_state(
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

const JS: &str = "text/javascript; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";
const MANIFEST: &str = "application/manifest+json";
const SVG: &str = "image/svg+xml";

fn sdk_file(ct: &'static str, body: &'static str) -> Response {
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
    let mut stale = None;
    let mut resp = match admit(&apps, &auth, peer, &mut req).await {
        Ok(()) => {
            stale = stale_draft_cookie(&req);
            next.run(req).await
        }
        Err(refused) => refused.into_response(),
    };
    let h = resp.headers_mut();
    // Not over one the handler set (`?draft=<n>` entering the current draft).
    let set = h.contains_key(header::SET_COOKIE);
    if let Some(clear) = stale
        .filter(|_| !set)
        .and_then(|c| HeaderValue::from_str(&c).ok())
    {
        h.append(header::SET_COOKIE, clear);
    }
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

/// A `Set-Cookie` dropping the draft cookie a page or file request carries if it no longer
/// applies (the draft was replaced or decided, or the asker is a viewer).
fn stale_draft_cookie(req: &Request) -> Option<String> {
    req.extensions()
        .get::<Member>()
        .filter(|m| m.draft.is_none())?;
    let slug = app_access::app_of(req.uri().path())?;
    let cookies = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok());
    app_access::cookie_draft(cookies, slug)?;
    Some(app_access::draft_cookie(
        slug,
        None,
        crate::via_cloudflare(req.headers()),
    ))
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
    let dev_role = match headers.get(app_access::DEV_ROLE) {
        Some(v) if auth.dev && subject == Subject::Local => Some(
            v.to_str()
                .ok()
                .and_then(app_access::dev_role)
                .ok_or((StatusCode::BAD_REQUEST, "x-mis-dev-role: viewer or editor"))?,
        ),
        _ => None,
    };
    // The SDK and the directory's own files are for anyone signed in.
    let sdk = matches!(
        req.uri().path(),
        "/a/_/mis.js"
            | "/a/_/mis-mock.js"
            | "/a/_/mis.d.ts"
            | "/a/_/apps.webmanifest"
            | "/a/_/apps-icon.svg"
            | "/a/_/apps-touch-icon.png"
    );
    let Some(slug) = app_access::app_of(req.uri().path()).filter(|_| !sdk) else {
        req.extensions_mut().insert(subject);
        return Ok(());
    };
    // Not shared with you and no such app look alike.
    let policy = apps::valid_slug(slug).then(|| apps.policy(slug));
    let role = app_access::role_of(&policy.unwrap_or_default(), &subject)
        .ok_or((StatusCode::FORBIDDEN, "this app isn't shared with you"))?;
    let role = dev_role.unwrap_or(role);
    // A page goes by its cookie, an API call by the header its page's `mis.js` sends.
    let h = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
    let api = apps::reserved(req.uri().path().splitn(4, '/').nth(3).unwrap_or(""));
    let asked = match api {
        true => h(app_access::DRAFT_HEADER).map(|v| v.parse().unwrap_or(0)),
        false => app_access::cookie_draft(h(header::COOKIE.as_str()), slug),
    };
    // Read only when a draft is asked for.
    let current = asked.and_then(|_| apps.draft(slug)).map(|d| d.release);
    let draft = match app_access::draft_of(asked, api, role, current) {
        Serve::Live => None,
        Serve::Draft(n) => Some(n),
        Serve::Gone => {
            let why = "this draft was published, discarded or replaced: reload the page";
            return Err((StatusCode::CONFLICT, why));
        }
    };
    req.extensions_mut().insert(Member {
        subject,
        role,
        draft,
    });
    Ok(())
}

/// Lists every live app shared with the asker.
async fn directory(
    State(apps): State<Arc<Apps>>,
    Extension(subject): Extension<Subject>,
) -> Response {
    off_thread(move || {
        Html(directory_page(subject.name(), &tiles(&apps, &subject))).into_response()
    })
    .await
}

/// The live apps `subject` has a role in. Reads each app's policy and live manifest.
fn tiles(apps: &Apps, subject: &Subject) -> Vec<Tile> {
    (apps.list().iter())
        .filter(|a| a.active.is_some())
        .filter(|a| app_access::role_of(&apps.policy(&a.slug), subject).is_some())
        .map(|a| {
            let icon = apps.live(&a.slug).and_then(|l| l.manifest.icon);
            Tile {
                slug: a.slug.clone(),
                title: a.title.clone(),
                icon: icon.map(|i| format!("/a/{}/{}", a.slug, url_path(&i))),
            }
        })
        .collect()
}

/// One app in the directory. `icon`: its URL, served from the live release.
struct Tile {
    slug: String,
    title: String,
    icon: Option<String>,
}

const DIRECTORY_CSS: &str = include_str!("../static/apps.css");
/// Makes the directory pinnable on its own: `/a/` in a standalone window.
const DIRECTORY_MANIFEST: &str = r##"{"name":"Apps","short_name":"Apps","start_url":"/a/","scope":"/a/",
"display":"standalone","background_color":"#0E1217","theme_color":"#0E1217","icons":[
{"src":"/a/_/apps-icon.svg","sizes":"any","type":"image/svg+xml"},
{"src":"/a/_/apps-touch-icon.png","sizes":"180x180","type":"image/png"}]}
"##;
const DIRECTORY_ICON: &str = include_str!("../static/apps-icon.svg");
const DIRECTORY_TOUCH_ICON: &[u8] = include_bytes!("../static/apps-touch-icon.png");

/// The directory: `who` is signed in and may open `tiles`. Static: no script.
fn directory_page(who: &str, tiles: &[Tile]) -> String {
    let rows: String = tiles
        .iter()
        .map(|t| {
            let icon = match &t.icon {
                Some(src) => format!(r#"<img class="icon" src="{}" alt="">"#, escape(src)),
                // The title's first letter: "garden" -> "G".
                None => {
                    let letter = t
                        .title
                        .chars()
                        .next()
                        .map_or("?".into(), |c| c.to_uppercase().to_string());
                    format!(
                        r#"<span class="icon" aria-hidden="true">{}</span>"#,
                        escape(&letter)
                    )
                }
            };
            format!(
                r#"<li><a href="/a/{}/">{icon}<span class="name">{}</span></a></li>"#,
                escape(&t.slug),
                escape(&t.title)
            )
        })
        .collect();
    let body = if rows.is_empty() {
        r#"<p class="empty">No apps are shared with you yet.</p>"#.to_string()
    } else {
        format!("<ul>{rows}</ul>")
    };
    format!(
        r##"<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="theme-color" content="#0E1217" media="(prefers-color-scheme: dark)">
<meta name="theme-color" content="#EAF3F7" media="(prefers-color-scheme: light)">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
<meta name="apple-mobile-web-app-title" content="Apps">
<link rel="manifest" href="/a/_/apps.webmanifest" crossorigin="use-credentials">
<link rel="icon" href="/a/_/apps-icon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/a/_/apps-touch-icon.png">
<title>Apps</title>
<style>{DIRECTORY_CSS}</style>
<header><svg class="logo" viewBox="0 0 18 18" aria-hidden="true"><rect x="1" y="3" width="6" height="12" rx="1" fill="var(--accent)"/><path d="M9 3h5l2 2-2 2H9z" fill="var(--accent)"/><path d="M9 8h5l2 2-2 2H9z" fill="#F2A65E"/></svg><h1>Apps</h1><span class="who">{}</span></header>
<main>{body}</main>
"##,
        escape(who)
    )
}

/// `path` for a URL path: bytes outside `A-Za-z0-9-._~/` percent-encoded. `my icon.png` -> `my%20icon.png`.
fn url_path(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
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
        drafted: (app_access::may_try(m.role))
            .then(|| apps.draft(&slug).map(|d| d.release))
            .flatten(),
        app: slug,
        draft: m.draft,
    })
    .into_response()
}

/// `f` off the async workers: SQLite and the app's files block.
async fn off_thread(f: impl FnOnce() -> Response + Send + 'static) -> Response {
    let done = tokio::task::spawn_blocking(f).await;
    done.unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The release `m` is served: the draft they try, else the live one.
fn serving(apps: &Apps, slug: &str, m: &Member) -> Option<apps::Live> {
    match m.draft {
        Some(n) => apps.release_at(slug, n),
        None => apps.live(slug),
    }
}

/// `f` on the records `m` reads and writes: the draft's copy while trying it (held so the
/// draft can't end mid-call), else the app's.
fn with_store<T>(
    apps: &Apps,
    slug: &str,
    m: &Member,
    f: impl FnOnce(&str) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    match m.draft {
        Some(n) => apps.in_draft(slug, n, f),
        None => f(slug),
    }
}

/// Why `m` may not do `action` in `slug`: it isn't served, or their role doesn't allow it.
fn denied(apps: &Apps, slug: &str, m: &Member, action: Action) -> Option<Response> {
    // Checked again: the path's slug was, but this one is decoded (`a%2F..%2Fa`).
    if !apps::valid_slug(slug) || serving(apps, slug, m).is_none() {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    if !app_access::allows(m.role, action) {
        let why = format!("a {} can't do that here", m.role.as_str());
        return Some((StatusCode::FORBIDDEN, why).into_response());
    }
    None
}

/// `index`: the refused op of a batch; other refusals say it as `ops[2]: …`.
fn refused(r: Refusal, index: Option<usize>) -> Response {
    let at = |why: String| match index {
        Some(i) => format!("ops[{i}]: {why}"),
        None => why,
    };
    match r {
        Refusal::Conflict(current) => {
            (StatusCode::CONFLICT, Json(Conflict { current, index })).into_response()
        }
        Refusal::TooLarge(why) => (StatusCode::PAYLOAD_TOO_LARGE, at(why)).into_response(),
        Refusal::Invalid(why) => (StatusCode::BAD_REQUEST, at(why)).into_response(),
    }
}

fn failed(slug: &str, e: anyhow::Error) -> Response {
    tracing::error!("apps/{slug} records: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, "records unavailable").into_response()
}

/// `POST /a/{slug}/api/v1/mutations`: a [`mis_types::Mutation`] (200 [`Written`]) or a
/// [`mis_types::Batch`] (200 [`BatchWritten`]); 409 [`Conflict`].
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
        let (who, now) = (m.subject.name(), crate::unix_ms());
        match records::parse_writes(&body) {
            Err(why) => (StatusCode::BAD_REQUEST, why).into_response(),
            Ok(Writes::One(op)) => {
                match with_store(&apps, &slug, &m, |key| records.mutate(key, &op, who, now)) {
                    Ok(Ok(record)) => Json(Written { record }).into_response(),
                    Ok(Err(r)) => refused(r, None),
                    Err(e) => failed(&slug, e),
                }
            }
            Ok(Writes::Batch(ops)) => match with_store(&apps, &slug, &m, |key| {
                records.mutate_all(key, &ops, who, now)
            }) {
                Ok(Ok(records)) => Json(BatchWritten { records }).into_response(),
                Ok(Err((i, r))) => refused(r, Some(i)),
                Err(e) => failed(&slug, e),
            },
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
        match with_store(&apps, &slug, &m, |key| records.list(key, &q, &c)) {
            Ok(page) => Json(page).into_response(),
            Err(e) => failed(&slug, e),
        }
    })
    .await
}

#[derive(serde::Deserialize)]
struct GetParams {
    missing: Option<Missing>,
}

/// `?missing=null`: answer a missing record with 200 `null`, not 404 (which browsers log).
#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Missing {
    Null,
}

/// `GET /a/{slug}/api/v1/records/{space}/{collection}/{id}[?missing=null]`: an
/// [`mis_types::Envelope`].
async fn get_record(
    State(apps): State<Arc<Apps>>,
    Extension(records): Extension<Arc<Records>>,
    Extension(m): Extension<Member>,
    Path((slug, space, collection, id)): Path<(String, Space, String, String)>,
    Query(params): Query<GetParams>,
) -> Response {
    off_thread(move || {
        if let Some(r) = denied(&apps, &slug, &m, Action::Read) {
            return r;
        }
        if !records::valid_collection(&collection) || !records::valid_id(&id) {
            return (StatusCode::BAD_REQUEST, "bad collection or id").into_response();
        }
        let owner = records::owner(space, m.subject.name());
        match with_store(&apps, &slug, &m, |key| {
            records.get(key, space, owner, &collection, &id)
        }) {
            Ok(Some(e)) => Json(e).into_response(),
            Ok(None) if params.missing.is_some() => Json(serde_json::Value::Null).into_response(),
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
        match with_store(&apps, &slug, &m, |key| records.visible(key, user)) {
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

#[derive(serde::Deserialize)]
struct EntryParams {
    /// A draft's release to try, or `off`.
    draft: Option<String>,
}

/// The app's entry page. `?draft=<n>`: an owner or editor tries draft n (its release, on a copy of the
/// records); `?draft=off` leaves it. Either sets a cookie, then goes back to the bare URL.
async fn entry(
    State(apps): State<Arc<Apps>>,
    Extension(m): Extension<Member>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(params): Query<EntryParams>,
) -> Response {
    let Some(want) = params.draft else {
        return blocking(apps, slug, String::new(), m).await;
    };
    let secure = crate::via_cloudflare(&headers);
    off_thread(move || {
        let n = match want.as_str() {
            "off" => None,
            n => match n.parse::<u32>() {
                Ok(n) => Some(n),
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "draft=<n> or draft=off").into_response()
                }
            },
        };
        if let Some(n) = n {
            let current = apps::valid_slug(&slug).then(|| apps.draft(&slug)).flatten();
            let current = current.map(|d| d.release);
            if app_access::draft_of(Some(n), true, m.role, current) != Serve::Draft(n) {
                return (StatusCode::NOT_FOUND, "no such draft").into_response();
            }
            // Its copy of the records is taken now, from the app's as they are.
            if let Err(e) = apps.in_draft(&slug, n, |_| Ok(())) {
                return failed(&slug, e);
            }
        }
        let cookie = app_access::draft_cookie(&slug, n, secure);
        let back = format!("/a/{slug}/");
        (
            StatusCode::SEE_OTHER,
            [(header::SET_COOKIE, cookie), (header::LOCATION, back)],
        )
            .into_response()
    })
    .await
}

async fn file(
    State(apps): State<Arc<Apps>>,
    Extension(m): Extension<Member>,
    Path((slug, path)): Path<(String, String)>,
) -> Response {
    blocking(apps, slug, path, m).await
}

/// [`serve`] off the async workers: reads may be large, and the orchestrator shares them.
async fn blocking(apps: Arc<Apps>, slug: String, path: String, m: Member) -> Response {
    off_thread(move || serve(&apps, &slug, &path, &m)).await
}

/// `path` (empty: the entry) from the release `m` is served; an extensionless miss gets the entry.
fn serve(apps: &Apps, slug: &str, path: &str, m: &Member) -> Response {
    let live = apps::valid_slug(slug)
        .then(|| serving(apps, slug, m))
        .flatten();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(slug: &str, title: &str, icon: Option<&str>) -> Tile {
        Tile {
            slug: slug.into(),
            title: title.into(),
            icon: icon.map(Into::into),
        }
    }

    #[test]
    fn directory_rows_link_each_app_with_its_icon_or_letter() {
        let page = directory_page(
            "me@example.com",
            &[
                tile("garden", "Garden", Some("/a/garden/icon.png")),
                tile("notes", "notes", None),
            ],
        );
        assert!(page.contains(r#"<a href="/a/garden/"><img class="icon" src="/a/garden/icon.png" alt=""><span class="name">Garden</span></a>"#), "{page}");
        assert!(page.contains(r#"<a href="/a/notes/"><span class="icon" aria-hidden="true">N</span><span class="name">notes</span></a>"#), "{page}");
        assert!(page.contains(r#"<span class="who">me@example.com</span>"#));
        assert!(!page.contains("No apps are shared"));
        assert!(!page.contains("<script"));
    }

    #[test]
    fn directory_escapes_titles_icons_and_who() {
        let page = directory_page(
            "<b>@example.com",
            &[tile("x", "<Tom & \"Jerry\">", Some("/a/x/\"><q"))],
        );
        assert!(page.contains("&lt;b&gt;@example.com"));
        assert!(page.contains("&lt;Tom &amp; &quot;Jerry&quot;&gt;"));
        assert!(page.contains(r#"src="/a/x/&quot;&gt;&lt;q""#));
        assert!(!page.contains("<b>") && !page.contains("<q"));
        // A lettered tile escapes its letter too.
        let page = directory_page("me", &[tile("x", "<x", None)]);
        assert!(page.contains(r#"aria-hidden="true">&lt;</span>"#), "{page}");
    }

    #[test]
    fn directory_with_no_apps_says_so() {
        let page = directory_page("me", &[]);
        assert!(page.contains(r#"<p class="empty">No apps are shared with you yet.</p>"#));
        assert!(!page.contains("<ul>"));
    }

    #[test]
    fn icon_paths_are_percent_encoded() {
        assert_eq!(url_path("img/icon-1_a.~.png"), "img/icon-1_a.~.png");
        assert_eq!(url_path("my icon#?.png"), "my%20icon%23%3F.png");
        assert_eq!(url_path("é.png"), "%C3%A9.png");
    }
}

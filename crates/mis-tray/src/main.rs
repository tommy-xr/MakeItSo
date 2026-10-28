//! Menu bar / tray app: the fleet at a glance, links into the web page.
//! Shell around `model`: a network thread feeds statuses to the tray's event
//! loop (`tray`, macOS and Windows), which renders each `MenuModel`.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod model;
#[cfg(any(target_os = "macos", windows))]
mod tray;

use std::time::Duration;

use anyhow::{bail, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use mis_types::{access_headers, http_url, ClientMsg, ServerMsg};
use model::{menu_model, Fleet};
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

#[derive(Parser)]
#[command(name = "mis-tray", about = "Make It So menu bar app", version)]
struct Args {
    /// Defaults to MIS_BIND in ~/.config/mis/orchestrator.env, else ws://127.0.0.1:7474.
    #[arg(long, env = "MIS_URL")]
    url: Option<String>,
    /// Defaults to MIS_TOKEN in ~/.config/mis/orchestrator.env when --url is unset.
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    #[arg(long, env = "CF_ACCESS_CLIENT_ID")]
    cf_access_client_id: Option<String>,
    #[arg(long, env = "CF_ACCESS_CLIENT_SECRET", hide_env_values = true)]
    cf_access_client_secret: Option<String>,
    /// Print the menu once as text and exit, instead of showing the tray icon.
    #[arg(long)]
    print: bool,
}

/// Where and how to connect.
#[derive(Clone)]
pub struct Conn {
    url: String,
    headers: Vec<(&'static str, String)>,
}

/// Ask for status at most once a second while events arrive, and every 15 s regardless.
const THROTTLE: Duration = Duration::from_secs(1);
const REFRESH_TICKS: u32 = 15;

/// One connection: subscribe, call `on_status` with every status, until it drops.
pub async fn session(conn: &Conn, on_status: impl Fn(Fleet)) -> Result<()> {
    let mut req = format!("{}/ws/client", conn.url).into_client_request()?;
    for (k, v) in &conn.headers {
        req.headers_mut().insert(*k, v.parse()?);
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    for m in [ClientMsg::Subscribe, ClientMsg::Status] {
        ws.send(Message::Text(serde_json::to_string(&m)?.into()))
            .await?;
    }
    let mut tick = interval_at(Instant::now() + THROTTLE, THROTTLE);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let (mut dirty, mut ticks) = (false, 0);
    loop {
        tokio::select! {
            m = ws.next() => match m {
                // Skip messages this build doesn't know rather than reconnect.
                Some(Ok(Message::Text(t))) => match serde_json::from_str(&t) {
                    Ok(ServerMsg::Status { workers, tasks, now_ms }) => {
                        let now_ms = now_ms.unwrap_or_else(unix_ms);
                        on_status(Fleet { workers, tasks, now_ms });
                    }
                    Ok(ServerMsg::Event { .. }) => dirty = true,
                    // E.g. "dropped N events; re-run status".
                    Ok(ServerMsg::Error { message }) => {
                        eprintln!("mis-tray: {message}");
                        dirty = true;
                    }
                    _ => {}
                },
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => bail!("connection closed"),
            },
            _ = tick.tick() => {
                ticks += 1;
                if dirty || ticks >= REFRESH_TICKS {
                    let m = serde_json::to_string(&ClientMsg::Status)?;
                    ws.send(Message::Text(m.into())).await?;
                    (dirty, ticks) = (false, 0);
                }
            }
        }
    }
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Flags and env first; the orchestrator.env URL and token only together, so
/// the local token never goes to a `--url` elsewhere.
fn conn(args: &Args) -> Result<Conn> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    let file = home
        .ok()
        .and_then(|h| std::fs::read_to_string(format!("{h}/.config/mis/orchestrator.env")).ok())
        .map(|t| model::parse_env(&t))
        .unwrap_or_default();
    let (url, token) = match args.url.clone().filter(|u| !u.is_empty()) {
        Some(u) => (u, args.token.clone()),
        None => (
            model::local_url(&file),
            args.token
                .clone()
                .or_else(|| file.get("MIS_TOKEN").cloned()),
        ),
    };
    let mut headers = access_headers(
        args.cf_access_client_id.as_deref(),
        args.cf_access_client_secret.as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        headers.push(("authorization", format!("Bearer {t}")));
    }
    Ok(Conn { url, headers })
}

fn main() -> Result<()> {
    let args = Args::parse();
    let conn = conn(&args)?;
    let page = http_url(&conn.url);
    eprintln!("mis-tray {}: {page}", env!("CARGO_PKG_VERSION"));
    if args.print {
        let print = |f: Fleet| {
            print!("{}", model::text(&menu_model(Some(&f), &page)));
            std::process::exit(0)
        };
        return runtime().block_on(session(&conn, print));
    }
    #[cfg(any(target_os = "macos", windows))]
    tray::run(conn, page);
    #[cfg(not(any(target_os = "macos", windows)))]
    bail!("the tray icon needs macOS or Windows; --print works here")
}

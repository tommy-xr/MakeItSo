//! Menu bar / tray app: the fleet at a glance, links into the web page.
//! Shell around `model`: a network thread feeds statuses to the tao event loop,
//! which renders each `MenuModel` with tray-icon and handles clicks.

mod model;

use std::time::Duration;

use anyhow::{bail, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use mis_types::{access_headers, http_url, ClientMsg, ServerMsg};
use model::{menu_model, Fleet, Item, QUIT};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Parser)]
#[command(name = "mis-tray", about = "Make It So menu bar app", version)]
struct Args {
    /// Defaults to the port in ~/.config/mis/orchestrator.env's MIS_BIND, else 7474.
    #[arg(long, env = "MIS_URL")]
    url: Option<String>,
    /// Defaults to MIS_TOKEN in ~/.config/mis/orchestrator.env.
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

enum UserEvent {
    Status(Option<Fleet>),
    Menu(MenuEvent),
}

/// Ask for status at most once a second while events arrive, and every 15 s regardless.
const THROTTLE: Duration = Duration::from_secs(1);
const REFRESH_TICKS: u32 = 15;
const RETRY: Duration = Duration::from_secs(3);

/// One connection: subscribe, call `on_status` with every status, until it drops.
async fn session(args: &Args, url: &str, on_status: impl Fn(Fleet)) -> Result<()> {
    let mut req = format!("{url}/ws/client").into_client_request()?;
    if let Some(t) = &args.token {
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse()?);
    }
    let access = access_headers(
        args.cf_access_client_id.as_deref(),
        args.cf_access_client_secret.as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    for (k, v) in access {
        req.headers_mut().insert(k, v.parse()?);
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    for m in [ClientMsg::Subscribe, ClientMsg::Status] {
        ws.send(Message::Text(serde_json::to_string(&m)?.into()))
            .await?;
    }
    let mut tick = tokio::time::interval(THROTTLE);
    let (mut dirty, mut ticks) = (false, 0);
    loop {
        tokio::select! {
            m = ws.next() => match m {
                Some(Ok(Message::Text(t))) => match serde_json::from_str(&t)? {
                    ServerMsg::Status { workers, tasks, now_ms } => {
                        let now_ms = now_ms.unwrap_or_else(unix_ms);
                        on_status(Fleet { workers, tasks, now_ms });
                    }
                    ServerMsg::Event { .. } => dirty = true,
                    ServerMsg::Error { message } => eprintln!("mis-tray: {message}"),
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

/// Reconnects forever; the menu shows "Disconnected" while down.
fn network(args: Args, url: String, proxy: EventLoopProxy<UserEvent>) {
    runtime().block_on(async {
        loop {
            let on_status = |f| {
                let _ = proxy.send_event(UserEvent::Status(Some(f)));
            };
            let err = session(&args, &url, on_status).await.unwrap_err();
            eprintln!("mis-tray: {err}; retrying in {}s", RETRY.as_secs());
            let _ = proxy.send_event(UserEvent::Status(None));
            tokio::time::sleep(RETRY).await;
        }
    });
}

fn runtime() -> tokio::runtime::Runtime {
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

/// Monochrome template image: three bars and a block cursor (icons/tray.svg).
fn icon() -> Icon {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../icons/tray.png")));
    let mut reader = decoder.read_info().expect("tray.png");
    let mut rgba = vec![0; reader.output_buffer_size().expect("tray.png size")];
    let info = reader.next_frame(&mut rgba).expect("tray.png");
    Icon::from_rgba(rgba, info.width, info.height).expect("tray.png")
}

fn render(tray: &TrayIcon, fleet: Option<&Fleet>, page: &str) {
    let m = menu_model(fleet, page);
    let menu = Menu::new();
    for item in m.items {
        let r = match item {
            Item::Label(l) => menu.append(&MenuItem::new(l, false, None)),
            Item::Separator => menu.append(&PredefinedMenuItem::separator()),
            Item::Open { label, url } => menu.append(&MenuItem::with_id(url, label, true, None)),
            Item::Quit => menu.append(&MenuItem::with_id(QUIT, "Quit", true, None)),
        };
        r.expect("menu append");
    }
    tray.set_menu(Some(Box::new(menu)));
    tray.set_title((!m.title.is_empty()).then_some(m.title));
}

fn open(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    if let Err(e) = cmd.arg(url).spawn() {
        eprintln!("mis-tray: opening {url}: {e}");
    }
}

fn main() {
    let mut args = Args::parse();
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    let file = home
        .ok()
        .and_then(|h| std::fs::read_to_string(format!("{h}/.config/mis/orchestrator.env")).ok())
        .map(|t| model::parse_env(&t))
        .unwrap_or_default();
    let url =
        (args.url.clone().filter(|u| !u.is_empty())).unwrap_or_else(|| model::local_url(&file));
    args.token = args.token.or_else(|| file.get("MIS_TOKEN").cloned());
    args.token = args.token.filter(|t| !t.is_empty());
    let page = http_url(&url);
    eprintln!("mis-tray {}: {page}", env!("CARGO_PKG_VERSION"));
    if args.print {
        let print = |f: Fleet| {
            print!("{}", model::text(&menu_model(Some(&f), &page)));
            std::process::exit(0)
        };
        if let Err(e) = runtime().block_on(session(&args, &url, print)) {
            eprintln!("mis-tray: {e}");
            std::process::exit(1)
        }
    }

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory); // no Dock icon
    }
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || network(args, url, proxy));

    let mut tray = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // macOS needs the loop running before the status item is created.
            Event::NewEvents(StartCause::Init) => {
                let t = TrayIconBuilder::new()
                    .with_icon_templated(icon())
                    .with_tooltip("Make It So")
                    .build()
                    .expect("tray icon");
                render(&t, None, &page);
                tray = Some(t);
            }
            Event::UserEvent(UserEvent::Status(fleet)) => {
                if let Some(t) = &tray {
                    render(t, fleet.as_ref(), &page);
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) if e.id.0 == QUIT => {
                *control_flow = ControlFlow::Exit
            }
            Event::UserEvent(UserEvent::Menu(e)) => open(&e.id.0),
            _ => {}
        }
    });
}

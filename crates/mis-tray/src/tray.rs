//! The tray icon and its event loop: renders `MenuModel`s, dispatches clicks.

use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{
    IsMenuItem, Menu, MenuEvent, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::model::{find, menu_model, same_shape, Fleet, Item, MenuModel};
use crate::{runtime, session, Conn};

enum UserEvent {
    Status(Option<Fleet>),
    Menu(MenuEvent),
}

const RETRY: Duration = Duration::from_secs(3);

/// Reconnects forever; `Status(None)` shows "Disconnected" while down.
fn network(conn: Conn, proxy: EventLoopProxy<UserEvent>) {
    runtime().block_on(async {
        loop {
            let on_status = |f| {
                let _ = proxy.send_event(UserEvent::Status(Some(f)));
            };
            let err = session(&conn, on_status).await.unwrap_err();
            eprintln!("mis-tray: {err}; retrying in {}s", RETRY.as_secs());
            let _ = proxy.send_event(UserEvent::Status(None));
            tokio::time::sleep(RETRY).await;
        }
    });
}

/// Monochrome template image of the favicon: block cursor, three arrow bars (icons/tray.svg,
/// rendered with `sips -s format png tray.svg --out tray.png`).
fn icon() -> Icon {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../icons/tray.png")));
    let mut reader = decoder.read_info().expect("tray.png");
    let mut rgba = vec![0; reader.output_buffer_size().expect("tray.png size")];
    let info = reader.next_frame(&mut rgba).expect("tray.png");
    Icon::from_rgba(rgba, info.width, info.height).expect("tray.png")
}

/// Updates `menu` in place: replacing or dropping it would close it while open
/// (macOS). Item ids are paths into the model (`7.2`), so clicks look up `model`.
fn render(tray: &TrayIcon, menu: &Menu, old: Option<&MenuModel>, new: &MenuModel) {
    if old.is_some_and(|o| same_shape(o, new)) {
        relabel(menu.items(), &new.items);
    } else {
        while menu.remove_at(0).is_some() {}
        build(&new.items, "", &|i| menu.append(i).expect("menu append"));
    }
    tray.set_title((!new.title.is_empty()).then_some(&new.title));
}

fn relabel(kinds: Vec<MenuItemKind>, items: &[Item]) {
    for (kind, item) in kinds.into_iter().zip(items) {
        match (kind, item) {
            (MenuItemKind::MenuItem(m), Item::Label(l) | Item::Open { label: l, .. }) => {
                m.set_text(l)
            }
            (MenuItemKind::Submenu(m), Item::Submenu { label, items }) => {
                m.set_text(label);
                relabel(m.items(), items);
            }
            _ => {}
        }
    }
}

/// Appends `items` with `add`; ids are `prefix` + index.
fn build(items: &[Item], prefix: &str, add: &dyn Fn(&dyn IsMenuItem)) {
    for (i, item) in items.iter().enumerate() {
        let id = format!("{prefix}{i}");
        match item {
            Item::Label(l) => add(&MenuItem::new(l, false, None)),
            Item::Separator => add(&PredefinedMenuItem::separator()),
            Item::Open { label, .. } => add(&MenuItem::with_id(id, label, true, None)),
            Item::Submenu { label, items } => {
                let sub = Submenu::new(label, true);
                build(items, &format!("{id}."), &|i| {
                    sub.append(i).expect("menu append")
                });
                add(&sub);
            }
            Item::Quit => add(&MenuItem::with_id(id, "Quit", true, None)),
        }
    }
}

fn open(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("open")
    };
    match cmd.arg(url).spawn() {
        // Reap it so finished launchers don't pile up as zombies.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => eprintln!("mis-tray: opening {url}: {e}"),
    }
}

pub fn run(conn: Conn, page: String) -> ! {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    let event_loop = {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        let mut e = event_loop;
        e.set_activation_policy(ActivationPolicy::Accessory); // no Dock icon
        e
    };
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || network(conn, proxy));

    let menu = Menu::new();
    let mut tray = None;
    let mut model = menu_model(None, &page);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // macOS needs the loop running before the status item is created.
            Event::NewEvents(StartCause::Init) => {
                let t = TrayIconBuilder::new()
                    .with_tooltip("Make It So")
                    .with_menu(Box::new(menu.clone()));
                // A template image follows the macOS menu bar's light/dark tint.
                #[cfg(target_os = "macos")]
                let t = t.with_icon_templated(icon());
                #[cfg(windows)]
                let t = t.with_icon(icon());
                let t = t.build().expect("tray icon");
                render(&t, &menu, None, &model);
                tray = Some(t);
            }
            Event::UserEvent(UserEvent::Status(fleet)) => {
                let new = menu_model(fleet.as_ref(), &page);
                if let Some(t) = &tray {
                    render(t, &menu, Some(&model), &new);
                }
                model = new;
            }
            Event::UserEvent(UserEvent::Menu(e)) => match find(&model.items, &e.id.0) {
                Some(Item::Open { url, .. }) => open(url),
                Some(Item::Quit) => *control_flow = ControlFlow::Exit,
                _ => {}
            },
            _ => {}
        }
    })
}

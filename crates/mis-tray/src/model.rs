//! Pure core: fleet status -> menu contents, and connection config.

use std::collections::HashMap;

use mis_types::*;

/// Tasks listed in the menu, newest first.
pub const MAX_TASKS: usize = 8;

/// Latest `ServerMsg::Status`.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    pub workers: Vec<WorkerSummary>,
    pub tasks: Vec<TaskSummary>,
    pub now_ms: u64,
}

#[derive(Debug, PartialEq)]
pub enum Item {
    /// Disabled text.
    Label(String),
    Separator,
    Open {
        label: String,
        url: String,
    },
    Quit,
}

#[derive(Debug, PartialEq)]
pub struct MenuModel {
    /// Text beside the icon (macOS): the running count, or empty.
    pub title: String,
    pub items: Vec<Item>,
}

/// `fleet` None = socket down. `page` is the orchestrator's http URL.
pub fn menu_model(fleet: Option<&Fleet>, page: &str) -> MenuModel {
    let page = page.trim_end_matches('/');
    let mut items = vec![];
    let mut title = String::new();
    match fleet {
        None => items.push(Item::Label("Disconnected · retrying".into())),
        Some(f) => {
            items.push(Item::Label(headline(&f.workers, &f.tasks)));
            let running = count(&f.tasks, |s| s == TaskStatus::Running);
            if running > 0 {
                title = running.to_string();
            }
            if !f.tasks.is_empty() {
                items.push(Item::Separator);
            }
            // The orchestrator lists tasks in submission order.
            for t in f.tasks.iter().rev().take(MAX_TASKS) {
                items.push(Item::Open {
                    label: task_label(t, f.now_ms),
                    url: format!("{page}/#task/{}", t.id),
                });
            }
        }
    }
    items.push(Item::Separator);
    items.push(Item::Open {
        label: "Open Make It So".into(),
        url: format!("{page}/"),
    });
    items.push(Item::Quit);
    MenuModel { title, items }
}

/// The menu as text, one item per line, for `--print`.
pub fn text(m: &MenuModel) -> String {
    let mut out = format!("title: {}\n", m.title);
    for i in &m.items {
        match i {
            Item::Label(l) => out += &format!("{l}\n"),
            Item::Separator => out += "---\n",
            Item::Open { label, url } => out += &format!("{label}  <{url}>\n"),
            Item::Quit => out += "Quit\n",
        }
    }
    out
}

/// Same kinds of item in the same order, so `new` can relabel `old`'s menu in place.
#[cfg(any(target_os = "macos", windows))]
pub fn same_shape(old: &MenuModel, new: &MenuModel) -> bool {
    let kinds = |m: &MenuModel| {
        m.items
            .iter()
            .map(std::mem::discriminant)
            .collect::<Vec<_>>()
    };
    kinds(old) == kinds(new)
}

fn count(tasks: &[TaskSummary], f: impl Fn(TaskStatus) -> bool) -> usize {
    tasks.iter().filter(|t| f(t.status)).count()
}

/// `2 workers · 1 running · 2 queued · 4 done · 1 failed`; zero buckets left out,
/// `idle` when nothing is running or waiting. Offline workers don't count.
fn headline(workers: &[WorkerSummary], tasks: &[TaskSummary]) -> String {
    use TaskStatus::*;
    let mut parts = vec![match workers.iter().filter(|w| w.online).count() {
        0 => "no workers".to_string(),
        1 => "1 worker".to_string(),
        n => format!("{n} workers"),
    }];
    let running = count(tasks, |s| s == Running);
    let queued = count(tasks, |s| matches!(s, Planning | Queued));
    if running + queued == 0 {
        parts.push("idle".into());
    }
    let done = count(tasks, |s| s == Done);
    let failed = count(tasks, |s| s == Failed);
    for (n, name) in [
        (running, "running"),
        (queued, "queued"),
        (done, "done"),
        (failed, "failed"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {name}"));
        }
    }
    parts.join(" · ")
}

/// `● 3m12s  counting tests`: status glyph, elapsed, then what a running task
/// is doing or else the prompt's first line.
fn task_label(t: &TaskSummary, now_ms: u64) -> String {
    let glyph = match t.status {
        TaskStatus::Running => '●',
        TaskStatus::Planning | TaskStatus::Queued => '○',
        TaskStatus::Done => '✓',
        TaskStatus::Failed => '✗',
    };
    let time = t.started_at.map_or(String::new(), |s| {
        fmt_elapsed(t.finished_at.unwrap_or(now_ms).saturating_sub(s)) + " "
    });
    let activity = t.activity.strip_prefix("status: ").unwrap_or(&t.activity);
    let what = match t.status {
        TaskStatus::Running if !activity.is_empty() => activity,
        _ => &t.prompt,
    };
    format!("{glyph} {time} {}", first_line(what, 50))
}

/// `KEY=value` lines of a shell env file; skips comments, strips quotes and `export`.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            (k.trim().to_string(), v.to_string())
        })
        .collect()
}

/// URL of the orchestrator at `MIS_BIND`; a wildcard host means loopback.
pub fn local_url(env: &HashMap<String, String>) -> String {
    let bind = env.get("MIS_BIND").map_or("127.0.0.1:7474", String::as_str);
    match bind.rsplit_once(':') {
        Some(("0.0.0.0", port)) => format!("ws://127.0.0.1:{port}"),
        Some(("[::]", port)) => format!("ws://[::1]:{port}"),
        _ => format!("ws://{bind}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(n: u128, status: TaskStatus, activity: &str) -> TaskSummary {
        TaskSummary {
            id: uuid_from(n),
            prompt: format!("prompt {n}\nmore"),
            status,
            worker: None,
            harness: "claude".into(),
            model: None,
            session: None,
            summary: None,
            started_at: Some(1_000),
            finished_at: None,
            tokens_in: None,
            tokens_out: None,
            activity: activity.into(),
            parent: None,
            links: vec![],
            attention: Default::default(),
            blocked_until: None,
            images: vec![],
        }
    }

    fn uuid_from(n: u128) -> TaskId {
        format!("{n:032x}").parse().unwrap()
    }

    fn worker(online: bool) -> WorkerSummary {
        WorkerSummary {
            id: uuid_from(99),
            info: WorkerInfo {
                hostname: "h".into(),
                os: "o".into(),
                arch: "a".into(),
                harnesses: vec![],
                roots: vec![],
                models: Default::default(),
            },
            busy: false,
            online,
            stats: None,
            stats_at: None,
        }
    }

    fn labels(m: &MenuModel) -> Vec<String> {
        m.items
            .iter()
            .map(|i| match i {
                Item::Label(l) | Item::Open { label: l, .. } => l.clone(),
                Item::Separator => "---".into(),
                Item::Quit => "Quit".into(),
            })
            .collect()
    }

    #[test]
    fn disconnected_shows_retrying() {
        let m = menu_model(None, "http://h:1/");
        assert_eq!(
            labels(&m),
            ["Disconnected · retrying", "---", "Open Make It So", "Quit"]
        );
        assert_eq!(
            m.items[2],
            Item::Open {
                label: "Open Make It So".into(),
                url: "http://h:1/".into()
            }
        );
        assert_eq!(m.title, "");
    }

    #[test]
    fn headline_counts_and_idle() {
        use TaskStatus::*;
        let empty = Fleet::default();
        assert_eq!(
            labels(&menu_model(Some(&empty), "p"))[0],
            "no workers · idle"
        );
        let f = Fleet {
            workers: vec![worker(true), worker(false)],
            tasks: vec![task(1, Done, "done"), task(2, Failed, "FAILED")],
            now_ms: 0,
        };
        assert_eq!(
            labels(&menu_model(Some(&f), "p"))[0],
            "1 worker · idle · 1 done · 1 failed"
        );
        let f = Fleet {
            workers: vec![worker(true), worker(true)],
            tasks: vec![
                task(1, Running, ""),
                task(2, Queued, "queued"),
                task(3, Planning, "planning…"),
                task(4, Done, "done"),
            ],
            now_ms: 0,
        };
        let m = menu_model(Some(&f), "p");
        assert_eq!(labels(&m)[0], "2 workers · 1 running · 2 queued · 1 done");
        assert_eq!(m.title, "1");
    }

    #[test]
    fn rows_newest_first_with_glyph_time_and_text() {
        use TaskStatus::*;
        let mut tasks: Vec<_> = (0..10).map(|n| task(n, Done, "done")).collect();
        tasks[9].finished_at = Some(43_000);
        tasks[8].status = Failed;
        tasks[7].status = Queued;
        tasks[7].started_at = None;
        tasks[6] = task(6, Running, "status: counting tests");
        tasks[5] = task(5, Running, "");
        let f = Fleet {
            workers: vec![],
            tasks,
            now_ms: 193_000,
        };
        let m = menu_model(Some(&f), "http://h:1");
        let l = labels(&m);
        assert_eq!(l.len(), 1 + 1 + MAX_TASKS + 3);
        assert_eq!(l[2], "✓ 42s  prompt 9");
        assert_eq!(l[3], "✗ 3m12s  prompt 8");
        assert_eq!(l[4], "○  prompt 7");
        assert_eq!(l[5], "● 3m12s  counting tests");
        assert_eq!(l[6], "● 3m12s  prompt 5");
        assert_eq!(l[9], "✓ 3m12s  prompt 2"); // tasks 0 and 1 cut
        assert_eq!(
            m.items[2],
            Item::Open {
                label: l[2].clone(),
                url: format!("http://h:1/#task/{}", uuid_from(9))
            }
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn shape_ignores_labels() {
        let a = menu_model(None, "p");
        let mut b = menu_model(None, "p");
        b.items[0] = Item::Label("other".into());
        assert!(same_shape(&a, &b));
        let one = Fleet {
            tasks: vec![task(1, TaskStatus::Done, "")],
            ..Default::default()
        };
        assert!(!same_shape(&a, &menu_model(Some(&one), "p")));
    }

    #[test]
    fn text_lists_items_and_urls() {
        assert_eq!(
            text(&menu_model(None, "http://h")),
            "title: \nDisconnected · retrying\n---\nOpen Make It So  <http://h/>\nQuit\n"
        );
    }

    #[test]
    fn env_file_and_url() {
        let env = parse_env("# c\nMIS_TOKEN='a b'\nexport MIS_BIND=\"0.0.0.0:7575\"\n\nX=1=2");
        assert_eq!(env["MIS_TOKEN"], "a b");
        assert_eq!(env["X"], "1=2");
        assert_eq!(local_url(&env), "ws://127.0.0.1:7575");
        assert_eq!(local_url(&HashMap::new()), "ws://127.0.0.1:7474");
        let bind = |b: &str| local_url(&parse_env(&format!("MIS_BIND={b}")));
        assert_eq!(bind("[::]:7474"), "ws://[::1]:7474");
        assert_eq!(bind("100.64.1.2:7474"), "ws://100.64.1.2:7474");
    }
}

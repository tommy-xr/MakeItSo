//! Pure core: fleet status -> menu contents, and connection config.

use std::collections::{BTreeMap, HashMap, HashSet};

use mis_types::*;

/// Rows per section; the rest collapse into a `+N more` row.
pub const MAX_ROWS: usize = 5;

/// A window this full stops its harness unless it has overage (as the orchestrator judges).
const FULL_PCT: f32 = 99.0;

/// Latest `ServerMsg::Status`, plus routines from `GET /api/routines`.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    pub workers: Vec<WorkerSummary>,
    pub tasks: Vec<TaskSummary>,
    pub now_ms: u64,
    /// Empty when the orchestrator has none or predates them.
    pub routines: Vec<RoutineView>,
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
    Submenu {
        label: String,
        items: Vec<Item>,
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
    let mut workers = None;
    match fleet {
        None => items.push(Item::Label("Disconnected · retrying".into())),
        Some(f) => {
            items.push(Item::Label(headline(&f.workers, &f.tasks)));
            let running = count(&f.tasks, |s| s == TaskStatus::Running);
            if running > 0 {
                title = running.to_string();
            }
            items.extend(
                usage_lines(&f.workers, f.now_ms)
                    .into_iter()
                    .map(Item::Label),
            );
            let body = sections(f, page);
            if !body.is_empty() {
                items.push(Item::Separator);
                items.extend(body);
            }
            workers = workers_menu(f, page);
        }
    }
    items.push(Item::Separator);
    items.extend(workers);
    items.push(Item::Open {
        label: "Open Make It So".into(),
        url: format!("{page}/"),
    });
    items.push(Item::Quit);
    MenuModel { title, items }
}

/// The menu as text, one item per line, submenus indented, for `--print`.
pub fn text(m: &MenuModel) -> String {
    fn lines(out: &mut String, items: &[Item], indent: &str) {
        for i in items {
            match i {
                Item::Label(l) => *out += &format!("{indent}{l}\n"),
                Item::Separator => *out += &format!("{indent}---\n"),
                Item::Open { label, url } => *out += &format!("{indent}{label}  <{url}>\n"),
                Item::Submenu { label, items } => {
                    *out += &format!("{indent}{label} ▸\n");
                    lines(out, items, &format!("{indent}  "));
                }
                Item::Quit => *out += &format!("{indent}Quit\n"),
            }
        }
    }
    let mut out = format!("title: {}\n", m.title);
    lines(&mut out, &m.items, "");
    out
}

/// Same kinds of item in the same order, submenus too, so `new` can relabel
/// `old`'s menu in place.
#[cfg(any(target_os = "macos", windows))]
pub fn same_shape(old: &MenuModel, new: &MenuModel) -> bool {
    fn same(a: &[Item], b: &[Item]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(x, y)| match (x, y) {
                (Item::Submenu { items: a, .. }, Item::Submenu { items: b, .. }) => same(a, b),
                _ => std::mem::discriminant(x) == std::mem::discriminant(y),
            })
    }
    same(&old.items, &new.items)
}

/// The item a menu id names: its index, and `.`-separated indices into submenus (`7.2`).
#[cfg(any(target_os = "macos", windows))]
pub fn find<'a>(items: &'a [Item], id: &str) -> Option<&'a Item> {
    let (head, rest) = id.split_once('.').map_or((id, None), |(h, r)| (h, Some(r)));
    let item = items.get(head.parse::<usize>().ok()?)?;
    match (item, rest) {
        (_, None) => Some(item),
        (Item::Submenu { items, .. }, Some(rest)) => find(items, rest),
        _ => None,
    }
}

/// Task ids and statuses of routine runs: when these change, routines' last runs did.
pub fn routine_runs(tasks: &[TaskSummary]) -> Vec<(TaskId, TaskStatus)> {
    tasks
        .iter()
        .filter(|t| t.routine.is_some())
        .map(|t| (t.id, t.status))
        .collect()
}

fn count(tasks: &[TaskSummary], f: impl Fn(TaskStatus) -> bool) -> usize {
    tasks.iter().filter(|t| f(t.status)).count()
}

/// `2 workers · 1 running · 2 queued · 4 done · 1 failed · 1 needs you`; zero
/// buckets left out, `idle` when nothing is running or waiting. Offline workers don't count.
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
    match tasks.iter().filter(|t| urgency(t).is_some()).count() {
        0 => {}
        1 => parts.push("1 needs you".into()),
        n => parts.push(format!("{n} need you")),
    }
    parts.join(" · ")
}

/// Why a task needs the user, most urgent first (as the page ranks them).
fn urgency(t: &TaskSummary) -> Option<u8> {
    match t.attention {
        Attention::None => None,
        Attention::Approval => Some(0),
        Attention::Failed => Some(1),
        Attention::Limited => Some(2),
        Attention::Unread => Some(3),
    }
}

/// Needs you, Running, Routines, then Recent; a task shows in the first that claims it.
fn sections(f: &Fleet, page: &str) -> Vec<Item> {
    let open = |t: &TaskSummary, label| Item::Open {
        label,
        url: format!("{page}/#task/{}", t.id),
    };
    let tasks_url = format!("{page}/#");
    // The orchestrator lists tasks in submission order.
    let newest = || f.tasks.iter().rev();
    let mut out = vec![];
    let mut need: Vec<_> = newest().filter(|t| urgency(t).is_some()).collect();
    need.sort_by_key(|t| urgency(t)); // stable: newest first within a rank
    let rows = need.iter().map(|t| open(t, need_label(t, &f.workers)));
    section(&mut out, "Needs you", rows.collect(), &tasks_url);
    let shown: HashSet<TaskId> = need.iter().map(|t| t.id).collect();
    let running: Vec<_> = newest()
        .filter(|t| t.status == TaskStatus::Running && !shown.contains(&t.id))
        .collect();
    let rows = running.iter().map(|t| open(t, running_label(t, f)));
    section(&mut out, "Running", rows.collect(), &tasks_url);
    let rows = f.routines.iter().map(|r| Item::Open {
        label: routine_label(r, &f.tasks, f.now_ms),
        url: format!("{page}/#routines"),
    });
    section(
        &mut out,
        "Routines",
        rows.collect(),
        &format!("{page}/#routines"),
    );
    let shown: HashSet<_> = shown
        .into_iter()
        .chain(running.iter().map(|t| t.id))
        .collect();
    let rows = newest()
        .filter(|t| !shown.contains(&t.id))
        .map(|t| open(t, task_label(t, f.now_ms)));
    section(&mut out, "Recent", rows.collect(), &tasks_url);
    out
}

/// `title`, up to `MAX_ROWS` rows, then `+N more` opening `more`; nothing without rows.
fn section(out: &mut Vec<Item>, title: &str, mut rows: Vec<Item>, more: &str) {
    if rows.is_empty() {
        return;
    }
    out.push(Item::Label(title.into()));
    let extra = rows.len().saturating_sub(MAX_ROWS);
    rows.truncate(MAX_ROWS);
    out.extend(rows);
    if extra > 0 {
        out.push(Item::Open {
            label: format!("+{extra} more"),
            url: more.into(),
        });
    }
}

fn host(workers: &[WorkerSummary], id: Option<WorkerId>) -> Option<&str> {
    let w = workers.iter().find(|w| Some(w.id) == id)?;
    Some(&w.info.hostname)
}

/// `⚠ approve Bash: make · mac-mini · tune physics`, `✗ failed · tune physics`.
fn need_label(t: &TaskSummary, workers: &[WorkerSummary]) -> String {
    let why = match (t.attention, t.approvals.first()) {
        (Attention::Approval, Some(a)) => format!("⚠ approve {}", first_line(&a.summary, 40)),
        (Attention::Approval, None) => "⚠ approval".into(),
        (Attention::Limited, _) => "⚠ usage limit".into(),
        (Attention::Failed, _) => "✗ failed".into(),
        _ => "✓ unread".into(),
    };
    let on = (t.attention == Attention::Approval)
        .then(|| host(workers, t.worker))
        .flatten();
    let what = first_line(&about(t), 30);
    [Some(why.as_str()), on, Some(what.as_str())]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `● counting tests · mac-mini · 3m12s`: what it does (else its prompt), where, how long.
fn running_label(t: &TaskSummary, f: &Fleet) -> String {
    let activity = t.activity.strip_prefix("status: ").unwrap_or(&t.activity);
    let about = about(t);
    let what = first_line(
        if activity.is_empty() {
            &about
        } else {
            activity
        },
        40,
    );
    let time = t
        .started_at
        .map(|s| fmt_elapsed(f.now_ms.saturating_sub(s)));
    let parts: Vec<_> = [
        Some(what.as_str()),
        host(&f.workers, t.worker),
        time.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    format!("● {}", parts.join(" · "))
}

/// `✓ 3m12s  tune physics`: status glyph, elapsed, what it is for.
fn task_label(t: &TaskSummary, now_ms: u64) -> String {
    let time = t.started_at.map_or(String::new(), |s| {
        fmt_elapsed(t.finished_at.unwrap_or(now_ms).saturating_sub(s)) + " "
    });
    format!("{} {time} {}", glyph(t.status), first_line(&about(t), 50))
}

/// What a task is for: `↻ <routine>` for a routine run (its prompt is boilerplate), else its prompt.
fn about(t: &TaskSummary) -> String {
    t.routine
        .as_ref()
        .map_or_else(|| t.prompt.clone(), |r| format!("↻ {r}"))
}

fn glyph(s: TaskStatus) -> char {
    match s {
        TaskStatus::Running => '●',
        TaskStatus::Planning | TaskStatus::Queued => '○',
        TaskStatus::Delivered => '↪',
        TaskStatus::Done => '✓',
        TaskStatus::Failed => '✗',
        TaskStatus::Cancelled => '⊘',
    }
}

/// `↻ pr-grooming · next 1h20m · last ✓ 3h ago`, `↻ nightly · running · live`,
/// `↻ broken · ⚠ no trigger`. Acting routines name their mode (dry run / live). A run in `tasks` beats the fetched view, which lags.
fn routine_label(r: &RoutineView, tasks: &[TaskSummary], now: u64) -> String {
    let mine = |t: &&TaskSummary| t.routine.as_deref() == Some(r.name.as_str());
    let active = tasks
        .iter()
        .rev()
        .filter(mine)
        .find(|t| !t.status.finished());
    let name = if r.name.is_empty() { &r.file } else { &r.name };
    let mut parts = vec![format!("↻ {name}")];
    match (active, r.trigger.is_some()) {
        (Some(t), _) if t.status == TaskStatus::Running => parts.push("running".into()),
        (Some(_), _) => parts.push("queued".into()),
        (None, false) => {}
        (None, true) if r.paused => parts.push("paused".into()),
        (None, true) => match r.next_at {
            Some(at) if at <= now => parts.push("due".into()),
            Some(at) => parts.push(format!("next {}", span(at - now))),
            None => {}
        },
    }
    if let Some(at) = r.last_at.filter(|_| active.is_none()) {
        let status = r
            .last_task
            .and_then(|id| tasks.iter().find(|t| t.id == id))
            .map(|t| t.status)
            .or(r.last_status);
        let g = status.map_or(String::new(), |s| format!("{} ", glyph(s)));
        parts.push(format!("last {g}{} ago", span(now.saturating_sub(at))));
    }
    match r.mode {
        Some(RoutineMode::DryRun) => parts.push("dry run".into()),
        Some(RoutineMode::Live) => parts.push("live".into()),
        Some(RoutineMode::Report) | None => {}
    }
    if let Some(e) = &r.error {
        parts.push(format!("⚠ {}", first_line(e, 40)));
    }
    parts.join(" · ")
}

/// A rough duration: `<1m`, `4m`, `1h20m`, `2d03h`.
fn span(ms: u64) -> String {
    let m = ms / 60_000;
    match m {
        0 => "<1m".into(),
        1..60 => format!("{m}m"),
        60..1440 => format!("{}h{:02}m", m / 60, m % 60),
        _ => format!("{}d{:02}h", m / 1440, m / 60 % 24),
    }
}

/// Per harness, a line of its windows' fullest fresh reading across online
/// workers (they usually share an account), else the newest stale one:
/// `Claude: 5h 62%, resets 2h10m · 7d 31%, resets 4d02h`.
fn usage_lines(workers: &[WorkerSummary], now: u64) -> Vec<String> {
    let mut best: BTreeMap<&str, Vec<&LimitWindow>> = BTreeMap::new();
    let limits = workers
        .iter()
        .filter(|w| w.online)
        .filter_map(|w| w.stats.as_ref())
        .flat_map(|s| &s.limits);
    for (h, ws) in limits {
        let have = best.entry(h).or_default();
        for w in ws {
            match have.iter_mut().find(|x| x.name == w.name) {
                Some(x) if better(w, x, now) => *x = w,
                Some(_) => {}
                None => have.push(w),
            }
        }
    }
    best.into_iter()
        .filter_map(|(h, ws)| limit_line(h, ws, now))
        .collect()
}

/// Fresh beats stale; then fuller when fresh, newer when stale.
fn better(a: &LimitWindow, b: &LimitWindow, now: u64) -> bool {
    match (a.stale(now), b.stale(now)) {
        (false, true) => true,
        (true, false) => false,
        (false, false) => a.used_pct > b.used_pct,
        (true, true) => a.observed_at > b.observed_at,
    }
}

/// `Claude: 5h 62%, resets 2h10m · 7d 31%`, shortest window first; None without windows.
fn limit_line(harness: &str, mut ws: Vec<&LimitWindow>, now: u64) -> Option<String> {
    if ws.is_empty() {
        return None;
    }
    ws.sort_by(|a, b| (minutes(&a.name), &a.name).cmp(&(minutes(&b.name), &b.name)));
    let mut name = harness.chars();
    let name = name
        .next()
        .map_or(String::new(), |c| c.to_uppercase().chain(name).collect());
    let ws: Vec<_> = ws.iter().map(|w| window_text(w, now)).collect();
    Some(format!("{name}: {}", ws.join(" · ")))
}

/// Length of a window named `5h`, `7d` or `90m`; other names sort last.
fn minutes(name: &str) -> u64 {
    let unit = |u, per: u64| {
        let n: u64 = name.strip_suffix(u)?.parse().ok()?;
        Some(n.saturating_mul(per))
    };
    unit('m', 1)
        .or_else(|| unit('h', 60))
        .or_else(|| unit('d', 1440))
        .unwrap_or(u64::MAX)
}

/// `5h 62%, resets 2h10m`; `(stale)` for a reading too old to trust, `(overage)`
/// for a full window its harness runs on past.
fn window_text(w: &LimitWindow, now: u64) -> String {
    let mut s = format!("{} {:.0}%", w.name, w.used_pct);
    if w.stale(now) {
        return s + " (stale)";
    }
    if w.overage && w.used_pct >= FULL_PCT {
        s += " (overage)";
    }
    if let Some(at) = w.resets_at {
        s += &format!(", resets {}", span(at.saturating_sub(now)));
    }
    s
}

/// `Workers · 2 online` with a row per worker (online first) opening it, and its usage.
fn workers_menu(f: &Fleet, page: &str) -> Option<Item> {
    if f.workers.is_empty() {
        return None;
    }
    let mut ws: Vec<_> = f.workers.iter().collect();
    ws.sort_by(|a, b| (!a.online, &a.info.hostname).cmp(&(!b.online, &b.info.hostname)));
    let mut items = vec![];
    for w in ws {
        let state = match (w.online, w.busy) {
            (false, _) => "✗ offline",
            (true, true) => "● busy",
            (true, false) => "○ idle",
        };
        let update = match &w.update {
            WorkerUpdate::None => None,
            WorkerUpdate::Pending { version } => Some(format!("updating to {version}")),
            WorkerUpdate::Failed { .. } => Some("update failed".into()),
        };
        let parts: Vec<&str> = [
            Some(w.info.hostname.as_str()),
            Some(state),
            w.info.version.as_deref(),
            update.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        items.push(Item::Open {
            label: parts.join(" · "),
            url: format!("{page}/#worker/{}", w.id),
        });
        let limits = w.stats.iter().flat_map(|s| &s.limits);
        let lines = limits.filter_map(|(h, ws)| limit_line(h, ws.iter().collect(), f.now_ms));
        items.extend(lines.map(|l| Item::Label(format!("    {l}"))));
    }
    let online = f.workers.iter().filter(|w| w.online).count();
    let offline = f.workers.len() - online;
    let label = match offline {
        0 => format!("Workers · {online} online"),
        n => format!("Workers · {online} online, {n} offline"),
    };
    Some(Item::Submenu { label, items })
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

    const NOW: u64 = 1_800_000_000_000;
    const MIN: u64 = 60_000;

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
            evidence: vec![],
            result: None,
            diff: None,
            plan: vec![],
            attention: Default::default(),
            blocked_until: None,
            approvals: vec![],
            images: vec![],
            send_to: None,
            routine: None,
            routine_mode: None,
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
                ..Default::default()
            },
            busy: false,
            online,
            stats: None,
            stats_at: None,
            new_tools: vec![],
            update: Default::default(),
        }
    }

    /// `host`, id `n`, with `limits` per harness.
    fn host(
        n: u128,
        name: &str,
        online: bool,
        limits: &[(&str, Vec<LimitWindow>)],
    ) -> WorkerSummary {
        let mut w = worker(online);
        w.id = uuid_from(n);
        w.info.hostname = name.into();
        w.stats = Some(WorkerStats {
            limits: limits
                .iter()
                .map(|(h, ws)| (h.to_string(), ws.clone()))
                .collect(),
            ..Default::default()
        });
        w
    }

    /// A reading of `pct`% taken `age` ago, resetting in `resets`.
    fn win(name: &str, pct: f32, age: u64, resets: Option<u64>) -> LimitWindow {
        LimitWindow {
            name: name.into(),
            used_pct: pct,
            resets_at: resets.map(|r| NOW + r),
            observed_at: NOW - age,
            overage: false,
        }
    }

    fn routine(name: &str) -> RoutineView {
        RoutineView {
            name: name.into(),
            file: format!("{name}.md"),
            trigger: Some("every 6h".into()),
            harness: None,
            model: None,
            worker: None,
            cwd: None,
            paused: false,
            next_at: None,
            last_at: None,
            last_task: None,
            last_status: None,
            error: None,
            mode: Some(RoutineMode::Report),
        }
    }

    fn labels(m: &MenuModel) -> Vec<String> {
        m.items
            .iter()
            .map(|i| match i {
                Item::Label(l) | Item::Open { label: l, .. } | Item::Submenu { label: l, .. } => {
                    l.clone()
                }
                Item::Separator => "---".into(),
                Item::Quit => "Quit".into(),
            })
            .collect()
    }

    fn fleet(workers: Vec<WorkerSummary>, tasks: Vec<TaskSummary>) -> Fleet {
        Fleet {
            workers,
            tasks,
            now_ms: NOW,
            routines: vec![],
        }
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
    fn empty_fleet_has_only_headline_and_footer() {
        let m = menu_model(Some(&Fleet::default()), "p");
        assert_eq!(
            labels(&m),
            ["no workers · idle", "---", "Open Make It So", "Quit"]
        );
    }

    #[test]
    fn headline_counts_idle_and_needs_you() {
        use TaskStatus::*;
        let mut failed = task(2, Failed, "FAILED");
        failed.attention = Attention::Failed;
        let f = fleet(
            vec![worker(true), worker(false)],
            vec![task(1, Done, "done"), failed],
        );
        assert_eq!(
            labels(&menu_model(Some(&f), "p"))[0],
            "1 worker · idle · 1 done · 1 failed · 1 needs you"
        );
        let f = fleet(
            vec![worker(true), worker(true)],
            vec![
                task(1, Running, ""),
                task(2, Queued, "queued"),
                task(3, Planning, "planning…"),
                task(4, Done, "done"),
            ],
        );
        let m = menu_model(Some(&f), "p");
        assert_eq!(labels(&m)[0], "2 workers · 1 running · 2 queued · 1 done");
        assert_eq!(m.title, "1");
    }

    #[test]
    fn sections_rank_needs_then_running_then_recent_without_repeats() {
        use TaskStatus::*;
        let mini = host(50, "mac-mini", true, &[]);
        let mut unread = task(1, Done, "done");
        unread.attention = Attention::Unread;
        unread.routine = Some("pr-grooming".into());
        let mut failed = task(2, Failed, "FAILED");
        failed.attention = Attention::Failed;
        let mut asks = task(3, Running, "> Bash make");
        asks.prompt = "tune jump physics in the functor2 platformer".into();
        asks.worker = Some(mini.id);
        asks.attention = Attention::Approval;
        asks.approvals = vec![Approval {
            id: uuid_from(7),
            tool: "Bash".into(),
            summary: "Bash: python3 make.py".into(),
            detail: String::new(),
            always: None,
        }];
        let mut runs = task(4, Running, "status: counting tests");
        runs.worker = Some(mini.id);
        runs.started_at = Some(NOW - 4 * MIN);
        let mut queued = task(5, Queued, "queued");
        queued.started_at = None;
        let mut limited = task(6, Failed, "limit");
        limited.attention = Attention::Limited;
        let f = fleet(
            vec![mini],
            vec![unread, failed, asks, runs, queued, limited],
        );
        let m = menu_model(Some(&f), "http://h:1");
        assert_eq!(
            labels(&m)[1..12],
            [
                "---",
                "Needs you",
                "⚠ approve Bash: python3 make.py · mac-mini · tune jump physics in the func…",
                "✗ failed · prompt 2",
                "⚠ usage limit · prompt 6",
                "✓ unread · ↻ pr-grooming",
                "Running",
                "● counting tests · mac-mini · 4m00s",
                "Recent",
                "○  prompt 5",
                "---",
            ]
        );
        assert_eq!(
            m.items[3],
            Item::Open {
                label: labels(&m)[3].clone(),
                url: format!("http://h:1/#task/{}", uuid_from(3))
            }
        );
    }

    #[test]
    fn recent_rows_newest_first_capped_with_more() {
        use TaskStatus::*;
        let mut tasks: Vec<_> = (0..8).map(|n| task(n, Done, "done")).collect();
        tasks[7].finished_at = Some(43_000);
        tasks[6].status = Cancelled;
        tasks[5].status = Queued;
        tasks[5].started_at = None;
        let f = Fleet {
            now_ms: 193_000,
            ..fleet(vec![], tasks)
        };
        let m = menu_model(Some(&f), "http://h:1");
        let l = labels(&m);
        assert_eq!(
            l[2..10],
            [
                "Recent",
                "✓ 42s  prompt 7",
                "⊘ 3m12s  prompt 6",
                "○  prompt 5",
                "✓ 3m12s  prompt 4",
                "✓ 3m12s  prompt 3",
                "+3 more",
                "---",
            ]
        );
        assert_eq!(
            m.items[8],
            Item::Open {
                label: "+3 more".into(),
                url: "http://h:1/#".into()
            }
        );
    }

    #[test]
    fn usage_takes_fullest_fresh_reading_per_window() {
        let stale = 7 * 60 * MIN;
        let mut over = win("5h", 100.0, MIN, Some(90 * MIN));
        over.overage = true;
        let f = fleet(
            vec![
                host(
                    1,
                    "a",
                    true,
                    &[
                        (
                            "claude",
                            vec![
                                win("7d", 31.0, stale, None),
                                win("5h", 62.0, MIN, Some(130 * MIN)),
                            ],
                        ),
                        ("codex", vec![over]),
                    ],
                ),
                // Fuller but stale, and fuller but offline: neither counts.
                host(
                    2,
                    "b",
                    true,
                    &[("claude", vec![win("5h", 70.0, stale, None)])],
                ),
                host(
                    3,
                    "c",
                    false,
                    &[("claude", vec![win("5h", 90.0, MIN, None)])],
                ),
            ],
            vec![],
        );
        let l = labels(&menu_model(Some(&f), "p"));
        assert_eq!(
            l[1..3],
            [
                "Claude: 5h 62%, resets 2h10m · 7d 31% (stale)",
                "Codex: 5h 100% (overage), resets 1h30m",
            ]
        );
        // A reset window is stale too, even when just read.
        let f = fleet(
            vec![host(
                1,
                "a",
                true,
                &[("claude", vec![win("5h", 99.0, 0, Some(0))])],
            )],
            vec![],
        );
        assert_eq!(
            labels(&menu_model(Some(&f), "p"))[1],
            "Claude: 5h 99% (stale)"
        );
    }

    #[test]
    fn routines_show_schedule_runs_and_errors() {
        use TaskStatus::*;
        let mut next = routine("pr-grooming");
        next.next_at = Some(NOW + 80 * MIN);
        next.last_at = Some(NOW - 3 * 60 * MIN);
        next.last_task = Some(uuid_from(1));
        next.last_status = Some(Failed); // the live task wins
        let mut busy = routine("nightly-ci");
        busy.last_at = Some(NOW - MIN);
        busy.mode = Some(RoutineMode::Live);
        let mut paused = routine("weekly");
        paused.paused = true;
        paused.mode = Some(RoutineMode::DryRun);
        let mut due = routine("due");
        due.next_at = Some(NOW);
        due.error = Some("no worker named mac-pro\nmore".into());
        let mut bad = routine("");
        bad.file = "broken.md".into();
        bad.trigger = None;
        bad.error = Some("no `every` or `cron`".into());
        let mut run = task(2, Running, "");
        run.routine = Some("nightly-ci".into());
        let mut f = fleet(vec![], vec![task(1, Done, ""), run]);
        f.routines = vec![next, busy, paused, due, bad];
        let m = menu_model(Some(&f), "http://h");
        let l = labels(&m);
        let at = l.iter().position(|l| l == "Routines").unwrap();
        assert_eq!(
            l[at + 1..at + 6],
            [
                "↻ pr-grooming · next 1h20m · last ✓ 3h00m ago",
                "↻ nightly-ci · running · live",
                "↻ weekly · paused · dry run",
                "↻ due · due · ⚠ no worker named mac-pro",
                "↻ broken.md · ⚠ no `every` or `cron`",
            ]
        );
        assert_eq!(
            m.items[at + 1],
            Item::Open {
                label: l[at + 1].clone(),
                url: "http://h/#routines".into()
            }
        );
        f.routines.clear();
        assert!(!labels(&menu_model(Some(&f), "p")).contains(&"Routines".to_string()));
    }

    #[test]
    fn workers_submenu_lists_state_version_and_usage() {
        let mut mini = host(
            2,
            "mac-mini",
            true,
            &[("claude", vec![win("5h", 62.0, MIN, None)])],
        );
        mini.busy = true;
        mini.info.version = Some("0.1.0+abc1234".into());
        mini.update = WorkerUpdate::Pending {
            version: "0.1.0+def5678".into(),
        };
        let air = host(3, "MacBook-Air", true, &[]);
        let old = host(1, "aaa-old", false, &[]);
        let m = menu_model(Some(&fleet(vec![mini, old, air], vec![])), "http://h");
        let Item::Submenu { label, .. } = &m.items[m.items.len() - 3] else {
            panic!("{:?}", m.items)
        };
        assert_eq!(label, "Workers · 2 online, 1 offline");
        let t = text(&m);
        let want = format!(
            "---\nWorkers · 2 online, 1 offline ▸\n\
             \x20 MacBook-Air · ○ idle  <http://h/#worker/{}>\n\
             \x20 mac-mini · ● busy · 0.1.0+abc1234 · updating to 0.1.0+def5678  <http://h/#worker/{}>\n\
             \x20     Claude: 5h 62%\n\
             \x20 aaa-old · ✗ offline  <http://h/#worker/{}>\n\
             Open Make It So",
            uuid_from(3),
            uuid_from(2),
            uuid_from(1)
        );
        assert!(t.contains(&want), "{t}");
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn find_walks_submenu_paths() {
        let items = vec![
            Item::Separator,
            Item::Submenu {
                label: "s".into(),
                items: vec![Item::Separator, Item::Quit],
            },
        ];
        assert_eq!(find(&items, "0"), Some(&Item::Separator));
        assert_eq!(find(&items, "1.1"), Some(&Item::Quit));
        assert_eq!(find(&items, "1.2"), None);
        assert_eq!(find(&items, "0.0"), None);
        assert_eq!(find(&items, "x"), None);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn shape_ignores_labels_but_not_submenu_contents() {
        let a = menu_model(None, "p");
        let mut b = menu_model(None, "p");
        b.items[0] = Item::Label("other".into());
        assert!(same_shape(&a, &b));
        let one = Fleet {
            tasks: vec![task(1, TaskStatus::Done, "")],
            ..Default::default()
        };
        assert!(!same_shape(&a, &menu_model(Some(&one), "p")));
        let sub = |n| MenuModel {
            title: String::new(),
            items: vec![Item::Submenu {
                label: format!("{n}"),
                items: (0..n).map(|_| Item::Separator).collect(),
            }],
        };
        assert!(same_shape(&sub(1), &sub(1)));
        assert!(!same_shape(&sub(1), &sub(2)));
    }

    #[test]
    fn text_lists_items_and_urls() {
        assert_eq!(
            text(&menu_model(None, "http://h")),
            "title: \nDisconnected · retrying\n---\nOpen Make It So  <http://h/>\nQuit\n"
        );
    }

    #[test]
    fn spans_and_window_lengths() {
        assert_eq!(span(59_000), "<1m");
        assert_eq!(span(4 * MIN), "4m");
        assert_eq!(span(80 * MIN), "1h20m");
        assert_eq!(span(51 * 60 * MIN), "2d03h");
        let mut names = ["7d", "x", "90m", "5時", "5h", "1d", "99999999999999999999d"];
        names.sort_by_key(|n| minutes(n));
        assert_eq!(
            names,
            ["90m", "5h", "1d", "7d", "x", "5時", "99999999999999999999d"]
        );
    }

    #[test]
    fn routine_runs_track_routine_tasks_only() {
        let mut r = task(2, TaskStatus::Running, "");
        r.routine = Some("x".into());
        let tasks = vec![task(1, TaskStatus::Done, ""), r];
        assert_eq!(routine_runs(&tasks), [(uuid_from(2), TaskStatus::Running)]);
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

//! Routines: periodic tasks, one Markdown file each (frontmatter + prompt). Pure:
//! parsing files, when each routine is due, and the prompt a run gets. The shell
//! reads the files and the clock; the ledger keeps each routine's [`State`] and
//! starts its runs as ordinary tasks.

use std::collections::BTreeMap;

use chrono::TimeZone;
use mis_types::{RoutineMode, RoutineView, TaskId, TaskStatus};
use serde::{Deserialize, Serialize};

mod cron;
pub use cron::Cron;

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    /// Millis between runs, counted from the last run; the first run is right away.
    Every(u64),
    /// Wall-clock pattern in the orchestrator's time zone.
    Cron(Cron),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Routine {
    pub name: String,
    /// File name in the routines dir.
    pub file: String,
    pub trigger: Trigger,
    /// None: claude.
    pub harness: Option<String>,
    pub model: Option<String>,
    /// Name of the only worker that may run it (as the planner pins).
    pub worker: Option<String>,
    pub cwd: Option<String>,
    /// As the file says; [`State::paused`] overrides it.
    pub paused: bool,
    /// May act (`act: true`): runs dry until [`State::live`].
    pub act: bool,
    pub prompt: String,
}

/// A file in the routines dir that doesn't define one, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct BadFile {
    pub file: String,
    pub error: String,
}

pub type Def = Result<Routine, BadFile>;

/// What the orchestrator keeps per routine name, apart from its file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Unix millis runs are counted from, failing a later run: when first seen, and
    /// moved to now while paused, so runs missed then are skipped, not caught up.
    pub since: u64,
    pub last_run: Option<u64>,
    pub last_task: Option<TaskId>,
    /// Paused or resumed from the API; beats the file's `paused`.
    pub paused: Option<bool>,
    /// Switched live from the API; counts only while the file says `act: true`.
    pub live: bool,
    /// Why the last attempt couldn't start a task; cleared once one does.
    pub error: Option<String>,
}

impl State {
    pub fn new(since: u64) -> Self {
        State {
            since,
            ..Default::default()
        }
    }
}

impl Routine {
    pub fn paused(&self, s: &State) -> bool {
        s.paused.unwrap_or(self.paused)
    }

    /// Unix millis of its next run, from its last one; None while paused (or a
    /// cron that never matches). Already past means due now. Runs that would have
    /// started before `s.since` or before the last run `ended` are skipped.
    pub fn next_at<Tz: TimeZone>(&self, s: &State, ended: Option<u64>, tz: &Tz) -> Option<u64> {
        if self.paused(s) {
            return None;
        }
        let floor = s.since.max(ended.unwrap_or(0));
        match (&self.trigger, s.last_run) {
            (Trigger::Every(_), None) => Some(floor),
            // The first of last + k * ms at or after `floor`, k >= 1.
            (Trigger::Every(ms), Some(last)) => {
                let k = floor.saturating_sub(last).div_ceil(*ms).max(1);
                Some(last.saturating_add(k.saturating_mul(*ms)))
            }
            (Trigger::Cron(c), last) => c.next_after(last.unwrap_or(0).max(floor), tz),
        }
    }

    /// Report unless the file says `act: true`; then dry run until switched live.
    pub fn mode(&self, s: &State) -> RoutineMode {
        match (self.act, s.live) {
            (false, _) => RoutineMode::Report,
            (true, false) => RoutineMode::DryRun,
            (true, true) => RoutineMode::Live,
        }
    }

    /// The prompt a run in `mode` gets: one paragraph saying what it may do, then
    /// the file's. Runs start unattended, so even live ones never merge.
    pub fn run_prompt(&self, mode: RoutineMode) -> String {
        let rules = match mode {
            RoutineMode::Report => {
                "(scheduled run, report only)\n\
                 Investigate and report what you find. Do not push, merge, open, close or \
                 comment on PRs or issues, deploy, or change any other remote state."
            }
            RoutineMode::DryRun => {
                "(scheduled run, dry run)\n\
                 This routine may act, but this run is a dry run, so the user can judge it \
                 before letting it act. Investigate, then list exactly the actions you would \
                 take: each command, push, comment or label, with its full text. Perform none \
                 of them: change no files, commits, PRs, issues or other state."
            }
            RoutineMode::Live => {
                "(scheduled run, live)\n\
                 You may act as the instructions below say, and only that far. Never merge \
                 PRs, push tags, create releases, or read, change or print secrets or \
                 credentials. Report every action you take (command, push, comment, label), \
                 and call `report_evidence` for each PR, commit or comment you made."
            }
        };
        format!("Routine \"{}\" {rules}\n\n{}", self.name, self.prompt)
    }
}

/// Where a routine's runs stand, from the ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Runs {
    /// A run hasn't finished.
    pub busy: bool,
    /// Unix millis the last run ended, if it has.
    pub ended: Option<u64>,
}

/// Routines to start at `now_ms`: unpaused, at or past their next run, and not
/// still running. However many runs downtime missed, one starts, and the next
/// counts from it; runs that fall while one is going are skipped. A routine
/// without a state is new: seen now.
pub fn due<'a, Tz: TimeZone>(
    defs: &'a [Def],
    states: &BTreeMap<String, State>,
    runs: impl Fn(&Routine) -> Runs,
    now_ms: u64,
    tz: &Tz,
) -> Vec<&'a Routine> {
    let new = State::new(now_ms);
    defs.iter()
        .flatten()
        .filter(|r| {
            let (s, runs) = (states.get(&r.name).unwrap_or(&new), runs(r));
            !runs.busy && r.next_at(s, runs.ended, tz).is_some_and(|at| at <= now_ms)
        })
        .collect()
}

/// `def` as listed at `now_ms`, with its state (None: new), its runs and its last
/// task's status.
pub fn view<Tz: TimeZone>(
    def: &Def,
    state: Option<&State>,
    runs: Runs,
    last_status: Option<TaskStatus>,
    now_ms: u64,
    tz: &Tz,
) -> RoutineView {
    let s = state.cloned().unwrap_or_else(|| State::new(now_ms));
    let (last_at, last_task) = (s.last_run, s.last_task);
    match def {
        Ok(r) => RoutineView {
            name: r.name.clone(),
            file: r.file.clone(),
            trigger: Some(match &r.trigger {
                Trigger::Every(ms) => format!("every {}", fmt_every(*ms)),
                Trigger::Cron(c) => format!("cron {}", c.as_str()),
            }),
            harness: r.harness.clone(),
            model: r.model.clone(),
            worker: r.worker.clone(),
            cwd: r.cwd.clone(),
            paused: r.paused(&s),
            mode: Some(r.mode(&s)),
            next_at: r.next_at(&s, runs.ended, tz),
            last_at,
            last_task,
            last_status,
            error: s.error.clone(),
        },
        Err(bad) => RoutineView {
            name: stem(&bad.file).to_string(),
            file: bad.file.clone(),
            trigger: None,
            harness: None,
            model: None,
            worker: None,
            cwd: None,
            paused: false,
            mode: None,
            next_at: None,
            last_at,
            last_task,
            last_status,
            error: Some(bad.error.clone()),
        },
    }
}

/// The name a bad file goes by: its file name less `.md`.
pub fn stem(file: &str) -> &str {
    file.strip_suffix(".md").unwrap_or(file)
}

/// One routine per file (name, contents or why it can't be read), in the given
/// order; a file reusing an earlier file's name is bad.
pub fn parse_all(files: Vec<(String, Result<String, String>)>) -> Vec<Def> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    files
        .into_iter()
        .map(|(file, text)| {
            let r = text.and_then(|t| parse(&file, &t));
            let r = r.and_then(|r| match seen.get(&r.name) {
                Some(first) => Err(format!("name `{}` is taken by {first}", r.name)),
                None => {
                    seen.insert(r.name.clone(), file.clone());
                    Ok(r)
                }
            });
            r.map_err(|error| BadFile { file, error })
        })
        .collect()
}

/// A routine file: `---`, `key: value` lines, `---`, then the prompt.
/// Keys: `name` (default: the file name less `.md`), `every` (`30m`, `6h`, `1d`) or
/// `cron` (5 fields), and optional `harness`, `model`, `worker`, `cwd` (a folder on
/// the worker, as for tasks: absolute or under its root; no `~`), `paused`, `act`
/// (may act once switched live; see [`Routine::mode`]).
pub fn parse(file: &str, text: &str) -> Result<Routine, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim) != Some("---") {
        return Err("must start with a `---` frontmatter line".into());
    }
    let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
    let mut body = None;
    let mut at = text.find('\n').map_or(text.len(), |i| i + 1);
    for line in lines {
        at += line.len();
        let line = line.trim();
        if line == "---" {
            body = Some(&text[at..]);
            break;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) =
            (line.split_once(':')).ok_or_else(|| format!("`{line}`: want `key: value`"))?;
        let (k, v) = (k.trim(), unquote(v.trim()));
        const KEYS: [&str; 9] = [
            "name", "every", "cron", "harness", "model", "worker", "cwd", "paused", "act",
        ];
        if !KEYS.contains(&k) {
            return Err(format!("unknown key `{k}` (known: {})", KEYS.join(", ")));
        }
        if keys.insert(k, v).is_some() {
            return Err(format!("`{k}` is set twice"));
        }
    }
    let body = body.ok_or("the frontmatter needs a closing `---` line")?;
    let get = |k| keys.get(k).filter(|v| !v.is_empty()).map(|v| v.to_string());
    let name = get("name").unwrap_or_else(|| stem(file).to_string());
    let name_ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    // Starting with a letter or digit keeps `.` and `..` out of URLs.
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) || !name.chars().all(name_ok) {
        return Err(format!(
            "name `{name}`: use letters, digits, `-`, `_` or `.`"
        ));
    }
    let trigger = match (get("every"), get("cron")) {
        (Some(e), None) => Trigger::Every(parse_every(&e)?),
        (None, Some(c)) => Trigger::Cron(c.parse()?),
        (Some(_), Some(_)) => return Err("set `every` or `cron`, not both".into()),
        (None, None) => {
            return Err("needs a trigger: `every: 6h` or `cron: \"0 9 * * 1-5\"`".into())
        }
    };
    let flag = |k: &'static str| match get(k).as_deref() {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(v) => Err(format!("{k} `{v}`: want true or false")),
    };
    let (paused, act) = (flag("paused")?, flag("act")?);
    let prompt = body.trim().to_string();
    if prompt.is_empty() {
        return Err("no prompt after the frontmatter".into());
    }
    Ok(Routine {
        name,
        file: file.to_string(),
        trigger,
        harness: get("harness"),
        model: get("model"),
        worker: get("worker"),
        cwd: get("cwd"),
        paused,
        act,
        prompt,
    })
}

/// `"x"` or `'x'` -> `x`.
fn unquote(v: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

/// `30m`, `6h`, `1d` in millis; at least a minute.
fn parse_every(s: &str) -> Result<u64, String> {
    let bad = || format!("every `{s}`: want minutes, hours or days, e.g. 30m, 6h, 1d");
    let Some((i, _)) = s.char_indices().last() else {
        return Err(bad());
    };
    let (n, unit) = s.split_at(i);
    let unit = match unit {
        "m" => MINUTE_MS,
        "h" => HOUR_MS,
        "d" => DAY_MS,
        _ => return Err(bad()),
    };
    match n.trim().parse::<u64>() {
        Ok(n) if n > 0 => n.checked_mul(unit).ok_or_else(bad),
        _ => Err(bad()),
    }
}

/// Millis back to the largest whole unit: `6h`, `90m`, `1d`.
fn fmt_every(ms: u64) -> String {
    match ms {
        _ if ms.is_multiple_of(DAY_MS) => format!("{}d", ms / DAY_MS),
        _ if ms.is_multiple_of(HOUR_MS) => format!("{}h", ms / HOUR_MS),
        _ => format!("{}m", ms / MINUTE_MS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    const FILE: &str = "---\nname: pr-grooming\nevery: 6h\nharness: codex\nmodel: \"gpt-5\"\n\
                        worker: mac\ncwd: /srv/app\n---\n\nGroom the open PRs.\n";

    fn routine(trigger: &str) -> Routine {
        parse("r.md", &format!("---\n{trigger}\n---\ngo")).unwrap()
    }

    fn defs(rs: &[Routine]) -> Vec<Def> {
        rs.iter().cloned().map(Ok).collect()
    }

    fn idle(_: &Routine) -> Runs {
        Runs::default()
    }

    fn busy() -> Runs {
        Runs {
            busy: true,
            ended: None,
        }
    }

    fn names(due: Vec<&Routine>) -> Vec<&str> {
        due.into_iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let r = parse("x.md", FILE).unwrap();
        assert_eq!(r.name, "pr-grooming");
        assert_eq!(r.trigger, Trigger::Every(6 * HOUR_MS));
        assert_eq!(r.harness.as_deref(), Some("codex"));
        assert_eq!(r.model.as_deref(), Some("gpt-5"));
        assert_eq!(r.worker.as_deref(), Some("mac"));
        assert_eq!(r.cwd.as_deref(), Some("/srv/app"));
        assert!(!r.paused);
        assert_eq!(r.prompt, "Groom the open PRs.");
        // CRLF, a BOM, comments, and the name from the file.
        let crlf =
            "\u{feff}---\r\n# nightly\r\ncron: '0 9 * * 1-5'\r\npaused: true\r\n---\r\nhi\r\n";
        let r = parse("daily.md", crlf).unwrap();
        assert_eq!(r.name, "daily");
        assert!(matches!(&r.trigger, Trigger::Cron(c) if c.as_str() == "0 9 * * 1-5"));
        assert!(r.paused);
        assert_eq!(r.prompt, "hi");
    }

    #[test]
    fn bad_files_say_why() {
        for (text, why) in [
            ("no frontmatter", "must start"),
            ("---\nevery: 6h\n", "closing `---`"),
            ("---\n---\nhi", "needs a trigger"),
            ("---\nevery: 6h\ncron: 0 * * * *\n---\nhi", "not both"),
            ("---\nevery: 6\n---\nhi", "every `6`"),
            ("---\nevery: 0m\n---\nhi", "every `0m`"),
            ("---\nevery: 10s\n---\nhi", "every `10s`"),
            ("---\nevery: 6é\n---\nhi", "every `6é`"),
            ("---\ncron: 0 9 * *\n---\nhi", "5 fields"),
            ("---\nevry: 6h\n---\nhi", "unknown key `evry`"),
            ("---\nevery: 6h\nevery: 1h\n---\nhi", "set twice"),
            ("---\nevery: 6h\nname: a b\n---\nhi", "name `a b`"),
            ("---\nevery: 6h\nname: ..\n---\nhi", "name `..`"),
            ("---\nevery: 6h\npaused: yes\n---\nhi", "paused `yes`"),
            ("---\nevery: 6h\nact: 1\n---\nhi", "act `1`"),
            (
                "---\nevery: 6h\nharness claude\n---\nhi",
                "want `key: value`",
            ),
            ("---\nevery: 6h\n---\n \n", "no prompt"),
        ] {
            let err = parse("r.md", text).unwrap_err();
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }

    #[test]
    fn parse_all_keeps_bad_files_and_refuses_reused_names() {
        let ok = |t: &str| Ok(t.to_string());
        let got = parse_all(vec![
            ("a.md".into(), ok("---\nname: same\nevery: 1h\n---\nx")),
            ("b.md".into(), ok("---\nname: same\nevery: 1h\n---\ny")),
            ("c.md".into(), Err("permission denied".into())),
            ("d.md".into(), ok("junk")),
        ]);
        assert!(got[0].is_ok());
        let errs: Vec<_> = got[1..].iter().map(|d| d.clone().unwrap_err()).collect();
        assert!(errs[0].error.contains("taken by a.md"), "{:?}", errs[0]);
        assert_eq!(errs[1].error, "permission denied");
        assert_eq!(errs[2].file, "d.md");
    }

    #[test]
    fn every_runs_first_right_away_then_each_interval_after_the_last() {
        let r = routine("every: 30m");
        let ds = defs(&[r]);
        // New: due at once.
        assert_eq!(names(due(&ds, &BTreeMap::new(), idle, 1000, &Utc)), ["r"]);
        let mut states = BTreeMap::new();
        let mut s = State::new(0);
        s.last_run = Some(10 * MINUTE_MS);
        states.insert("r".to_string(), s);
        assert!(due(&ds, &states, idle, 39 * MINUTE_MS, &Utc).is_empty());
        assert_eq!(names(due(&ds, &states, idle, 40 * MINUTE_MS, &Utc)), ["r"]);
    }

    #[test]
    fn downtime_catches_up_once() {
        let ds = defs(&[routine("every: 1h")]);
        let mut states = BTreeMap::new();
        let mut s = State::new(0);
        s.last_run = Some(0);
        states.insert("r".to_string(), s.clone());
        // Ten missed runs: one is due...
        let now = 10 * HOUR_MS + 5;
        assert_eq!(due(&ds, &states, idle, now, &Utc).len(), 1);
        // ...and once it starts, the next is an hour on, not the other nine.
        s.last_run = Some(now);
        states.insert("r".to_string(), s);
        assert!(due(&ds, &states, idle, now + 1, &Utc).is_empty());
        assert!(due(&ds, &states, idle, now + HOUR_MS - 1, &Utc).is_empty());
        assert_eq!(due(&ds, &states, idle, now + HOUR_MS, &Utc).len(), 1);
    }

    #[test]
    fn no_overlap_while_the_last_run_is_busy() {
        let ds = defs(&[routine("every: 1m")]);
        let states = BTreeMap::new();
        assert!(due(&ds, &states, |_| busy(), 0, &Utc).is_empty());
        assert_eq!(due(&ds, &states, idle, 0, &Utc).len(), 1);
    }

    #[test]
    fn runs_that_fall_while_one_is_going_are_skipped() {
        let r = routine("every: 1h");
        let mut s = State::new(0);
        s.last_run = Some(0);
        // The run took 2.5h: the next is at 3h, not right away.
        assert_eq!(
            r.next_at(&s, Some(150 * MINUTE_MS), &Utc),
            Some(3 * HOUR_MS)
        );
        // Ended within its hour: an hour after it started.
        assert_eq!(r.next_at(&s, Some(MINUTE_MS), &Utc), Some(HOUR_MS));
        // Cron: a catch-up at 08:59:30 that ran to 09:05 skips 09:00.
        let c = routine("cron: 0 9 * * *");
        s.last_run = Some(8 * HOUR_MS + 59 * MINUTE_MS + 30_000);
        let ended = Some(9 * HOUR_MS + 5 * MINUTE_MS);
        assert_eq!(c.next_at(&s, ended, &Utc), Some(33 * HOUR_MS));
    }

    #[test]
    fn resuming_skips_the_runs_missed_while_paused() {
        let r = routine("every: 1h");
        let mut s = State::new(0);
        s.last_run = Some(0);
        // Paused from 0 to 5.5h: `since` kept moving, so the next is at 6h.
        s.since = 5 * HOUR_MS + 30 * MINUTE_MS;
        assert_eq!(r.next_at(&s, Some(MINUTE_MS), &Utc), Some(6 * HOUR_MS));
    }

    #[test]
    fn paused_by_file_or_state_and_resume_overrides_the_file() {
        let ds = defs(&[routine("every: 1m\npaused: true")]);
        let mut states = BTreeMap::new();
        assert!(due(&ds, &states, idle, 0, &Utc).is_empty());
        let mut s = State::new(0);
        s.paused = Some(false);
        states.insert("r".to_string(), s.clone());
        assert_eq!(due(&ds, &states, idle, 0, &Utc).len(), 1);
        let ds = defs(&[routine("every: 1m")]);
        s.paused = Some(true);
        states.insert("r".to_string(), s);
        assert!(due(&ds, &states, idle, 0, &Utc).is_empty());
        let v = view(&ds[0], states.get("r"), Runs::default(), None, 0, &Utc);
        assert!(v.paused);
        assert_eq!(v.next_at, None);
    }

    #[test]
    fn cron_waits_for_its_first_match_then_counts_from_the_last_run() {
        let ds = defs(&[routine("cron: 0 * * * *")]);
        let mut states = BTreeMap::new();
        // First seen at 00:10: nothing until 01:00.
        let mut s = State::new(10 * MINUTE_MS);
        states.insert("r".to_string(), s.clone());
        assert!(due(&ds, &states, idle, 59 * MINUTE_MS, &Utc).is_empty());
        assert_eq!(due(&ds, &states, idle, HOUR_MS, &Utc).len(), 1);
        // Down from 01:00 to 05:30: one run, then 06:00.
        s.last_run = Some(HOUR_MS);
        states.insert("r".to_string(), s.clone());
        let late = 5 * HOUR_MS + 30 * MINUTE_MS;
        assert_eq!(due(&ds, &states, idle, late, &Utc).len(), 1);
        s.last_run = Some(late);
        assert_eq!(
            ds[0].as_ref().unwrap().next_at(&s, None, &Utc),
            Some(6 * HOUR_MS)
        );
    }

    #[test]
    fn bad_files_are_never_due_and_show_their_error() {
        let ds = parse_all(vec![("bad.md".into(), Ok("nope".into()))]);
        assert!(due(&ds, &BTreeMap::new(), idle, 0, &Utc).is_empty());
        let v = view(&ds[0], None, Runs::default(), None, 0, &Utc);
        assert_eq!((v.name.as_str(), v.trigger), ("bad", None));
        assert!(v.error.unwrap().contains("must start"));
    }

    #[test]
    fn view_shows_the_trigger_and_schedule() {
        let r = parse("x.md", FILE).unwrap();
        let mut s = State::new(0);
        s.last_run = Some(HOUR_MS);
        let v = view(
            &Ok(r),
            Some(&s),
            Runs::default(),
            Some(TaskStatus::Done),
            0,
            &Utc,
        );
        assert_eq!(v.trigger.as_deref(), Some("every 6h"));
        assert_eq!(v.next_at, Some(7 * HOUR_MS));
        assert_eq!(v.last_status, Some(TaskStatus::Done));
        assert_eq!(fmt_every(90 * MINUTE_MS), "90m");
        assert_eq!(fmt_every(DAY_MS), "1d");
    }

    #[test]
    fn mode_needs_act_in_the_file_and_live_in_the_state() {
        let mut live = State::new(0);
        live.live = true;
        let report = parse("x.md", FILE).unwrap();
        let act = routine("every: 1h\nact: true");
        assert!(!report.act && act.act);
        assert_eq!(report.mode(&State::new(0)), RoutineMode::Report);
        assert_eq!(report.mode(&live), RoutineMode::Report, "act removed");
        assert_eq!(act.mode(&State::new(0)), RoutineMode::DryRun);
        assert_eq!(act.mode(&live), RoutineMode::Live);
        let v = view(&Ok(act), Some(&live), Runs::default(), None, 0, &Utc);
        assert_eq!(v.mode, Some(RoutineMode::Live));
    }

    #[test]
    fn run_prompt_says_what_each_mode_may_do() {
        let r = parse("x.md", FILE).unwrap();
        let p = |m| r.run_prompt(m);
        assert!(p(RoutineMode::Report)
            .starts_with("Routine \"pr-grooming\" (scheduled run, report only)\n"));
        assert!(p(RoutineMode::Report).contains("Do not push, merge"));
        let dry = p(RoutineMode::DryRun);
        assert!(dry.starts_with("Routine \"pr-grooming\" (scheduled run, dry run)\n"));
        assert!(
            dry.contains("list exactly the actions you would take") && dry.contains("Perform none")
        );
        let live = p(RoutineMode::Live);
        assert!(live.starts_with("Routine \"pr-grooming\" (scheduled run, live)\n"));
        assert!(live.contains("Never merge PRs, push tags, create releases"));
        assert!(live.contains("report_evidence"));
        for m in [RoutineMode::Report, RoutineMode::DryRun, RoutineMode::Live] {
            // One paragraph of rules: the phone shows what follows it as the routine's prompt.
            let text = p(m);
            assert_eq!(text.split_once("\n\n").unwrap().1, "Groom the open PRs.");
        }
    }
}

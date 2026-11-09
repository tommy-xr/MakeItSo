//! Routines: periodic tasks, one Markdown file each (frontmatter + prompt). Pure:
//! parsing files, when each routine is due, and the prompt a run gets. The shell
//! reads the files and the clock; the ledger keeps each routine's [`State`] and
//! starts its runs as ordinary tasks.

use std::collections::BTreeMap;

use chrono::TimeZone;
use mis_types::{
    MergeApprove, MergeMethod, MergePolicy, MergeRule, NewRoutine, RoutineMode, RoutineView,
    TaskId, TaskStatus, MERGE_TOOL,
};
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
    /// What `merge_pr` may merge; None: runs never merge.
    pub merge: Option<MergePolicy>,
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
    /// Switched live from the API; counts only while the file says `act: true`
    /// and its [`Routine::pin`] is `live_pin`.
    pub live: bool,
    /// The routine's pin when it went live.
    pub live_pin: Option<String>,
    /// It dropped back to dry run because its file changed while live.
    pub changed: bool,
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

    /// Report unless the file says `act: true`; then dry run until switched live,
    /// and again once what went live changes (see [`Routine::pin`]).
    pub fn mode(&self, s: &State) -> RoutineMode {
        let pinned = s.live_pin.as_deref() == Some(self.pin().as_str());
        match (self.act, s.live && pinned) {
            (false, _) => RoutineMode::Report,
            (true, false) => RoutineMode::DryRun,
            (true, true) => RoutineMode::Live,
        }
    }

    /// Hex SHA-256 of what going live approves: the prompt, merge policy, and
    /// where and how it runs (harness, model, worker, cwd).
    pub fn pin(&self) -> String {
        use sha2::Digest;
        let what = serde_json::json!({
            "prompt": self.prompt,
            "merge": self.merge,
            "harness": self.harness,
            "model": self.model,
            "worker": self.worker,
            "cwd": self.cwd,
        });
        let hash = sha2::Sha256::digest(what.to_string().as_bytes());
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// How its runs may merge: through `merge_pr` if the file has a policy (a dry
    /// run's only says what it would merge), else never.
    pub fn merge_rule(&self) -> MergeRule {
        self.merge
            .clone()
            .map_or(MergeRule::Never, |p| MergeRule::Tool(Box::new(p)))
    }

    /// The prompt a run in `mode` gets: one paragraph saying what it may do, then
    /// the file's. Runs start unattended, so they merge only through `merge_pr`.
    pub fn run_prompt(&self, mode: RoutineMode) -> String {
        let tool = self.merge.is_some();
        let rules = match mode {
            RoutineMode::Report => "(scheduled run, report only)\n\
                 Investigate and report what you find. Do not push, merge, open, close or \
                 comment on PRs or issues, deploy, or change any other remote state."
                .to_string(),
            RoutineMode::DryRun => {
                let merge = match tool {
                    true => format!(
                        " For each PR you would merge, call `{MERGE_TOOL}` (repo, number): \
                         in a dry run it only says whether this routine's merge rules would \
                         let it merge, and why not."
                    ),
                    false => String::new(),
                };
                format!(
                    "(scheduled run, dry run)\n\
                     This routine may act, but this run is a dry run, so the user can judge it \
                     before letting it act. Investigate, then list exactly the actions you would \
                     take: each command, push, comment or label, with its full text. Perform none \
                     of them: change no files, commits, PRs, issues or other state.{merge}"
                )
            }
            RoutineMode::Live => {
                let merge = match tool {
                    true => format!(
                        "Merge PRs only by calling `{MERGE_TOOL}` (repo, number), never with \
                         gh, git or the API: it checks this routine's merge rules, may ask the \
                         user, and merges only if they pass. Report what it answered for each \
                         PR. Never push tags"
                    ),
                    false => "Never merge PRs, push tags".to_string(),
                };
                format!(
                    "(scheduled run, live)\n\
                     You may act as the instructions below say, and only that far. {merge}, \
                     create releases, or read, change or print secrets or credentials. Report \
                     every action you take (command, push, comment, label), and call \
                     `report_evidence` for each PR, commit or comment you made."
                )
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
            merge: r.merge.clone(),
            changed: s.changed,
            pin: Some(r.pin()),
            prompt: Some(r.prompt.clone()),
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
            merge: None,
            changed: false,
            pin: None,
            prompt: None,
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
/// (may act once switched live; see [`Routine::mode`]), and `merge:` with its keys
/// on indented lines below (see [`parse_merge`]). ` #` starts a comment.
pub fn parse(file: &str, text: &str) -> Result<Routine, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim) != Some("---") {
        return Err("must start with a `---` frontmatter line".into());
    }
    let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
    let mut merge: Option<BTreeMap<&str, &str>> = None;
    // The top-level key the indented lines below it belong to.
    let mut block = None;
    let mut body = None;
    let mut at = text.find('\n').map_or(text.len(), |i| i + 1);
    for raw in lines {
        at += raw.len();
        let line = raw.trim();
        if line == "---" {
            body = Some(&text[at..]);
            break;
        }
        let line = strip_comment(line);
        if line.is_empty() {
            continue;
        }
        let indented = raw.starts_with([' ', '\t']);
        if line.starts_with("- ") || line == "-" {
            return Err(format!("`{line}`: write lists as `[a, b]`"));
        }
        let (k, v) =
            (line.split_once(':')).ok_or_else(|| format!("`{line}`: want `key: value`"))?;
        let (k, v) = (k.trim(), v.trim());
        if indented {
            let Some(keys) = merge.as_mut().filter(|_| block == Some("merge")) else {
                return Err(format!("`{line}`: indented, but not under `merge:`"));
            };
            if !MERGE_KEYS.contains(&k) {
                return Err(format!(
                    "merge: unknown key `{k}` (known: {})",
                    MERGE_KEYS.join(", ")
                ));
            }
            if keys.insert(k, v).is_some() {
                return Err(format!("merge: `{k}` is set twice"));
            }
            continue;
        }
        const KEYS: [&str; 10] = [
            "name", "every", "cron", "harness", "model", "worker", "cwd", "paused", "act", "merge",
        ];
        if !KEYS.contains(&k) {
            return Err(format!("unknown key `{k}` (known: {})", KEYS.join(", ")));
        }
        if keys.insert(k, unquote(v)).is_some() {
            return Err(format!("`{k}` is set twice"));
        }
        block = Some(k);
        if k == "merge" {
            if !v.is_empty() {
                return Err("merge: put its keys on indented lines below it".into());
            }
            merge = Some(BTreeMap::new());
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
    let merge = merge.map(|m| parse_merge(&m)).transpose()?;
    if merge.is_some() && !act {
        return Err("merge needs `act: true`: report-only runs never merge".into());
    }
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
        merge,
        prompt,
    })
}

/// A new routine's file name and text. It starts paused and report-only, so
/// nothing runs until the user resumes it. Parsed back to check it.
pub fn new_file(new: &NewRoutine) -> Result<(String, String), String> {
    // Written in canonical form, on one line, so nothing in it can start another key.
    let schedule = new.schedule.trim();
    let trigger = match parse_every(schedule) {
        Ok(ms) => format!("every: {}", fmt_every(ms)),
        Err(_) if schedule.split_whitespace().count() == 5 => {
            format!("cron: \"{}\"", schedule.parse::<Cron>()?.as_str())
        }
        Err(_) => {
            return Err(format!(
                "schedule `{schedule}`: want e.g. 30m, 6h, 1d or a cron line like `0 9 * * 1-5`"
            ))
        }
    };
    let file = format!("{}.md", new.name.trim());
    let text = format!("---\n{trigger}\npaused: true\n---\n{}\n", new.prompt.trim());
    parse(&file, &text)?;
    Ok((file, text))
}

const MERGE_KEYS: [&str; 8] = [
    "repos",
    "max_files",
    "max_lines",
    "authors",
    "forbid_paths",
    "require_labels",
    "method",
    "approve",
];

/// Most files `gh pr view --json files` lists; a PR with more can't be checked.
pub const MAX_MERGE_FILES: u32 = 100;

/// The `merge:` block's keys: `repos` (`owner/name`) and `authors` (logins as
/// `gh` gives them, e.g. `app/dependabot`), at least one each; `max_files` (at
/// most `MAX_MERGE_FILES`) and `max_lines`; optional `forbid_paths` (globs),
/// `require_labels`, `method` (squash, merge or rebase; default squash) and
/// `approve` (phone or none; default phone). Lists are `[a, b]`.
fn parse_merge(keys: &BTreeMap<&str, &str>) -> Result<MergePolicy, String> {
    let list = |k: &str| keys.get(k).map_or(Ok(vec![]), |v| parse_list(k, v));
    let num = |k: &str, max: u32| {
        let v = keys.get(k).map(|v| unquote(v));
        let n = v.ok_or_else(|| format!("merge: needs `{k}`"))?;
        match n.parse::<u32>() {
            Ok(n) if (1..=max).contains(&n) => Ok(n),
            _ => Err(format!(
                "merge: {k} `{n}`: want a whole number from 1 to {max}"
            )),
        }
    };
    let word = |k: &str, default: &'static str| keys.get(k).map_or(default, |v| unquote(v));
    let repos = list("repos")?;
    if repos.is_empty() {
        return Err("merge: needs `repos: [owner/name]`".into());
    }
    let repo_ok = |r: &str| {
        let part = |p: &str| {
            !p.is_empty()
                && !p.starts_with(['.', '-'])
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        r.split_once('/').is_some_and(|(o, n)| part(o) && part(n))
    };
    if let Some(bad) = repos.iter().find(|r| !repo_ok(r)) {
        return Err(format!("merge: repo `{bad}`: want owner/name"));
    }
    let authors = list("authors")?;
    if authors.is_empty() {
        return Err("merge: needs `authors: [login]`, whose PRs it may merge".into());
    }
    let login_ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '[' | ']');
    if let Some(bad) = authors.iter().find(|a| !a.chars().all(login_ok)) {
        return Err(format!("merge: author `{bad}`: want a GitHub login"));
    }
    let method = match word("method", "squash") {
        "squash" => MergeMethod::Squash,
        "merge" => MergeMethod::Merge,
        "rebase" => MergeMethod::Rebase,
        v => return Err(format!("merge: method `{v}`: want squash, merge or rebase")),
    };
    let approve = match word("approve", "phone") {
        "phone" => MergeApprove::Phone,
        "none" => MergeApprove::None,
        v => return Err(format!("merge: approve `{v}`: want phone or none")),
    };
    let forbid_paths = list("forbid_paths")?;
    // Classes and braces would match nothing here, leaving those paths open.
    if let Some(bad) = forbid_paths
        .iter()
        .find(|g| g.contains(['[', ']', '{', '}']))
    {
        return Err(format!(
            "merge: forbid_paths `{bad}`: use only `*` and `**`"
        ));
    }
    Ok(MergePolicy {
        repos,
        max_files: num("max_files", MAX_MERGE_FILES)?,
        max_lines: num("max_lines", u32::MAX)?,
        authors,
        forbid_paths,
        require_labels: list("require_labels")?,
        method,
        approve,
    })
}

/// `[a, "b c"]` -> `a`, `b c`; a bare value is a list of one.
fn parse_list(k: &str, v: &str) -> Result<Vec<String>, String> {
    let inner = match v.strip_prefix('[') {
        Some(rest) => (rest.strip_suffix(']'))
            .ok_or_else(|| format!("merge: {k}: a list needs its closing `]`"))?,
        None => v,
    };
    if inner.trim().is_empty() {
        return Ok(vec![]);
    }
    (inner.split(','))
        .map(|item| match (item.trim(), unquote(item.trim())) {
            (_, "") => Err(format!("merge: {k}: empty item")),
            // `"a,b"` was split at its comma: half-quoted, it would match nothing.
            (raw, same) if raw == same && raw.starts_with(['"', '\'']) => {
                Err(format!("merge: {k}: `{raw}`: no commas inside quotes"))
            }
            (_, item) => Ok(item.to_string()),
        })
        .collect()
}

/// `value  # note` -> `value`; a `#` inside quotes, or not after a space, stays.
pub(crate) fn strip_comment(line: &str) -> &str {
    if line.starts_with('#') {
        return "";
    }
    let mut quote = None;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') if prev.is_whitespace() => return line[..i].trim_end(),
            _ => {}
        }
        prev = c;
    }
    line
}

/// `"x"` or `'x'` -> `x`.
pub(crate) fn unquote(v: &str) -> &str {
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

    #[test]
    fn new_file_starts_paused_and_checks_its_fields() {
        let new = |name: &str, schedule: &str, prompt: &str| {
            new_file(&NewRoutine {
                name: name.into(),
                schedule: schedule.into(),
                prompt: prompt.into(),
            })
        };
        let (file, text) = new("deps", " 90m ", " Check deps.\n").unwrap();
        assert_eq!(file, "deps.md");
        assert_eq!(text, "---\nevery: 90m\npaused: true\n---\nCheck deps.\n");
        let r = parse(&file, &text).unwrap();
        assert!(r.paused && !r.act && r.name == "deps");

        // A schedule split over lines is joined into one, so it can't add a key.
        let (_, text) = new("nightly", "0 9 * *\n1-5", "Sweep.").unwrap();
        assert!(
            text.starts_with("---\ncron: \"0 9 * * 1-5\"\npaused: true\n"),
            "{text}"
        );
        assert!(new("nightly", "0 9 * *\nact: true", "Sweep.").is_err());
        assert!(new("x", "soon", "Sweep.")
            .unwrap_err()
            .contains("schedule `soon`"));
        assert!(new("x", "0 25 * * *", "Sweep.")
            .unwrap_err()
            .contains("hour `25`"));
        assert!(new("../x", "1d", "Sweep.").unwrap_err().contains("name"));
        assert!(new("x", "1d", "  ").unwrap_err().contains("no prompt"));
    }

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
        let act = routine("every: 1h\nact: true");
        let mut live = State::new(0);
        (live.live, live.live_pin) = (true, Some(act.pin()));
        let report = parse("x.md", FILE).unwrap();
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

    const MERGE: &str =
        "---\nevery: 1d\nact: true\nmerge:\n  repos: [tommy-xr/MakeItSo, \"o/r\"]\n  \
                         max_files: 10\n  max_lines: 300\n  \
                         authors: [app/dependabot, tommy-xr]      # PR author logins allowed\n  \
                         forbid_paths: [.github/workflows/**, release/**]\n  \
                         require_labels: [automerge]   # optional\n  method: rebase\n  \
                         approve: none\nharness: claude\n---\nMerge green dependabot PRs.";

    #[test]
    fn parses_a_merge_policy() {
        let r = parse("deps.md", MERGE).unwrap();
        let m = r.merge.clone().unwrap();
        assert_eq!(m.repos, ["tommy-xr/MakeItSo", "o/r"]);
        assert_eq!((m.max_files, m.max_lines), (10, 300));
        assert_eq!(m.authors, ["app/dependabot", "tommy-xr"]);
        assert_eq!(m.forbid_paths, [".github/workflows/**", "release/**"]);
        assert_eq!(m.require_labels, ["automerge"]);
        assert_eq!(
            (m.method, m.approve),
            (MergeMethod::Rebase, MergeApprove::None)
        );
        assert_eq!(r.harness.as_deref(), Some("claude"), "keys after the block");
        assert_eq!(r.merge_rule(), MergeRule::Tool(Box::new(m)));
        // Defaults: squash, approved on the phone; no paths or labels required.
        let min = "---\nevery: 1d\nact: true\nmerge:\n\trepos: o/r\n\tauthors: [me]\n\t\
                   max_files: 1\n\tmax_lines: 1\n---\ngo";
        let m = parse("r.md", min).unwrap().merge.unwrap();
        assert_eq!(
            (m.method, m.approve),
            (MergeMethod::Squash, MergeApprove::Phone)
        );
        assert!(m.forbid_paths.is_empty() && m.require_labels.is_empty());
        assert_eq!(
            routine("every: 1h\nact: true").merge_rule(),
            MergeRule::Never
        );
    }

    #[test]
    fn bad_merge_policies_say_why() {
        let base = |extra: &str| {
            format!(
                "---\nevery: 1d\nact: true\nmerge:\n  repos: [o/r]\n  authors: [me]\n  \
                 max_files: 5\n  max_lines: 50\n{extra}---\ngo"
            )
        };
        for (text, why) in [
            (base("  mehtod: squash\n"), "merge: unknown key `mehtod`"),
            (base("  repos: [x/y]\n"), "merge: `repos` is set twice"),
            (base("  method: octopus\n"), "method `octopus`"),
            (base("  approve: email\n"), "approve `email`"),
            (
                base("  forbid_paths: [.github/{a,b}]\n"),
                "use only `*` and `**`",
            ),
            (base("  forbid_paths: [a,,b]\n"), "empty item"),
            (
                base("  forbid_paths: [\"a,b\"]\n"),
                "no commas inside quotes",
            ),
            (base("  require_labels: [a\n"), "closing `]`"),
            (base("  - x\n"), "write lists as `[a, b]`"),
            (base("every2: 1h\n  x: y\n"), "unknown key `every2`"),
            (
                "---\nevery: 1d\nact: true\nmerge: {repos: [o/r]}\n---\ngo".into(),
                "indented lines below",
            ),
            (
                "---\nevery: 1d\n  repos: [o/r]\n---\ngo".into(),
                "not under `merge:`",
            ),
            (
                base("").replace("act: true\n", ""),
                "merge needs `act: true`",
            ),
            (base("").replace("  repos: [o/r]\n", ""), "needs `repos"),
            (
                base("").replace("[o/r]", "[o]"),
                "repo `o`: want owner/name",
            ),
            (base("").replace("[o/r]", "[--admin/r]"), "repo `--admin/r`"),
            (base("").replace("  authors: [me]\n", ""), "needs `authors"),
            (base("").replace("[me]", "[\"me; rm\"]"), "author `me; rm`"),
            (
                base("").replace("  max_files: 5\n", ""),
                "needs `max_files`",
            ),
            (
                base("").replace("max_files: 5", "max_files: 0"),
                "max_files `0`",
            ),
            (
                base("").replace("max_files: 5", "max_files: 101"),
                "from 1 to 100",
            ),
            (
                base("").replace("max_lines: 50", "max_lines: lots"),
                "max_lines `lots`",
            ),
        ] {
            let err = parse("r.md", &text).unwrap_err();
            assert!(err.contains(why), "{text:?}: {err}");
        }
        // A `#` without a space before it is part of the value.
        let r = parse("r.md", "---\nevery: 1h\nmodel: a#b # note\n---\ngo").unwrap();
        assert_eq!(r.model.as_deref(), Some("a#b"));
    }

    #[test]
    fn going_live_pins_the_prompt_policy_and_placement() {
        let r = parse("deps.md", MERGE).unwrap();
        let mut s = State::new(0);
        (s.live, s.live_pin) = (true, Some(r.pin()));
        assert_eq!(r.mode(&s), RoutineMode::Live);
        // The schedule and pause aren't pinned...
        let same = parse(
            "deps.md",
            &MERGE.replace("every: 1d", "every: 2h\npaused: true"),
        )
        .unwrap();
        assert_eq!(same.mode(&s), RoutineMode::Live);
        // ...but each of these is.
        for (from, to) in [
            ("Merge green", "Merge any"),
            ("max_lines: 300", "max_lines: 3000"),
            ("approve: none", "approve: phone"),
            ("harness: claude", "harness: codex"),
            ("harness: claude", "harness: claude\nworker: other"),
            ("harness: claude", "harness: claude\ncwd: /elsewhere"),
            ("harness: claude", "harness: claude\nmodel: opus"),
        ] {
            let edited = parse("deps.md", &MERGE.replace(from, to)).unwrap();
            assert_eq!(edited.mode(&s), RoutineMode::DryRun, "{from} -> {to}");
        }
        // A live state from before pins existed has none: dry run.
        s.live_pin = None;
        assert_eq!(r.mode(&s), RoutineMode::DryRun);
        let v = view(&Ok(r.clone()), None, Runs::default(), None, 0, &Utc);
        assert_eq!(v.pin.as_deref(), Some(r.pin().as_str()));
        assert_eq!(v.merge, r.merge);
        assert_eq!(r.pin().len(), 64);
    }

    #[test]
    fn merge_runs_are_told_to_use_merge_pr() {
        let r = parse("deps.md", MERGE).unwrap();
        let live = r.run_prompt(RoutineMode::Live);
        assert!(
            live.contains("Merge PRs only by calling `merge_pr`"),
            "{live}"
        );
        assert!(live.contains("Never push tags, create releases"));
        let dry = r.run_prompt(RoutineMode::DryRun);
        assert!(dry.contains("call `merge_pr`") && dry.contains("only says whether"));
        for text in [live, dry] {
            assert_eq!(
                text.split_once("\n\n").unwrap().1,
                "Merge green dependabot PRs."
            );
        }
    }
}

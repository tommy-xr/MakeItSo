//! Publishes capture receipts (`<state>/captures/inbox/`) as records in a clone of the
//! notes repo (`inbox/`), by capture id: each is written once, then left to the human,
//! who may edit, rename, file or delete it. Choosing what to write is pure ([`plan`],
//! [`record`]); [`Notes`] is the `git` shell around it, and also files inbox notes
//! ([`Notes::file`], laid out by [`crate::filing`]), so all its `git` runs one at a time.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mis_types::{NoteMove, NoteMoved, NotesSync, SyncState};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Where records go in the repo.
const INBOX: &str = "inbox";
/// The remote branch published to.
const BRANCH: &str = "main";
/// Times a push refused because someone pushed first is retried, after a rebase.
const PUSH_RETRIES: usize = 3;
/// Trailer naming each record a commit publishes: `MIS-Capture: <id> <path>`.
const CAPTURE_TRAILER: &str = "MIS-Capture";
/// In the clone's `.git`: the files a run is writing, until committed.
const WRITING: &str = "mis-writing";
/// Any one `git` command is killed after this long.
const GIT_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// A run waits this long after a capture, so a burst lands in one commit.
pub const AFTER_CAPTURE: Duration = Duration::from_secs(30);
/// And runs this often regardless.
pub const EVERY: Duration = Duration::from_secs(10 * 60);

/// A capture kept in the state dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub id: Uuid,
    /// `.md` file name, e.g. `2026-11-08-foo.md`.
    pub file: String,
    /// Its images' file names, e.g. `2026-11-08-foo-1.jpg`.
    pub assets: Vec<String>,
}

/// Where a capture was published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    /// Repo path it was written to, e.g. `inbox/2026-11-08-foo.md`.
    pub path: String,
    /// Remote commit that published it (or held it, when adopted).
    pub commit: String,
}

/// `<state>/captures/published.json`: capture id -> where it went.
pub type Map = BTreeMap<Uuid, Published>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write the receipt as record `path`.
    Write { id: Uuid, path: String },
    /// Already at `path` in the repo (pushed, then the map wasn't saved): record it only.
    Adopt { id: Uuid, path: String },
    /// Published before. The repo's copy, if any, is the human's now.
    Skip { id: Uuid },
}

/// What to do with each receipt, given the map and the repo's `inbox/` files (path ->
/// record id; None for images and files without one). A new record keeps the receipt's
/// name unless another file uses it (see [`crate::capture::free_stem`]): then `-2`, `-3`, …
pub fn plan(receipts: &[Receipt], map: &Map, repo: &BTreeMap<String, Option<Uuid>>) -> Vec<Action> {
    let mut at: BTreeMap<Uuid, &String> = BTreeMap::new();
    for (path, id) in repo {
        if let Some(id) = id {
            at.entry(*id).or_insert(path);
        }
    }
    let prefix = format!("{INBOX}/");
    let mut names: Vec<String> = (repo.keys())
        .map(|p| p.strip_prefix(&prefix).unwrap_or(p).to_string())
        .collect();
    let mut actions = Vec::new();
    for r in receipts {
        let id = r.id;
        if map.contains_key(&id) {
            actions.push(Action::Skip { id });
        } else if let Some(path) = at.get(&id) {
            let path = path.to_string();
            actions.push(Action::Adopt { id, path });
        } else {
            let from = stem(&r.file);
            let to = crate::capture::free_stem(from, &names);
            names.extend(r.assets.iter().map(|a| renamed(a, from, &to)));
            names.push(format!("{to}.md"));
            let path = format!("{prefix}{to}.md");
            actions.push(Action::Write { id, path });
        }
    }
    actions
}

fn stem(file: &str) -> &str {
    file.strip_suffix(".md").unwrap_or(file)
}

/// `asset` of a receipt named `from` renamed for `to`: `a-1.jpg` -> `a-2-1.jpg`.
pub(crate) fn renamed(asset: &str, from: &str, to: &str) -> String {
    match asset.strip_prefix(from) {
        Some(rest) => format!("{to}{rest}"),
        None => asset.to_string(),
    }
}

/// Frontmatter `lines` (ending in `eol`) with each image line `  - path: <a>` naming `b`
/// instead, for each (a, b) in `renames`. Each line is renamed once: `a.jpg` -> `a-2.jpg`
/// never then follows `a-2.jpg` -> `a-2-2.jpg`.
pub(crate) fn rename_images<'a>(
    lines: &str,
    eol: &str,
    renames: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    let renames: BTreeMap<&str, &str> = renames.into_iter().collect();
    (lines.split_inclusive('\n'))
        .map(|l| {
            let name = l
                .strip_prefix("  - path: ")
                .and_then(|n| n.strip_suffix(eol));
            match name.and_then(|n| renames.get(n)) {
                Some(b) => format!("  - path: {b}{eol}"),
                None => l.to_string(),
            }
        })
        .collect()
}

/// Receipt `r` (text `md`) written as `path`: the record's text, and each image as
/// (receipt file name, repo path). Renamed images are renamed in the frontmatter too.
pub fn record(md: &str, r: &Receipt, path: &str) -> (String, Vec<(String, String)>) {
    let name = path.strip_prefix(&format!("{INBOX}/")).unwrap_or(path);
    let (from, to) = (stem(&r.file), stem(name));
    let names: Vec<(&str, String)> = (r.assets.iter())
        .map(|a| (a.as_str(), renamed(a, from, to)))
        .collect();
    let images = (names.iter())
        .map(|(a, b)| (a.to_string(), format!("{INBOX}/{b}")))
        .collect();
    // Only the frontmatter: the text below it is the human's words.
    let (end, eol) = crate::capture::front(md).map_or((md.len(), "\n"), |f| (f.end, f.eol));
    let renames = names.iter().map(|(a, b)| (*a, b.as_str()));
    (rename_images(&md[..end], eol, renames) + &md[end..], images)
}

/// The commit message publishing `writes` (id, path).
fn message(writes: &[(Uuid, &str)]) -> String {
    let n = writes.len();
    let mut msg = format!("capture: {n} item{}\n\n", if n == 1 { "" } else { "s" });
    msg += "MIS-Writer: capture\n";
    for (id, path) in writes {
        msg += &format!("{CAPTURE_TRAILER}: {id} {path}\n");
    }
    msg
}

/// Every capture a history published, from `git log --format=` [`LOG_FORMAT`]: the repo
/// remembers what it was given even if the map is lost and the records since moved.
fn published_in(log: &str) -> Map {
    let mut map = Map::new();
    let mut commit = "";
    for line in log.lines() {
        if let Some(c) = line.strip_prefix("commit ") {
            commit = c;
        } else if let Some((id, path)) = line.split_once(' ') {
            if let Ok(id) = id.parse() {
                let (path, commit) = (path.to_string(), commit.to_string());
                map.entry(id).or_insert(Published { path, commit });
            }
        }
    }
    map
}

const LOG_FORMAT: &str = "--format=commit %H%n%(trailers:key=MIS-Capture,valueonly)";

/// Whether `git push --porcelain` says someone pushed first, so a rebase and retry may
/// succeed. Seen with git 2.54: `[rejected] (fetch first)` after a stale fetch, `[remote
/// rejected] (incorrect old value provided)` for a push racing ours.
fn raced(stdout: &str) -> bool {
    let reasons = [
        "(fetch first)",
        "(non-fast-forward)",
        "(incorrect old value provided)",
    ];
    (stdout.lines())
        .filter(|l| l.starts_with("!\t"))
        .any(|l| reasons.iter().any(|r| l.ends_with(r)))
}

/// What the status shows after a run that left `pending` captures.
fn state(pending: usize, run: &Result<bool, Failure>) -> SyncState {
    match run {
        Err(Failure::Offline(_)) => SyncState::Offline,
        Err(Failure::Conflict(_)) => SyncState::Conflict,
        Err(Failure::Other(_)) => SyncState::Pending,
        Ok(_) if pending > 0 => SyncState::Pending,
        Ok(_) => SyncState::Idle,
    }
}

/// Why a note wasn't filed.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not a note and folder [`crate::filing::check`] passes, or not text.
    Invalid(String),
    /// Gone, changed since it was read, too large, its folder gone, a symlink or a file
    /// the clone doesn't track in its way, a picture another note lists, or frontmatter it
    /// can't safely edit.
    Stale(String),
    /// The clone couldn't be used: offline, a conflict, git failing.
    Failed(String),
}

impl From<Failure> for Refused {
    fn from(f: Failure) -> Self {
        match f {
            Failure::Offline(why) => {
                Self::Failed(format!("offline; try again when connected ({why})"))
            }
            f => Self::Failed(f.why().to_string()),
        }
    }
}

/// How a queued filing went: committed (pushed now, or by a later run if this push failed)
/// or not.
pub type Filed = Result<NoteMoved, Refused>;

#[derive(Debug)]
enum Failure {
    /// The remote couldn't be reached.
    Offline(String),
    Conflict(String),
    Other(String),
}

impl Failure {
    fn why(&self) -> &str {
        match self {
            Self::Offline(w) | Self::Conflict(w) | Self::Other(w) => w,
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.to_string())
    }
}

/// One `git` command's result.
struct Out {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    /// Ok if it succeeded, else `fail` with what git said.
    fn check(self, what: &str, fail: fn(String) -> Failure) -> Result<Self, Failure> {
        match self.ok {
            true => Ok(self),
            false => Err(fail(format!("{what}: {}", self.stderr.trim()))),
        }
    }
}

/// The publisher: the clone at `<state>/notes` and the status `GET /api/notes/sync` shows.
pub struct Notes {
    remote: String,
    clone: PathBuf,
    receipts: PathBuf,
    map: PathBuf,
    status: Mutex<NotesSync>,
    /// Captures kept since start, so ones arriving mid-run still count as pending.
    arrived: AtomicUsize,
    /// Woken by each new capture.
    kick: tokio::sync::Notify,
    /// Filings for the next run, each with whoever awaits it.
    moves: Mutex<Vec<(NoteMove, oneshot::Sender<Filed>)>>,
    /// Where notes may be filed, as of the last run's HEAD.
    dests: Mutex<Vec<String>>,
    /// Woken by each filing queued.
    queued: tokio::sync::Notify,
    after_capture: Duration,
    every: Duration,
}

impl Notes {
    /// Publish `state_dir`'s captures to `remote`, cloned at `<state_dir>/notes`.
    pub fn new(remote: String, state_dir: &Path) -> Self {
        Self {
            remote,
            clone: state_dir.join("notes"),
            receipts: state_dir.join("captures").join("inbox"),
            map: state_dir.join("captures").join("published.json"),
            status: Mutex::new(NotesSync {
                state: SyncState::Idle,
                ..NotesSync::OFF
            }),
            arrived: AtomicUsize::new(0),
            kick: Default::default(),
            moves: Default::default(),
            dests: Default::default(),
            queued: Default::default(),
            after_capture: AFTER_CAPTURE,
            every: EVERY,
        }
    }

    /// Run `after_capture` after a capture and every `every` (tests).
    pub fn with_timing(mut self, after_capture: Duration, every: Duration) -> Self {
        self.after_capture = after_capture;
        self.every = every;
        self
    }

    /// The clone, `<state>/notes`: absent until the first run clones it.
    pub fn clone_dir(&self) -> &Path {
        &self.clone
    }

    pub fn status(&self) -> NotesSync {
        self.status.lock().unwrap().clone()
    }

    /// Where notes may be filed ([`crate::filing::dests`]) as of the last run's HEAD.
    pub fn dests(&self) -> Vec<String> {
        self.dests.lock().unwrap().clone()
    }

    /// A new capture was kept: count it and publish soon.
    pub fn captured(&self) {
        let mut s = self.status.lock().unwrap();
        self.arrived.fetch_add(1, Ordering::SeqCst);
        s.pending += 1;
        if s.state == SyncState::Idle {
            s.state = SyncState::Pending;
        }
        drop(s);
        self.kick.notify_one();
    }

    /// File `m` ([`crate::filing::check`]ed) on the next run, which starts now.
    pub fn file(&self, m: NoteMove) -> oneshot::Receiver<Filed> {
        let (tx, rx) = oneshot::channel();
        self.moves.lock().unwrap().push((m, tx));
        self.queued.notify_one();
        rx
    }

    /// Publish at start, `after_capture` after each capture, every `every`, and at once
    /// when a filing is queued. The only caller of [`Self::run`], so runs never overlap.
    pub async fn publish_loop(self: std::sync::Arc<Self>) {
        let mut every = tokio::time::interval(self.every);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = every.tick() => {}
                _ = self.queued.notified() => {}
                _ = self.kick.notified() => tokio::select! {
                    _ = tokio::time::sleep(self.after_capture) => {}
                    _ = self.queued.notified() => {}
                },
            }
            let notes = self.clone();
            let _ = tokio::task::spawn_blocking(move || notes.run()).await;
        }
    }

    /// The queued filings, then one publish (blocking: runs `git`), then the status it leaves.
    pub fn run(&self) -> NotesSync {
        let moves = std::mem::take(&mut *self.moves.lock().unwrap());
        let mut filed = Vec::new();
        for (m, tx) in moves {
            match self.file_now(&m) {
                Ok(path) => filed.push((path, tx)),
                Err(r) => {
                    tracing::warn!(path = %m.path, dest = %m.dest, "filing a note: {r:?}");
                    let _ = tx.send(Err(r));
                }
            }
        }
        let before = self.arrived.load(Ordering::SeqCst);
        let (pending, run) = self.publish();
        // The publish pushes the filings' commits, or leaves them for a later run.
        for (path, tx) in filed {
            let _ = tx.send(Ok(NoteMoved { path }));
        }
        if let Ok(modes) = self.dest_modes() {
            *self.dests.lock().unwrap() = crate::filing::dests(&modes);
        }
        let mut s = self.status.lock().unwrap();
        // Captures kept mid-run may postdate the receipts it read. Counted before
        // reading them, so at worst one is counted twice until the next run.
        if let Some(pending) = pending {
            s.pending = pending + (self.arrived.load(Ordering::SeqCst) - before);
        }
        s.state = state(s.pending, &run);
        match &run {
            Ok(pushed) => {
                if *pushed {
                    s.last_push = Some(crate::unix_ms());
                }
                s.last_error = None;
            }
            Err(f) => {
                tracing::warn!(state = ?s.state, "publishing captures: {}", f.why());
                s.last_error = Some(f.why().to_string());
            }
        }
        s.clone()
    }

    /// Captures still unpublished after the run (None: couldn't count them), and whether
    /// it pushed.
    fn publish(&self) -> (Option<usize>, Result<bool, Failure>) {
        let (receipts, mut map) = match self.read() {
            Ok(read) => read,
            Err(f) => return (None, Err(f)),
        };
        let unpublished = |map: &Map| {
            (receipts.iter())
                .filter(|r| !map.contains_key(&r.id))
                .count()
        };
        let pending = unpublished(&map);
        if pending == 0 {
            // Nothing to publish; still catch the clone up, which the Notes view reads, and
            // push what an offline run committed (a filing).
            return (Some(0), self.sync().and_then(|()| self.push(|| Ok(()))));
        }
        match self.publish_new(&receipts, &mut map) {
            Ok(pushed) => (Some(unpublished(&map)), Ok(pushed)),
            Err(f) => (Some(pending), Err(f)),
        }
    }

    /// The receipts, by file name, and the published map.
    fn read(&self) -> Result<(Vec<Receipt>, Map), Failure> {
        let mut receipts = Vec::new();
        let entries = match std::fs::read_dir(&self.receipts) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            r => r?.flatten().collect(),
        };
        for e in entries {
            let file = e.file_name().to_string_lossy().into_owned();
            if !file.ends_with(".md") || file.starts_with('.') {
                continue;
            }
            let md = std::fs::read_to_string(e.path())?;
            if let Some(k) = crate::capture::parse(&md) {
                let assets = k.assets;
                receipts.push(Receipt {
                    id: k.id,
                    file,
                    assets,
                });
            }
        }
        receipts.sort_by(|a, b| a.file.cmp(&b.file));
        let map = match std::fs::read(&self.map) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
            // A corrupt map is never overwritten, though history would rebuild most of it.
            r => serde_json::from_slice(&r?)
                .map_err(|e| Failure::Other(format!("{}: {e}", self.map.display())))?,
        };
        Ok((receipts, map))
    }

    /// Bring the clone up to date, commit what [`plan`] says to write, push, and only
    /// then record it in `map` and save that. Ok(true): it pushed.
    fn publish_new(&self, receipts: &[Receipt], map: &mut Map) -> Result<bool, Failure> {
        self.sync()?;
        let mut actions = Vec::new();
        let pushed = self.push(|| {
            // Published per history counts as published, map or not.
            let mut known = self.history()?;
            known.extend(map.iter().map(|(id, p)| (*id, p.clone())));
            actions = plan(receipts, &known, &self.scan()?);
            self.commit(receipts, &actions)
        })?;
        // HEAD is the remote's now: what its history published, plus what was adopted.
        for (id, p) in self.history()? {
            map.entry(id).or_insert(p);
        }
        let head = self.git(&["rev-parse", "HEAD"])?;
        let commit = head.check("rev-parse", Failure::Other)?.text();
        for a in actions {
            if let Action::Adopt { id, path } = a {
                let commit = commit.clone();
                map.entry(id).or_insert(Published { path, commit });
            }
        }
        let json = serde_json::to_vec_pretty(map).expect("serializes");
        crate::push::write_private(&self.map, &json)?;
        if pushed {
            tracing::info!(%commit, "published captures to the notes repo");
        }
        Ok(pushed)
    }

    /// `commit`, then push HEAD if that leaves it ahead of the remote. If someone pushed
    /// first, rebase onto theirs and do both again, up to [`PUSH_RETRIES`] times. Ok(true):
    /// it pushed.
    fn push(&self, mut commit: impl FnMut() -> Result<(), Failure>) -> Result<bool, Failure> {
        let mut retries = 0;
        loop {
            commit()?;
            let ahead = self.git(&["rev-list", "--count", &format!("origin/{BRANCH}..HEAD")])?;
            if ahead.check("rev-list", Failure::Other)?.text() == "0" {
                return Ok(false);
            }
            let to = format!("HEAD:refs/heads/{BRANCH}");
            let push = self.git(&["push", "--porcelain", "origin", &to])?;
            if push.ok {
                return Ok(true);
            }
            if !raced(&String::from_utf8_lossy(&push.stdout)) {
                return Err(Failure::Offline(format!("push: {}", push.stderr.trim())));
            }
            if retries == PUSH_RETRIES {
                let why = format!(
                    "push refused after {PUSH_RETRIES} retries: {}",
                    push.stderr.trim()
                );
                return Err(Failure::Other(why));
            }
            retries += 1;
            self.fetch_rebase()?;
        }
    }

    /// Clone if needed, point at the configured remote, remove what an interrupted run
    /// left uncommitted, then fetch and rebase. Anything else uncommitted is a human's:
    /// an open rebase, merge or cherry-pick is a conflict, other changes stop the rebase.
    fn sync(&self) -> Result<(), Failure> {
        let git_dir = self.clone.join(".git");
        if !git_dir.exists() {
            // Into a temp dir first, so a clone cut short is never taken for one.
            let tmp = self.clone.with_extension("tmp");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            let clone = [
                "clone",
                "--quiet",
                "--branch",
                BRANCH,
                "--",
                &self.remote,
                ".",
            ];
            let out = self.git_in(&tmp, &clone)?;
            out.check("clone", Failure::Offline)?;
            std::fs::rename(&tmp, &self.clone)?;
        }
        let open = [
            "rebase-merge",
            "rebase-apply",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
        ];
        if open.iter().any(|f| git_dir.join(f).exists()) {
            return Err(self.conflict());
        }
        let branch = self.git(&["symbolic-ref", "--short", "-q", "HEAD"])?.text();
        if branch != BRANCH {
            let why = format!("{} is on {branch:?}, not {BRANCH}", self.clone.display());
            return Err(Failure::Other(why));
        }
        let set = self.git(&["remote", "set-url", "origin", &self.remote])?;
        set.check("remote set-url", Failure::Other)?;
        self.undo_writing()?;
        self.fetch_rebase()
    }

    /// File `m` in a commit of its own, built from HEAD without touching the working tree,
    /// then fast-forwarded onto (git refuses if that would overwrite a file it doesn't
    /// track), left for [`Self::publish`] to push: the note's new path.
    fn file_now(&self, m: &NoteMove) -> Result<String, Refused> {
        crate::filing::check(m).map_err(Refused::Invalid)?;
        self.sync()?;
        let inbox = self.modes(&[&format!("{INBOX}/")])?;
        if !inbox.contains_key(&m.path) {
            return Err(Refused::Stale(format!("{} is gone", m.path)));
        }
        if !crate::filing::dests(&self.dest_modes()?).contains(&m.dest) {
            let why = format!("{} is not a folder to file in", m.dest);
            return Err(Refused::Stale(why));
        }
        // The page hashed the clean tree's bytes as the Notes view serves them (HEAD's blob
        // may differ by an `eol` or smudge filter), which stops at `MAX_BYTES`.
        let Some(shown) = crate::apps::read_file(&self.clone, &m.path) else {
            return Err(Refused::Stale(format!("{} can't be read", m.path)));
        };
        if shown.len() as u64 >= crate::apps::MAX_BYTES {
            let mib = crate::apps::MAX_BYTES >> 20;
            return Err(Refused::Stale(format!("{} is over {mib} MiB", m.path)));
        }
        let hash = crate::capture::sha256(&shown);
        if m.hash
            .as_ref()
            .is_some_and(|h| !h.eq_ignore_ascii_case(&hash))
        {
            let why = format!("{} changed since it was read", m.path);
            return Err(Refused::Stale(why));
        }
        let blob = self.git(&["cat-file", "blob", &format!("HEAD:{}", m.path)])?;
        let blob = blob.check("cat-file", Failure::Other)?.stdout;
        let md = String::from_utf8(blob).map_err(|_| Refused::Invalid("not UTF-8".into()))?;
        // The pictures the inbox's other notes list (read as the Notes view reads files).
        let note = |p: &&String| crate::notes_view::kind(p, false) == mis_types::NoteKind::Md;
        let others = (inbox.keys()).filter(|p| *p != &m.path).filter(note);
        let claimed: Vec<String> = others
            .filter_map(|p| crate::apps::read_file(&self.clone, p))
            .flat_map(|b| {
                let md = String::from_utf8_lossy(&b);
                crate::capture::front(&md).map_or(vec![], |f| crate::capture::assets(f.lines))
            })
            .collect();
        // What's in the folder, committed or not, so a filing never lands on a human's file.
        let pre = format!("{}/", m.dest);
        let mut taken: Vec<String> = (self.modes(&[&pre])?.into_keys())
            .map(|p| p[pre.len()..].to_string())
            .collect();
        if let Ok(read) = std::fs::read_dir(self.clone.join(&m.dest)) {
            let names = read
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned());
            taken.extend(names);
        }
        let f = crate::filing::plan(m, &md, &inbox, &claimed, &taken).map_err(Refused::Stale)?;
        // Early, with a clear reason; the fast-forward refuses anyway.
        let created: Vec<String> = f.moves.iter().map(|(_, to)| to.clone()).collect();
        self.absent(&created)
            .map_err(|f| Refused::Stale(f.why().to_string()))?;
        // Git's attributes for a moved file must be its source's, or the checkout converts
        // it (line endings, a filter) and leaves the clone dirty.
        let paths: Vec<&str> = (f.moves.iter())
            .flat_map(|(from, to)| [from.as_str(), to.as_str()])
            .collect();
        let check = [
            &["check-attr", "-z", "text", "eol", "filter", "--"],
            paths.as_slice(),
        ];
        let attrs = self
            .git(&check.concat())?
            .check("check-attr", Failure::Other)?;
        // `<path> NUL <attribute> NUL <value> NUL`, per path and attribute.
        let attrs: BTreeMap<(String, String), String> = (attrs.stdout.split(|b| *b == 0))
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect::<Vec<_>>()
            .as_chunks::<3>()
            .0
            .iter()
            .map(|[path, attr, value]| ((path.clone(), attr.clone()), value.clone()))
            .collect();
        let differ = |(from, to): &&(String, String)| {
            ["text", "eol", "filter"].iter().any(|a| {
                attrs.get(&(from.clone(), a.to_string())) != attrs.get(&(to.clone(), a.to_string()))
            })
        };
        if let Some((from, to)) = f.moves.iter().find(differ) {
            let why = format!("{to} converts line endings differently from {from}");
            return Err(Refused::Stale(why));
        }
        let commit = self.commit_filing(&f, &inbox)?;
        // Never over an ignored file either.
        let ff = [
            "merge",
            "--ff-only",
            "--no-overwrite-ignore",
            "--quiet",
            &commit,
        ];
        let merge = self.git(&ff)?;
        if !merge.ok {
            return Err(Refused::Stale(format!("merge: {}", merge.stderr.trim())));
        }
        Ok(f.moves[0].1.clone())
    }

    /// Filing `f` as a commit on HEAD, built in an index of its own: the working tree and
    /// its index are untouched, so a run cut short leaves only an unreferenced commit.
    /// `inbox` gives the moved files' modes.
    fn commit_filing(
        &self,
        f: &crate::filing::Filing,
        inbox: &crate::filing::Modes,
    ) -> Result<String, Failure> {
        // In a folder of this run's own, removed however it ends, so nothing an earlier run
        // left (a lock) is in the way. Paths relative to the clone, where git runs.
        let dir = format!(".git/mis-filing-{}", Uuid::new_v4());
        std::fs::create_dir(self.clone.join(&dir))?;
        struct Remove(PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _remove = Remove(self.clone.join(&dir));
        let (index, note) = (format!("{dir}/index"), format!("{dir}/note"));
        let git = |args: &[&str]| -> Result<String, Failure> {
            let mut out = run_git(&self.clone, args, GIT_DEADLINE, Some(Path::new(&index)))?;
            out.stderr = out.stderr.replace(&self.remote, "<remote>");
            Ok(out.check(args[0], Failure::Other)?.text())
        };
        git(&["read-tree", "HEAD"])?;
        let sources: Vec<&str> = f.moves.iter().map(|(from, _)| from.as_str()).collect();
        git(&[
            &["update-index", "--force-remove", "--"],
            sources.as_slice(),
        ]
        .concat())?;
        // The note as edited, byte for byte; its pictures as HEAD has them.
        crate::push::write_private(&self.clone.join(&note), f.md.as_bytes())?;
        let mut blobs = vec![git(&["hash-object", "-w", "--no-filters", "--", &note])?];
        for (from, _) in &f.moves[1..] {
            blobs.push(git(&["rev-parse", "--verify", &format!("HEAD:{from}")])?);
        }
        for ((from, to), blob) in f.moves.iter().zip(&blobs) {
            let entry = format!("{},{blob},{to}", inbox[from]);
            git(&["update-index", "--add", "--cacheinfo", &entry])?;
        }
        let tree = git(&["write-tree"])?;
        git(&["commit-tree", &tree, "-p", "HEAD", "-m", &f.message])
    }

    /// Ok if none of `paths` is in the clone (not even as a dangling link, or a file where
    /// one of its folders would be).
    fn absent(&self, paths: &[String]) -> Result<(), Failure> {
        let gone = |p: &&String| {
            let at = std::fs::symlink_metadata(self.clone.join(p));
            at.is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        };
        match paths.iter().find(|p| !gone(p)) {
            Some(p) => Err(Failure::Other(format!("{p} exists"))),
            None => Ok(()),
        }
    }

    /// HEAD's modes [`crate::filing::dests`] reads: the folders and what's directly in
    /// them (one `ls-tree` lists either, not both).
    fn dest_modes(&self) -> Result<crate::filing::Modes, Failure> {
        let mut modes = self.modes(&["projects", "areas", "resources", "archive"])?;
        modes.extend(self.modes(&["projects/", "areas/", "resources/"])?);
        Ok(modes)
    }

    /// HEAD's entries that are `paths`, or directly in the folders `x/` names: path -> git
    /// mode, e.g. `040000` for a folder, `120000` for a symlink.
    fn modes(&self, paths: &[&str]) -> Result<crate::filing::Modes, Failure> {
        let args = [&["ls-tree", "-z", "HEAD", "--"], paths].concat();
        let ls = self.git(&args)?.check("ls-tree", Failure::Other)?;
        // Each `<mode> <type> <object>\t<path>`.
        Ok((ls.stdout.split(|b| *b == 0))
            .filter_map(|e| {
                let e = String::from_utf8_lossy(e);
                let (meta, path) = e.split_once('\t')?;
                Some((path.to_string(), meta.split(' ').next()?.to_string()))
            })
            .collect())
    }

    /// Unstage and delete the files a run cut short was writing, unless it committed them.
    fn undo_writing(&self) -> Result<(), Failure> {
        let marker = self.clone.join(".git").join(WRITING);
        let paths = match std::fs::read_to_string(&marker) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            r => r?,
        };
        let paths: Vec<&str> = paths.lines().filter(|p| !p.is_empty()).collect();
        // No paths would mean the whole tree to `reset` and `clean`.
        if !paths.is_empty() {
            for cmd in [["reset", "-q"].as_slice(), &["clean", "-fq"]] {
                let args: Vec<&str> = [cmd, &["--"], &paths].concat();
                self.git(&args)?.check(cmd[0], Failure::Other)?;
            }
        }
        std::fs::remove_file(marker)?;
        Ok(())
    }

    fn fetch_rebase(&self) -> Result<(), Failure> {
        let fetch = self.git(&["fetch", "--quiet", "origin"])?;
        fetch.check("fetch", Failure::Offline)?;
        let rebase = self.git(&["rebase", "--quiet", &format!("origin/{BRANCH}")])?;
        if rebase.ok {
            return Ok(());
        }
        let git_dir = self.clone.join(".git");
        if !git_dir.join("rebase-merge").exists() && !git_dir.join("rebase-apply").exists() {
            return Err(Failure::Other(format!("rebase: {}", rebase.stderr.trim())));
        }
        // Ours: `sync` refuses to start with one open. Back to the unpublished commit; it
        // stays pending until a human resolves it.
        let abort = self.git(&["rebase", "--abort"])?;
        abort.check("rebase --abort", Failure::Other)?;
        Err(self.conflict())
    }

    fn conflict(&self) -> Failure {
        let at = self.clone.display();
        Failure::Conflict(format!("notes repo conflict, resolve in {at}"))
    }

    /// What HEAD's history says it published.
    fn history(&self) -> Result<Map, Failure> {
        let log = self
            .git(&["log", LOG_FORMAT, "HEAD"])?
            .check("log", Failure::Other)?;
        Ok(published_in(&log.text()))
    }

    /// HEAD's `inbox/` files, with their record ids, plus any other file there (no id), so
    /// a new record never lands on a human's uncommitted one. Ids are read from the
    /// working tree: after a clean rebase or commit, its tracked files are HEAD's.
    fn scan(&self) -> Result<BTreeMap<String, Option<Uuid>>, Failure> {
        let paths = self.modes(&[&format!("{INBOX}/")])?.into_keys();
        let mut files: BTreeMap<String, Option<Uuid>> = paths
            .map(|p| {
                let md = (p.ends_with(".md"))
                    .then(|| std::fs::read_to_string(self.clone.join(&p)).ok())
                    .flatten();
                let id = md.as_deref().and_then(crate::capture::parse).map(|k| k.id);
                (p, id)
            })
            .collect();
        let untracked = match std::fs::read_dir(self.clone.join(INBOX)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            r => r?.flatten().collect(),
        };
        for e in untracked {
            let name = e.file_name().to_string_lossy().into_owned();
            files.entry(format!("{INBOX}/{name}")).or_insert(None);
        }
        Ok(files)
    }

    /// Write and commit the records `actions` says to write, if any.
    fn commit(&self, receipts: &[Receipt], actions: &[Action]) -> Result<(), Failure> {
        let mut files = Vec::new();
        let mut writes = Vec::new();
        for a in actions {
            let Action::Write { id, path } = a else {
                continue;
            };
            let r = (receipts.iter())
                .find(|r| r.id == *id)
                .expect("planned from these");
            let md = std::fs::read_to_string(self.receipts.join(&r.file))?;
            let (md, images) = record(&md, r, path);
            files.push((path.clone(), None));
            files.extend(images.into_iter().map(|(from, to)| (to, Some(from))));
            writes.push((r, *id, path.as_str(), md));
        }
        if writes.is_empty() {
            return Ok(());
        }
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        let marker = self.clone.join(".git").join(WRITING);
        crate::push::write_private(&marker, paths.join("\n").as_bytes())?;
        std::fs::create_dir_all(self.clone.join(INBOX))?;
        // New files only: a name taken since `scan` fails the run rather than lose a file.
        let create = |to: &str| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.clone.join(to))
        };
        for (to, from) in &files {
            if let Some(from) = from {
                let mut src = std::fs::File::open(self.receipts.join(from))?;
                std::io::copy(&mut src, &mut create(to)?)?;
            }
        }
        for (_, _, path, md) in &writes {
            std::io::Write::write_all(&mut create(path)?, md.as_bytes())?;
        }
        let add: Vec<&str> = [&["add", "--"], paths.as_slice()].concat();
        self.git(&add)?.check("add", Failure::Other)?;
        let msg = message(
            &writes
                .iter()
                .map(|(_, id, p, _)| (*id, *p))
                .collect::<Vec<_>>(),
        );
        // Only these paths, whatever else is staged.
        let commit: Vec<&str> =
            [&["commit", "--quiet", "-m", &msg, "--"], paths.as_slice()].concat();
        self.git(&commit)?.check("commit", Failure::Other)?;
        std::fs::remove_file(marker)?;
        Ok(())
    }

    fn git(&self, args: &[&str]) -> Result<Out, Failure> {
        self.git_in(&self.clone, args)
    }

    /// `git args` in `dir`, with the remote's URL kept out of what it says.
    fn git_in(&self, dir: &Path, args: &[&str]) -> Result<Out, Failure> {
        let mut out = run_git(dir, args, GIT_DEADLINE, None)?;
        out.stderr = out.stderr.replace(&self.remote, "<remote>");
        Ok(out)
    }
}

/// `git args` in `dir`, never waiting on a person: no prompts, askpass, editors, hooks or
/// signing, and killed (with whatever it started) after `deadline`. Commits are Make It
/// So's; the user's own git config (the credential helper) still applies.
fn run_git(
    dir: &Path,
    args: &[&str],
    deadline: Duration,
    index: Option<&Path>,
) -> std::io::Result<Out> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        // Autostash would carry a human's uncommitted edits through the rebase.
        .args(["-c", "core.askPass=", "-c", "rebase.autoStash=false"])
        // A stalled transfer (under 1 KB/s for a minute) fails sooner than the deadline.
        .args([
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=60",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        // Paths are file names, never globs.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env("GIT_AUTHOR_NAME", "Make It So")
        .env("GIT_AUTHOR_EMAIL", "mis@localhost")
        .env("GIT_COMMITTER_NAME", "Make It So")
        .env("GIT_COMMITTER_EMAIL", "mis@localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so the deadline also kills ssh and credential helpers.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd.spawn()?;
    let (stdout, stderr) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let start = Instant::now();
    // Done once git has exited and its output closed: a process it left holding the
    // pipes (an ssh master) is waited for only until the deadline too.
    let mut status = None;
    while status.is_none() || !stdout.is_finished() || !stderr.is_finished() {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if start.elapsed() >= deadline {
            kill_group(&child);
            let _ = child.kill();
            let _ = child.wait();
            // Readers still blocked (on a pipe held outside the group) are left behind.
            let why = format!("timed out after {}s", deadline.as_secs_f32());
            return Ok(Out {
                ok: false,
                stdout: vec![],
                stderr: why,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let stdout = stdout.join().expect("reader");
    let stderr = String::from_utf8_lossy(&stderr.join().expect("reader")).into_owned();
    Ok(Out {
        ok: status.is_some_and(|s| s.success()),
        stdout,
        stderr,
    })
}

/// Stop the process group `child` leads (see `process_group(0)` above): TERM first, so
/// git removes its lock files, then KILL what's left. Elsewhere only `child` is killed.
fn kill_group(child: &std::process::Child) {
    #[cfg(unix)]
    for (signal, then) in [(libc::SIGTERM, 200), (libc::SIGKILL, 0)] {
        // SAFETY: a plain syscall on the group spawned with this child; at worst it is gone.
        unsafe {
            libc::killpg(child.id() as i32, signal);
        }
        std::thread::sleep(Duration::from_millis(then));
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// Read `pipe` to the end on a thread, so neither pipe fills while we wait.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut out);
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn receipt(n: u128, file: &str, assets: &[&str]) -> Receipt {
        Receipt {
            id: id(n),
            file: file.into(),
            assets: assets.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn repo(files: &[(&str, Option<u128>)]) -> BTreeMap<String, Option<Uuid>> {
        (files.iter())
            .map(|(p, n)| (p.to_string(), n.map(id)))
            .collect()
    }

    fn published(path: &str) -> Published {
        Published {
            path: path.into(),
            commit: "c".into(),
        }
    }

    fn write(n: u128, path: &str) -> Action {
        Action::Write {
            id: id(n),
            path: path.into(),
        }
    }

    #[test]
    fn writes_new_receipts_under_their_own_names() {
        let rs = [receipt(1, "a.md", &["a-1.jpg"]), receipt(2, "b.md", &[])];
        let got = plan(&rs, &Map::new(), &repo(&[("inbox/.gitkeep", None)]));
        assert_eq!(got, [write(1, "inbox/a.md"), write(2, "inbox/b.md")]);
    }

    #[test]
    fn published_ids_are_skipped_whatever_the_repo_holds() {
        let rs = [receipt(1, "a.md", &[]), receipt(2, "b.md", &[])];
        let map = Map::from([
            (id(1), published("inbox/a.md")),
            (id(2), published("inbox/b.md")),
        ]);
        // 1 deleted by the human, 2 renamed: neither comes back.
        let got = plan(&rs, &map, &repo(&[("inbox/b-renamed.md", Some(2))]));
        assert_eq!(
            got,
            [Action::Skip { id: id(1) }, Action::Skip { id: id(2) }]
        );
    }

    #[test]
    fn a_record_already_pushed_is_adopted_wherever_it_is() {
        let rs = [receipt(1, "a.md", &[])];
        let got = plan(&rs, &Map::new(), &repo(&[("inbox/moved.md", Some(1))]));
        let path = "inbox/moved.md".to_string();
        assert_eq!(got, [Action::Adopt { id: id(1), path }]);
    }

    #[test]
    fn a_taken_name_gets_a_number() {
        let rs = [receipt(1, "a.md", &["a-1.jpg"])];
        // Another record's name, or no record's: either way not ours to overwrite.
        for other in [Some(9), None] {
            let got = plan(&rs, &Map::new(), &repo(&[("inbox/a.md", other)]));
            assert_eq!(got, [write(1, "inbox/a-2.md")], "{other:?}");
        }
        let held = repo(&[("inbox/a.md", Some(9)), ("inbox/a-2.md", Some(8))]);
        assert_eq!(plan(&rs, &Map::new(), &held), [write(1, "inbox/a-3.md")]);
        // An image name counts too, in any case.
        let image = repo(&[("inbox/A-1.JPG", None)]);
        assert_eq!(plan(&rs, &Map::new(), &image), [write(1, "inbox/a-2.md")]);
        // So do names this plan has already given out.
        let rs = [receipt(1, "a.md", &[]), receipt(2, "a-2.md", &[])];
        let got = plan(&rs, &Map::new(), &repo(&[("inbox/a.md", Some(9))]));
        assert_eq!(got, [write(1, "inbox/a-2.md"), write(2, "inbox/a-2-2.md")]);
    }

    #[test]
    fn record_renames_images_in_the_frontmatter_only() {
        let r = receipt(1, "a.md", &["a-1.jpg", "a-2.png"]);
        let md = "---\nid: x\nassets:\n  - path: a-1.jpg\n    sha256: h\n  - path: a-2.png\n    \
                  sha256: h\n---\n  - path: a-1.jpg\n";
        let (same, images) = record(md, &r, "inbox/a.md");
        assert_eq!(same, md);
        assert_eq!(
            images,
            [
                ("a-1.jpg".into(), "inbox/a-1.jpg".into()),
                ("a-2.png".into(), "inbox/a-2.png".into())
            ]
        );
        let (moved, images) = record(md, &r, "inbox/a-2.md");
        assert_eq!(
            moved,
            "---\nid: x\nassets:\n  - path: a-2-1.jpg\n    sha256: h\n  - path: a-2-2.png\n    \
             sha256: h\n---\n  - path: a-1.jpg\n"
        );
        assert_eq!(images[0], ("a-1.jpg".into(), "inbox/a-2-1.jpg".into()));
        assert_eq!(images[1], ("a-2.png".into(), "inbox/a-2-2.png".into()));
    }

    #[test]
    fn history_names_what_each_commit_published() {
        let msg = message(&[(id(1), "inbox/a.md"), (id(2), "inbox/b-2.md")]);
        assert_eq!(
            msg,
            format!(
                "capture: 2 items\n\nMIS-Writer: capture\nMIS-Capture: {} inbox/a.md\n\
                 MIS-Capture: {} inbox/b-2.md\n",
                id(1),
                id(2)
            )
        );
        assert!(message(&[(id(1), "inbox/a.md")]).starts_with("capture: 1 item\n"));
        // `git log` newest first; a human commit has no trailers.
        let log = format!(
            "commit c3\n\ncommit c2\n{} inbox/a.md\n{} inbox/b-2.md\n\ncommit c1\n",
            id(1),
            id(2)
        );
        let map = published_in(&log);
        let at = |path: &str, commit: &str| Published {
            path: path.into(),
            commit: commit.into(),
        };
        assert_eq!(
            map,
            Map::from([
                (id(1), at("inbox/a.md", "c2")),
                (id(2), at("inbox/b-2.md", "c2"))
            ])
        );
        assert_eq!(published_in("commit c1\nnot-an-id x\n"), Map::new());
    }

    #[test]
    fn only_a_push_someone_beat_is_retried() {
        // As git 2.54 prints them (scratch repos: a stale fetch, a racing push, a hook).
        let line = |why: &str| format!("To /r.git\n!\tHEAD:refs/heads/main\t{why}\nDone\n");
        assert!(raced(&line("[rejected] (fetch first)")));
        assert!(raced(&line(
            "[remote rejected] (incorrect old value provided)"
        )));
        assert!(!raced(&line(
            "[remote rejected] (pre-receive hook declined)"
        )));
        assert!(!raced(""), "unreachable: nothing on stdout");
        assert!(!raced(
            "To /r.git\n \tHEAD:refs/heads/main\t[up to date]\nDone\n"
        ));
    }

    #[test]
    fn state_follows_the_run() {
        use SyncState::*;
        let other = || Err(Failure::Other("x".into()));
        assert_eq!(state(0, &Ok(true)), Idle);
        assert_eq!(state(2, &Ok(false)), Pending);
        assert_eq!(state(2, &other()), Pending);
        assert_eq!(state(0, &other()), Pending, "failed before counting");
        assert_eq!(state(1, &Err(Failure::Offline("x".into()))), Offline);
        assert_eq!(state(1, &Err(Failure::Conflict("x".into()))), Conflict);
    }

    #[test]
    #[cfg(unix)] // process groups
    fn a_hung_git_is_killed_with_what_it_started() {
        let dir = std::env::temp_dir();
        let start = Instant::now();
        let hang = ["-c", "alias.hang=!sleep 30", "hang"];
        let out = run_git(&dir, &hang, Duration::from_millis(300), None).unwrap();
        assert!(!out.ok);
        assert_eq!(out.stderr, "timed out after 0.3s");
        // The readers saw EOF, so `sleep` (git's grandchild) died too.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        // Git exits at once but leaves something holding its output open.
        let start = Instant::now();
        let linger = ["-c", "alias.linger=!sleep 30 & exit 0", "linger"];
        let out = run_git(&dir, &linger, Duration::from_millis(300), None).unwrap();
        assert_eq!(
            (out.ok, out.stderr.as_str()),
            (false, "timed out after 0.3s")
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let ok = run_git(&dir, &["--version"], GIT_DEADLINE, None).unwrap();
        assert!(
            ok.ok && ok.text().starts_with("git version"),
            "{}",
            ok.stderr
        );
    }
}

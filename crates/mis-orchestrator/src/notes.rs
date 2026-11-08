//! Publishes capture receipts (`<state>/captures/inbox/`) as records in a clone of the
//! notes repo (`inbox/`), by capture id: each is written once, then left to the human,
//! who may edit, rename or delete it. Choosing what to write is pure ([`plan`],
//! [`record`]); [`Notes`] is the `git` shell around it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use mis_types::{NotesSync, SyncState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where records go in the repo.
const INBOX: &str = "inbox";
/// The remote branch published to.
const BRANCH: &str = "main";
/// Pushes refused (someone pushed first) before giving up until the next run.
const PUSH_TRIES: usize = 3;
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
    /// Repo path at the time, e.g. `inbox/2026-11-08-foo.md`.
    pub path: String,
    /// Remote commit that held it when this was recorded.
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
/// name unless that, or one of its images' names, is taken: then `-2`, `-3`, …
pub fn plan(receipts: &[Receipt], map: &Map, repo: &BTreeMap<String, Option<Uuid>>) -> Vec<Action> {
    let mut at: BTreeMap<Uuid, &String> = BTreeMap::new();
    for (path, id) in repo {
        if let Some(id) = id {
            at.entry(*id).or_insert(path);
        }
    }
    // Lowercased: macOS file names ignore case.
    let mut taken: BTreeSet<String> = repo.keys().map(|p| p.to_lowercase()).collect();
    let mut actions = Vec::new();
    for r in receipts {
        let id = r.id;
        if map.contains_key(&id) {
            actions.push(Action::Skip { id });
        } else if let Some(path) = at.get(&id) {
            actions.push(Action::Adopt {
                id,
                path: path.to_string(),
            });
        } else {
            let base = stem(&r.file);
            let files = std::iter::once(base.to_string())
                .chain((2..).map(|n| format!("{base}-{n}")))
                .map(|s| paths(r, &s))
                .find(|f| f.iter().all(|p| !taken.contains(&p.to_lowercase())))
                .expect("unbounded");
            taken.extend(files.iter().map(|p| p.to_lowercase()));
            let path = files.into_iter().next().expect("the record");
            actions.push(Action::Write { id, path });
        }
    }
    actions
}

fn stem(file: &str) -> &str {
    file.strip_suffix(".md").unwrap_or(file)
}

/// `asset` of a receipt named `from` renamed for `to`: `a-1.jpg` -> `a-2-1.jpg`.
fn renamed(asset: &str, from: &str, to: &str) -> String {
    match asset.strip_prefix(from) {
        Some(rest) => format!("{to}{rest}"),
        None => asset.to_string(),
    }
}

/// Repo paths of `r` written as `stem`: the record, then its images.
fn paths(r: &Receipt, stem_: &str) -> Vec<String> {
    let from = stem(&r.file);
    std::iter::once(format!("{INBOX}/{stem_}.md"))
        .chain(
            r.assets
                .iter()
                .map(|a| format!("{INBOX}/{}", renamed(a, from, stem_))),
        )
        .collect()
}

/// Receipt `r` (text `md`) written as `path`: the record's text, and each image as
/// (receipt file name, repo path). Renamed images are renamed in the frontmatter too.
pub fn record(md: &str, r: &Receipt, path: &str) -> (String, Vec<(String, String)>) {
    let name = path.strip_prefix(&format!("{INBOX}/")).unwrap_or(path);
    let (from, to) = (stem(&r.file), stem(name));
    let images = (r.assets.iter())
        .map(|a| (a.clone(), format!("{INBOX}/{}", renamed(a, from, to))))
        .collect();
    // Only the frontmatter: the text below it is the human's words.
    let end = md.find("\n---\n").map_or(md.len(), |i| i + 1);
    let mut front = md[..end].to_string();
    for a in &r.assets {
        let line = |name: &str| format!("  - path: {name}\n");
        front = front.replace(&line(a), &line(&renamed(a, from, to)));
    }
    (front + &md[end..], images)
}

/// `git cat-file --batch` output: each object's contents, None where it is missing.
fn batch(mut out: &[u8]) -> Vec<Option<&[u8]>> {
    let mut objects = Vec::new();
    while let Some(nl) = out.iter().position(|b| *b == b'\n') {
        let head = String::from_utf8_lossy(&out[..nl]);
        out = &out[nl + 1..];
        // `<oid> blob <size>`, else `<name> missing`.
        match head
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse::<usize>().ok())
        {
            Some(n) if out.len() > n => {
                objects.push(Some(&out[..n]));
                out = &out[n + 1..];
            }
            // Cut short: what follows is contents, not headers.
            Some(_) => break,
            None => objects.push(None),
        }
    }
    objects
}

/// Whether `git push --porcelain` says the remote refused the ref: someone pushed first
/// (`[rejected] (fetch first)`, `[remote rejected] (incorrect old value provided)`).
/// Otherwise a failed push never reached it.
fn refused(stdout: &str) -> bool {
    stdout.lines().any(|l| l.starts_with("!\t"))
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
    /// Woken by each new capture.
    kick: tokio::sync::Notify,
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
            kick: Default::default(),
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

    pub fn status(&self) -> NotesSync {
        self.status.lock().unwrap().clone()
    }

    /// A new capture was kept: count it and publish soon.
    pub fn captured(&self) {
        let mut s = self.status.lock().unwrap();
        s.pending += 1;
        if s.state == SyncState::Idle {
            s.state = SyncState::Pending;
        }
        drop(s);
        self.kick.notify_one();
    }

    /// Publish at start, `after_capture` after each capture, and every `every`. The only
    /// caller of [`Self::run`], so runs never overlap.
    pub async fn publish_loop(self: std::sync::Arc<Self>) {
        let mut every = tokio::time::interval(self.every);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = every.tick() => {}
                _ = self.kick.notified() => tokio::time::sleep(self.after_capture).await,
            }
            let notes = self.clone();
            let _ = tokio::task::spawn_blocking(move || notes.run()).await;
        }
    }

    /// One publish (blocking: runs `git`), then the status it leaves.
    pub fn run(&self) -> NotesSync {
        let (pending, run) = self.publish();
        let mut s = self.status.lock().unwrap();
        s.state = state(pending, &run);
        s.pending = pending;
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

    /// Captures still unpublished after the run, and whether it pushed.
    fn publish(&self) -> (usize, Result<bool, Failure>) {
        let (receipts, mut map) = match self.read() {
            Ok(read) => read,
            Err(f) => return (self.status().pending, Err(f)),
        };
        let unpublished = |map: &Map| receipts.iter().filter(|r| !map.contains_key(&r.id)).count();
        let pending = unpublished(&map);
        if pending == 0 {
            return (0, Ok(false));
        }
        match self.publish_new(&receipts, &mut map) {
            Ok(pushed) => (unpublished(&map), Ok(pushed)),
            Err(f) => (pending, Err(f)),
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
                receipts.push(Receipt {
                    id: k.id,
                    file,
                    assets: k.assets,
                });
            }
        }
        receipts.sort_by(|a, b| a.file.cmp(&b.file));
        let map = match std::fs::read(&self.map) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
            // A corrupt map is never overwritten: that would publish everything again.
            r => serde_json::from_slice(&r?)
                .map_err(|e| Failure::Other(format!("{}: {e}", self.map.display())))?,
        };
        Ok((receipts, map))
    }

    /// Bring the clone up to date, commit what [`plan`] says to write, push, and only
    /// then record it in `map` and save that. Ok(true): it pushed.
    fn publish_new(&self, receipts: &[Receipt], map: &mut Map) -> Result<bool, Failure> {
        self.sync()?;
        let mut tries = 0;
        let (actions, pushed) = loop {
            let actions = plan(receipts, map, &self.scan()?);
            self.commit(receipts, &actions)?;
            let ahead = self.git(&["rev-list", "--count", &format!("origin/{BRANCH}..HEAD")])?;
            if ahead.check("rev-list", Failure::Other)?.text() == "0" {
                break (actions, false);
            }
            let push = self.git(&[
                "push",
                "--porcelain",
                "origin",
                &format!("HEAD:refs/heads/{BRANCH}"),
            ])?;
            if push.ok {
                break (actions, true);
            }
            tries += 1;
            if !refused(&String::from_utf8_lossy(&push.stdout)) {
                return Err(Failure::Offline(format!("push: {}", push.stderr.trim())));
            }
            if tries == PUSH_TRIES {
                return Err(Failure::Other(format!(
                    "push refused {PUSH_TRIES} times: {}",
                    push.stderr.trim()
                )));
            }
            // Someone pushed first: replay onto theirs and try again.
            self.fetch_rebase()?;
        };
        let head = self.git(&["rev-parse", "HEAD"])?;
        let commit = head.check("rev-parse", Failure::Other)?.text();
        for a in actions {
            if let Action::Write { id, path } | Action::Adopt { id, path } = a {
                let commit = commit.clone();
                map.insert(id, Published { path, commit });
            }
        }
        let json = serde_json::to_vec_pretty(map).expect("serializes");
        crate::push::write_private(&self.map, &json)?;
        if pushed {
            tracing::info!(%commit, "published captures to the notes repo");
        }
        Ok(pushed)
    }

    /// Clone if needed, point at the configured remote, clear what an interrupted run
    /// left uncommitted, then fetch and rebase. A rebase someone else left open stays.
    fn sync(&self) -> Result<(), Failure> {
        if !self.clone.join(".git").exists() {
            let parent = self.clone.parent().expect("in the state dir");
            std::fs::create_dir_all(parent)?;
            let name = self.clone.file_name().expect("notes").to_string_lossy();
            let out = git(parent, &["clone", "--quiet", "--", &self.remote, &name])?;
            out.check("clone", Failure::Offline)?;
        }
        let git_dir = self.clone.join(".git");
        if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
            return Err(self.conflict());
        }
        let set = self.git(&["remote", "set-url", "origin", &self.remote])?;
        set.check("remote set-url", Failure::Other)?;
        // Only this publisher writes here, and runs never overlap: anything uncommitted is
        // a record file from a run cut short, rewritten below if still wanted.
        let reset = self.git(&["reset", "--hard", "--quiet", "HEAD"])?;
        reset.check("reset", Failure::Other)?;
        let clean = self.git(&["clean", "-fdq", "--", INBOX])?;
        clean.check("clean", Failure::Other)?;
        self.fetch_rebase()
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
        // Back to the unpublished commit; it stays pending until a human resolves it.
        let abort = self.git(&["rebase", "--abort"])?;
        abort.check("rebase --abort", Failure::Other)?;
        Err(self.conflict())
    }

    fn conflict(&self) -> Failure {
        Failure::Conflict(format!(
            "notes repo conflict, resolve in {}",
            self.clone.display()
        ))
    }

    /// The committed `inbox/` files (HEAD, not the working tree), with their record ids.
    fn scan(&self) -> Result<BTreeMap<String, Option<Uuid>>, Failure> {
        let ls = self.git(&["ls-tree", "-z", "--name-only", "HEAD", &format!("{INBOX}/")])?;
        let ls = ls.check("ls-tree", Failure::Other)?;
        let paths: Vec<String> = (ls.stdout.split(|b| *b == 0))
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        let md: Vec<&String> = paths.iter().filter(|p| p.ends_with(".md")).collect();
        let input: String = md.iter().map(|p| format!("HEAD:{p}\n")).collect();
        let cat = self.git_with(&["cat-file", "--batch"], input.as_bytes())?;
        let cat = cat.check("cat-file", Failure::Other)?;
        let ids: BTreeMap<&String, Uuid> = (md.iter().copied())
            .zip(batch(&cat.stdout))
            .filter_map(|(p, text)| {
                let k = crate::capture::parse(&String::from_utf8_lossy(text?))?;
                Some((p, k.id))
            })
            .collect();
        Ok(paths
            .iter()
            .map(|p| (p.clone(), ids.get(p).copied()))
            .collect())
    }

    /// Write and commit the records `actions` says to write, if any.
    fn commit(&self, receipts: &[Receipt], actions: &[Action]) -> Result<(), Failure> {
        let mut added = Vec::new();
        for a in actions {
            let Action::Write { id, path } = a else {
                continue;
            };
            std::fs::create_dir_all(self.clone.join(INBOX))?;
            let r = receipts
                .iter()
                .find(|r| r.id == *id)
                .expect("planned from these");
            let md = std::fs::read_to_string(self.receipts.join(&r.file))?;
            let (md, images) = record(&md, r, path);
            for (from, to) in images {
                std::fs::copy(self.receipts.join(from), self.clone.join(&to))?;
                added.push(to);
            }
            std::fs::write(self.clone.join(path), md)?;
            added.push(path.clone());
        }
        if added.is_empty() {
            return Ok(());
        }
        let n = actions
            .iter()
            .filter(|a| matches!(a, Action::Write { .. }))
            .count();
        let subject = format!("capture: {n} item{}", if n == 1 { "" } else { "s" });
        let mut add = vec!["add", "--"];
        add.extend(added.iter().map(String::as_str));
        self.git(&add)?.check("add", Failure::Other)?;
        let commit = [
            "commit",
            "--quiet",
            "-m",
            &subject,
            "-m",
            "MIS-Writer: capture",
        ];
        self.git(&commit)?.check("commit", Failure::Other)?;
        Ok(())
    }

    fn git(&self, args: &[&str]) -> Result<Out, Failure> {
        Ok(git(&self.clone, args)?)
    }

    fn git_with(&self, args: &[&str], input: &[u8]) -> Result<Out, Failure> {
        Ok(run_git(&self.clone, args, Some(input))?)
    }
}

fn git(dir: &Path, args: &[&str]) -> std::io::Result<Out> {
    run_git(dir, args, None)
}

/// `git args` in `dir`, never waiting on a person: no prompts, editors or signing.
/// Commits are Make It So's; the user's own git config (credential helper) still applies.
fn run_git(dir: &Path, args: &[&str], input: Option<&[u8]>) -> std::io::Result<Out> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        // A stalled transfer (under 1 KB/s for a minute) fails instead of hanging.
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "http.lowSpeedLimit=1000",
        ])
        .args(["-c", "http.lowSpeedTime=60"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Make It So")
        .env("GIT_AUTHOR_EMAIL", "mis@localhost")
        .env("GIT_COMMITTER_NAME", "Make It So")
        .env("GIT_COMMITTER_EMAIL", "mis@localhost")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    // Fed from a thread: cat-file answers as it reads, and would block on a full stdout
    // pipe while we block on a full stdin one.
    let feed = (input.map(<[u8]>::to_vec))
        .zip(child.stdin.take())
        .map(|(input, mut stdin)| std::thread::spawn(move || stdin.write_all(&input)));
    let out = child.wait_with_output()?;
    if let Some(feed) = feed {
        feed.join().expect("stdin writer")?;
    }
    Ok(Out {
        ok: out.status.success(),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
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
    fn reads_cat_file_batch_output() {
        let out = b"45b9 blob 3\nhi\n\nHEAD:nope.md missing\n0123 blob 0\n\n";
        assert_eq!(batch(out), [Some(&b"hi\n"[..]), None, Some(&b""[..])]);
        assert_eq!(batch(b"0123 blob 9\nshort\n"), [], "truncated");
    }

    #[test]
    fn a_refused_push_is_told_from_an_unreachable_remote() {
        // As git 2.54 prints them (scratch repos, a stale fetch and a racing push).
        let fetch_first = "To /r.git\n!\tHEAD:refs/heads/main\t[rejected] (fetch first)\nDone\n";
        let raced = "To /r.git\n!\tHEAD:refs/heads/main\t[remote rejected] (incorrect old value provided)\nDone\n";
        assert!(refused(fetch_first) && refused(raced));
        assert!(!refused(""), "unreachable: nothing on stdout");
        assert!(!refused(
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
}

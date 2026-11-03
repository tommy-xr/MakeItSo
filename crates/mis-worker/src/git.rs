//! What a task changed in its git repo. `base` notes HEAD and the paths already
//! dirty when the task starts; `diff` reads git's view against that when it
//! ends. Parsing git's output and assembling the diff are pure; `git` runs
//! read-only commands behind a timeout, taking no optional locks.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use mis_types::diff::*;
use tokio::io::AsyncReadExt;

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Most of one command's output read; past it the rest is dropped.
const MAX_OUTPUT: usize = 16 << 20;
/// Untracked files are read for their lines up to this size each, and this much in all.
const MAX_UNTRACKED: u64 = 4 << 20;
const MAX_UNTRACKED_TOTAL: u64 = 64 << 20;
/// Files fingerprinted by content up to this size; larger ones by size and mtime.
const MAX_HASHED: u64 = 16 << 20;
/// git's binary test: a NUL in the first this many bytes.
const BINARY_SNIFF: usize = 8000;

/// `git status --porcelain=v2 --branch -z`, read.
#[derive(Debug, Default, PartialEq)]
struct Status {
    /// None before the first commit.
    head: Option<String>,
    /// None when detached.
    branch: Option<String>,
    upstream: Option<String>,
    ahead: u32,
    /// Changed tracked paths, renamed-from paths included.
    changed: Vec<String>,
    untracked: Vec<String>,
}

/// NUL-separated records, each as text.
fn records(out: &[u8]) -> impl Iterator<Item = String> + '_ {
    let out = out.strip_suffix(b"\0").unwrap_or(out);
    (out.split(|b| *b == 0)).map(|r| String::from_utf8_lossy(r).into_owned())
}

fn parse_status(out: &[u8]) -> Status {
    let mut s = Status::default();
    let mut recs = records(out);
    while let Some(rec) = recs.next() {
        if let Some(h) = rec.strip_prefix("# ") {
            let (k, v) = h.split_once(' ').unwrap_or((h, ""));
            match k {
                "branch.oid" if v != "(initial)" => s.head = Some(v.into()),
                "branch.head" if v != "(detached)" => s.branch = Some(v.into()),
                "branch.upstream" => s.upstream = Some(v.into()),
                "branch.ab" => {
                    let a = v.split(' ').next().and_then(|a| a.strip_prefix('+'));
                    s.ahead = a.and_then(|a| a.parse().ok()).unwrap_or(0);
                }
                _ => {}
            }
            continue;
        }
        // The path follows a fixed number of fields: 8 for `1`, 9 for `2` (then
        // its source as the next record), 10 for `u` (unmerged).
        let (kind, rest) = rec.split_once(' ').unwrap_or((&rec, ""));
        let path = |n| rest.splitn(n + 1, ' ').nth(n).map(String::from);
        match kind {
            "1" => s.changed.extend(path(7)),
            "2" => {
                s.changed.extend(path(8));
                s.changed.extend(recs.next());
            }
            "u" => s.changed.extend(path(9)),
            "?" => s.untracked.push(rest.into()),
            _ => {}
        }
    }
    s
}

/// One tracked file's change: `git diff --raw --numstat -z`.
#[derive(Debug, PartialEq)]
struct Change {
    status: FileStatus,
    path: String,
    from: Option<String>,
    added: u32,
    removed: u32,
    binary: bool,
}

/// `--raw` records (`:modes shas STATUS`, then the path, or source and
/// destination for a rename or copy) followed by as many `--numstat` records
/// (`added\tremoved\tpath`, or `…\t` then source and destination), same order.
fn parse_changes(out: &[u8]) -> Vec<Change> {
    let mut recs = records(out).peekable();
    let mut changes = vec![];
    while let Some(meta) = recs.next_if(|r| r.starts_with(':')) {
        let letter = meta.rsplit(' ').next().and_then(|s| s.chars().next());
        let Some(first) = recs.next() else { break };
        let (status, from, path) = match letter {
            Some('R') => (FileStatus::Renamed, Some(first), recs.next()),
            Some('C') => (FileStatus::Added, Some(first), recs.next()),
            Some('A') => (FileStatus::Added, None, Some(first)),
            Some('D') => (FileStatus::Deleted, None, Some(first)),
            _ => (FileStatus::Modified, None, Some(first)),
        };
        let Some(path) = path else { break };
        changes.push(Change {
            status,
            path,
            from: from.filter(|_| status == FileStatus::Renamed),
            added: 0,
            removed: 0,
            binary: false,
        });
    }
    for c in &mut changes {
        let Some(rec) = recs.next() else { break };
        let mut f = rec.splitn(3, '\t');
        let (a, d, p) = (f.next(), f.next(), f.next().unwrap_or(""));
        if p.is_empty() {
            recs.next();
            recs.next();
        }
        c.binary = a == Some("-");
        c.added = a.and_then(|n| n.parse().ok()).unwrap_or(0);
        c.removed = d.and_then(|n| n.parse().ok()).unwrap_or(0);
    }
    changes
}

/// `git diff` output cut into one patch per file. Each starts at a `diff --git `
/// line, which no content line can be: those start with ` `, `+`, `-` or `\`.
fn split_patches(out: &[u8]) -> Vec<String> {
    let mut starts = vec![];
    let mut at = 0;
    for line in out.split_inclusive(|b| *b == b'\n') {
        if line.starts_with(b"diff --git ") {
            starts.push(at);
        }
        at += line.len();
    }
    starts.push(out.len());
    (starts.windows(2))
        .map(|w| String::from_utf8_lossy(&out[w[0]..w[1]]).into_owned())
        .collect()
}

/// An untracked file's diff, every line added; `bytes` None when it wasn't read (`why`).
fn untracked(path: String, bytes: Result<Vec<u8>, String>) -> FileDiff {
    let mut f = FileDiff {
        path,
        from: None,
        status: FileStatus::Added,
        added: 0,
        removed: 0,
        binary: false,
        untracked: true,
        before: Before::Clean,
        patch: None,
        note: None,
    };
    let bytes = match bytes {
        Ok(b) => b,
        Err(why) => {
            f.note = Some(why);
            return f;
        }
    };
    if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
        f.binary = true;
        f.note = Some("binary".into());
        return f;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    f.added = lines.len() as u32;
    if lines.is_empty() {
        f.note = Some("empty".into());
    } else if bytes.len() > MAX_FILE_PATCH {
        f.note = Some(format!("patch over {} KB", MAX_FILE_PATCH >> 10));
    } else {
        let mut p = format!("@@ -0,0 +1,{} @@\n", lines.len());
        for l in &lines {
            p.push('+');
            p.push_str(l);
        }
        if !text.ends_with('\n') {
            p.push_str("\n\\ No newline at end of file\n");
        }
        f.patch = Some(p);
    }
    f
}

/// FNV-1a: tells whether a file changed between the task's start and end.
fn fingerprint(bytes: &[u8]) -> String {
    let h = (bytes.iter()).fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    });
    format!("{h:016x}")
}

/// Whether `path` had changes when the task started, and if so whether it has
/// changed since (`now` holds its fingerprint at the end).
fn before(base: &DiffBase, now: &BTreeMap<String, String>, path: &str) -> Before {
    match base.dirty.get(path) {
        None => Before::Clean,
        Some(then) if now.get(path) == Some(then) => Before::Only,
        Some(_) => Before::Also,
    }
}

/// The task's diff: tracked `changes` with their `patches` (one per change in
/// order; `cut` when git's output ran past what was read, so the last are
/// missing), then the untracked files, by path and bounded.
fn assemble(
    base: &DiffBase,
    changes: Vec<Change>,
    patches: Vec<String>,
    cut: bool,
    untracked: Vec<FileDiff>,
    now: &BTreeMap<String, String>,
    repo: RepoState,
) -> TaskDiff {
    // Matched by header, not order: git ran twice and the tree may have changed
    // between. A file swapped for a symlink comes as two patches under one header.
    let mut by_header: BTreeMap<String, String> = BTreeMap::new();
    for p in patches {
        let head = p.lines().next().unwrap_or("").to_string();
        by_header.entry(head).or_default().push_str(&p);
    }
    let mut files: Vec<FileDiff> = (changes.into_iter())
        .map(|c| {
            // A file renamed after its earlier changes still carries them.
            let moved = c.from.as_ref().is_some_and(|f| base.dirty.contains_key(f));
            let before = match before(base, now, &c.path) {
                Before::Clean if moved => Before::Also,
                b => b,
            };
            // Paths git quotes (`"`, `\`, control characters) never match: no patch.
            let from = c.from.as_deref().unwrap_or(&c.path);
            let patch = by_header.remove(&format!("diff --git a/{from} b/{}", c.path));
            let note = match (&patch, c.binary) {
                _ if before == Before::Only => Some(ONLY),
                (_, true) => Some("binary"),
                (Some(_), false) => None,
                (None, false) if cut => Some("patch too large to read"),
                (None, false) => Some("patch unavailable"),
            };
            FileDiff {
                before,
                patch: patch.filter(|_| !c.binary && before != Before::Only),
                note: note.map(String::from),
                path: c.path,
                from: c.from,
                status: c.status,
                added: c.added,
                removed: c.removed,
                binary: c.binary,
                untracked: false,
            }
        })
        .collect();
    files.extend(untracked.into_iter().map(|mut f| {
        f.before = before(base, now, &f.path);
        if f.before == Before::Only {
            (f.patch, f.note) = (None, Some(ONLY.into()));
        }
        f
    }));
    // The task's own first, so the file limit cuts earlier changes first.
    let only = |f: &FileDiff| f.before == Before::Only;
    files.sort_by(|a, b| only(a).cmp(&only(b)).then_with(|| a.path.cmp(&b.path)));
    TaskDiff {
        base: base.clone(),
        repo,
        files,
        more: 0,
    }
    .bounded()
}

/// The note on a file only changed before the task: its contents are the user's, never read.
const ONLY: &str = "changed before this task";

/// Whether a `git remote -v` listing has a remote on github.com.
fn github(remotes: &str) -> bool {
    let host = |url: &str| {
        // `https://u:t@github.com/o/r`, `ssh://git@github.com:22/o/r`, `git@github.com:o/r`.
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let auth = rest.split('/').next().unwrap_or("");
        let host = auth.rsplit('@').next().unwrap_or("");
        host.split(':').next().unwrap_or("").to_ascii_lowercase()
    };
    (remotes.lines())
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|u| matches!(host(u).as_str(), "github.com" | "ssh.github.com"))
}

/// A commit id as git prints it. Anything else (an option, say) never reaches git.
fn is_commit(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `git -C dir <args>`: its stdout, and whether it was cut at `MAX_OUTPUT`.
async fn git(dir: &str, args: &[&str]) -> Result<(Vec<u8>, bool), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(dir);
    // Paths verbatim, no fsmonitor hook, patches as `a/` `b/` from the top.
    for c in [
        "core.quotePath=false",
        "core.fsmonitor=false",
        "diff.noprefix=false",
        "diff.mnemonicPrefix=false",
        "diff.relative=false",
    ] {
        cmd.args(["-c", c]);
    }
    cmd.args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    for v in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_EXTERNAL_DIFF",
    ] {
        cmd.env_remove(v);
    }
    let mut child = (cmd.stdin(std::process::Stdio::null()))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    let run = async {
        let stdout = child.stdout.take().expect("piped");
        let mut out = vec![];
        let mut stdout = stdout.take(MAX_OUTPUT as u64 + 1);
        (stdout.read_to_end(&mut out).await).map_err(|e| format!("git {}: {e}", args[0]))?;
        if out.len() > MAX_OUTPUT {
            out.truncate(MAX_OUTPUT);
            return Ok((out, true));
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        match status.success() {
            true => Ok((out, false)),
            false => Err(format!("git {} failed", args[0])),
        }
    };
    let out = tokio::time::timeout(GIT_TIMEOUT, run).await;
    out.map_err(|_| format!("git {} timed out", args[0]))?
}

/// `git status` of `root`, untracked files listed one by one unless the repo
/// hides them (`status.showUntrackedFiles=no`, as dotfiles repos set).
async fn status(root: &str) -> Option<Status> {
    let shown = git(root, &["config", "--get", "status.showUntrackedFiles"]).await;
    let hidden = shown.is_ok_and(|o| String::from_utf8_lossy(&o.0).trim() == "no");
    let untracked = if hidden { "-uno" } else { "-uall" };
    let args = ["status", "--porcelain=v2", "--branch", "-z", untracked];
    Some(parse_status(whole(&git(root, &args).await.ok()?)))
}

/// Output cut mid-record loses its last record.
fn whole((out, cut): &(Vec<u8>, bool)) -> &[u8] {
    match cut {
        true => &out[..out.iter().rposition(|b| *b == 0).map_or(0, |i| i + 1)],
        false => out,
    }
}

/// `rel` (from git) under `root`, opened only if a regular file inside it.
fn open_in(root: &Path, rel: &str) -> Result<std::fs::File, String> {
    if !Path::new(rel)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err("outside the repo".into());
    }
    let path = root.join(rel);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("symlink".into());
    }
    let real = path.canonicalize().map_err(|e| e.to_string())?;
    if !real.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
        return Err("outside the repo".into());
    }
    // Opened without following a symlink swapped in since, or waiting on a FIFO.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut opts, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = opts.open(&real).map_err(|e| e.to_string())?;
    match file.metadata().map_err(|e| e.to_string())?.is_file() {
        true => Ok(file),
        false => Err("not a regular file".into()),
    }
}

/// At most `max` bytes of `file`; Err past that.
fn read_max(file: std::fs::File, max: u64) -> Result<Vec<u8>, String> {
    let mut bytes = vec![];
    (file.take(max + 1).read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    match bytes.len() as u64 > max {
        true => Err(format!("over {} MB, not read", max >> 20)),
        false => Ok(bytes),
    }
}

/// Each of `paths`' fingerprint now: its content's, `big:<len>:<mtime>` past
/// `MAX_HASHED`, or `gone`.
fn fingerprints(root: &Path, paths: impl IntoIterator<Item = String>) -> BTreeMap<String, String> {
    let one = |rel: &str| -> String {
        if !Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            return "gone".into();
        }
        let Ok(file) = open_in(root, rel) else {
            // A symlink by its target; anything else unreadable alike each time.
            return match std::fs::read_link(root.join(rel)) {
                Ok(t) => fingerprint(t.to_string_lossy().as_bytes()),
                Err(_) => "gone".into(),
            };
        };
        let meta = file.metadata().ok();
        match meta.filter(|m| m.len() > MAX_HASHED) {
            Some(m) => {
                let at = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                format!("big:{}:{}", m.len(), at.map_or(0, |d| d.as_nanos()))
            }
            None => read_max(file, MAX_HASHED).map_or("gone".into(), |b| fingerprint(&b)),
        }
    };
    paths
        .into_iter()
        .map(|p| {
            let f = one(&p);
            (p, f)
        })
        .collect()
}

/// Where a task starting in `cwd` starts in its repo: `inherited` (its thread's
/// base) when it is this repo's and its commit is still there. None outside a
/// repo, in one reaching above the worker's `roots` (canonical), or before its
/// first commit.
pub async fn base(cwd: &str, roots: &[PathBuf], inherited: Option<DiffBase>) -> Option<DiffBase> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"]).await.ok()?;
    let root = String::from_utf8_lossy(&top.0).trim_end().to_string();
    let real = Path::new(&root).canonicalize().ok()?;
    if !roots.iter().any(|r| real.starts_with(r)) {
        return None;
    }
    if let Some(b) = inherited.filter(|b| b.root == root && is_commit(&b.head)) {
        let commit = format!("{}^{{commit}}", b.head);
        if git(&root, &["cat-file", "-e", &commit]).await.is_ok() {
            return Some(b);
        }
    }
    let status = status(&root).await?;
    let head = status.head.filter(|h| is_commit(h))?;
    let paths = (status.changed.into_iter().chain(status.untracked)).take(MAX_DIRTY);
    let paths: Vec<String> = paths.collect();
    let dir = root.clone();
    let dirty = tokio::task::spawn_blocking(move || fingerprints(Path::new(&dir), paths))
        .await
        .ok()?;
    Some(DiffBase { root, head, dirty })
}

/// What changed in `base`'s repo since it: commits, the index and the working
/// tree, untracked files included. None if git can't say.
pub async fn diff(base: &DiffBase) -> Option<TaskDiff> {
    let root = base.root.as_str();
    if !is_commit(&base.head) {
        return None;
    }
    let status = status(root).await?;
    let opts = ["-M", "--no-ext-diff", "--no-textconv", "--no-color"];
    let mut args = vec!["diff", "--raw", "--numstat", "-z"];
    args.extend(opts);
    args.extend([base.head.as_str(), "--"]);
    let changes = parse_changes(whole(&git(root, &args).await.ok()?));
    let mut args = vec!["diff"];
    args.extend(opts);
    args.extend([base.head.as_str(), "--"]);
    let (patch, cut) = git(root, &args).await.ok()?;
    let mut patches = split_patches(&patch);
    if cut {
        patches.pop();
    }
    let commits = match status.head.as_deref() == Some(&base.head) {
        true => 0,
        false => {
            let range = format!("{}..HEAD", base.head);
            let out = git(root, &["rev-list", "--count", &range]).await.ok();
            out.and_then(|o| String::from_utf8_lossy(&o.0).trim().parse().ok())
                .unwrap_or(0)
        }
    };
    let origin_head = git(
        root,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    );
    let default_branch = (origin_head.await.ok())
        .and_then(|o| {
            let name = String::from_utf8_lossy(&o.0).trim().to_string();
            name.strip_prefix("origin/").map(String::from)
        })
        .filter(|n| !n.is_empty());
    let remotes = git(root, &["remote", "-v"]).await.ok();
    let (b, new) = (base.clone(), status.untracked);
    let (untracked, now) = tokio::task::spawn_blocking(move || {
        let root = Path::new(&b.root);
        let now = fingerprints(root, b.dirty.keys().cloned());
        let mut budget = MAX_UNTRACKED_TOTAL;
        let files = (new.into_iter().enumerate())
            .map(|(i, p)| {
                let read = match i < MAX_FILES && budget > 0 {
                    _ if before(&b, &now, &p) == Before::Only => Err(ONLY.into()),
                    true => open_in(root, &p).and_then(|f| read_max(f, MAX_UNTRACKED.min(budget))),
                    false => Err("not read".into()),
                };
                budget -= read.as_ref().map_or(0, |b| b.len() as u64);
                untracked(p, read)
            })
            .collect::<Vec<_>>();
        (files, now)
    })
    .await
    .ok()?;
    let changed = status
        .changed
        .iter()
        .chain(untracked.iter().map(|f| &f.path));
    let repo = RepoState {
        branch: status.branch,
        upstream: status.upstream,
        ahead: status.ahead,
        default_branch,
        commits,
        github: remotes.is_some_and(|o| github(&String::from_utf8_lossy(&o.0))),
        uncommitted: changed
            .into_iter()
            .any(|p| before(base, &now, p) != Before::Only),
    };
    Some(assemble(base, changes, patches, cut, untracked, &now, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_branch_changes_renames_and_untracked() {
        let out = "# branch.oid 593042f0981b2123582009552e93037fe0eed362\0# branch.head feat/x\0\
            # branch.upstream origin/feat/x\0# branch.ab +3 -1\0\
            1 .M N... 100644 100644 100644 88768ef 88768ef sp ace.txt\0\
            2 R. N... 100644 100644 100644 587be6b 587be6b R100 new é.txt\0old.txt\0\
            u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
            ? untr \"q\".txt\0? d/z.txt\0! ignored\0";
        let s = parse_status(out.as_bytes());
        assert_eq!(
            s,
            Status {
                head: Some("593042f0981b2123582009552e93037fe0eed362".into()),
                branch: Some("feat/x".into()),
                upstream: Some("origin/feat/x".into()),
                ahead: 3,
                changed: vec![
                    "sp ace.txt".into(),
                    "new é.txt".into(),
                    "old.txt".into(),
                    "conflict.rs".into()
                ],
                untracked: vec!["untr \"q\".txt".into(), "d/z.txt".into()],
            }
        );
        let fresh = parse_status(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!((fresh.head, fresh.branch), (None, None));
    }

    #[test]
    fn changes_pair_raw_records_with_their_counts() {
        let out = ":100644 100644 88768ef 0000000 M\0b.bin\0\
            :100644 000000 4e349b5 0000000 D\0crlf.txt\0\
            :100644 100644 587be6b 587be6b R087\0old.txt\0new é.txt\0\
            :000000 100644 0000000 1234567 A\0sp ace.txt\0\
            -\t-\tb.bin\0\
            0\t2\tcrlf.txt\0\
            1\t1\t\0old.txt\0new é.txt\0\
            4\t0\tsp ace.txt\0";
        let c = parse_changes(out.as_bytes());
        let got: Vec<_> = c
            .iter()
            .map(|c| {
                (
                    c.status,
                    c.path.as_str(),
                    c.from.as_deref(),
                    c.added,
                    c.removed,
                    c.binary,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (FileStatus::Modified, "b.bin", None, 0, 0, true),
                (FileStatus::Deleted, "crlf.txt", None, 0, 2, false),
                (
                    FileStatus::Renamed,
                    "new é.txt",
                    Some("old.txt"),
                    1,
                    1,
                    false
                ),
                (FileStatus::Added, "sp ace.txt", None, 4, 0, false),
            ]
        );
        assert!(parse_changes(b"").is_empty());
    }

    #[test]
    fn patches_split_at_each_file_header_only() {
        let out = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-diff --git x\n+one\r\n\
            diff --git a/sp ace b/sp ace\nBinary files a/sp ace and b/sp ace differ\n";
        let p = split_patches(out.as_bytes());
        assert_eq!(p.len(), 2);
        assert!(p[0].ends_with("-diff --git x\n+one\r\n"), "{:?}", p[0]);
        assert!(p[1].starts_with("diff --git a/sp ace"));
        assert!(split_patches(b"").is_empty());
    }

    #[test]
    fn untracked_files_count_as_added_lines() {
        let f = untracked("n.txt".into(), Ok(b"one\r\ntwo\nthree".to_vec()));
        assert_eq!(
            (f.added, f.untracked, f.status),
            (3, true, FileStatus::Added)
        );
        assert_eq!(
            f.patch.as_deref(),
            Some("@@ -0,0 +1,3 @@\n+one\r\n+two\n+three\n\\ No newline at end of file\n")
        );
        let bin = untracked("x.png".into(), Ok(b"\x89PNG\r\n\x1a\n\0\0".to_vec()));
        assert_eq!(
            (bin.binary, bin.patch, bin.note.as_deref()),
            (true, None, Some("binary"))
        );
        let big = untracked("big.txt".into(), Ok(b"x\n".repeat(MAX_FILE_PATCH)));
        assert_eq!(
            (big.added, big.patch.is_none()),
            (MAX_FILE_PATCH as u32, true)
        );
        let unread = untracked("l".into(), Err("symlink".into()));
        assert_eq!(unread.note.as_deref(), Some("symlink"));
        assert_eq!(
            untracked("e".into(), Ok(vec![])).note.as_deref(),
            Some("empty")
        );
    }

    #[test]
    fn github_remotes_by_host() {
        for url in [
            "git@github.com:o/r.git",
            "https://github.com/o/r",
            "https://u:tok@github.com/o/r.git",
            "ssh://git@ssh.github.com:443/o/r",
            "HTTPS://GitHub.com/o/r",
        ] {
            assert!(github(&format!("origin\t{url} (fetch)\n")), "{url}");
        }
        for url in [
            "/srv/git/r.git",
            "file:///srv/r",
            "https://gitlab.com/github.com/r",
            "git@github.com.evil.io:o/r",
        ] {
            assert!(!github(&format!("origin\t{url} (push)\n")), "{url}");
        }
    }

    #[test]
    fn only_commit_ids_reach_git() {
        assert!(is_commit("593042f0981b2123582009552e93037fe0eed362"));
        for bad in ["--output=/tmp/x", "HEAD", "abc", "593042f; rm", ""] {
            assert!(!is_commit(bad), "{bad}");
        }
    }

    fn change(path: &str, binary: bool) -> Change {
        Change {
            status: FileStatus::Modified,
            path: path.into(),
            from: None,
            added: 1,
            removed: 1,
            binary,
        }
    }

    #[test]
    fn assembled_files_put_the_tasks_first_and_tell_earlier_changes_apart() {
        let base = DiffBase {
            root: "/r".into(),
            head: "abcd".into(),
            dirty: [("a", "1"), ("b", "2"), ("z", "3"), ("old", "4")]
                .map(|(p, f)| (p.to_string(), f.to_string()))
                .into(),
        };
        // `a` untouched since, `b` changed further, `z` untracked and untouched,
        // `old` renamed to `new` after its earlier change.
        let now: BTreeMap<_, _> = [("a", "1"), ("b", "9"), ("z", "3")]
            .map(|(p, f)| (p.to_string(), f.to_string()))
            .into();
        let p = |path: &str| format!("diff --git a/{path} b/{path}\n@@ x");
        let mut renamed = change("new", false);
        renamed.from = Some("old".into());
        let d = assemble(
            &base,
            vec![
                change("b", false),
                change("a", false),
                change("c", true),
                renamed,
            ],
            vec![
                p("b"),
                p("a"),
                p("c"),
                "diff --git a/old b/new\nrename from old\n".into(),
            ],
            false,
            vec![untracked("z".into(), Ok(b"z\n".to_vec()))],
            &now,
            RepoState::default(),
        );
        let got: Vec<_> = (d.files.iter())
            .map(|f| {
                (
                    f.path.as_str(),
                    f.before,
                    f.patch.is_some(),
                    f.note.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("b", Before::Also, true, None),
                ("c", Before::Clean, false, Some("binary")),
                ("new", Before::Also, true, None),
                ("a", Before::Only, false, Some(ONLY)),
                ("z", Before::Only, false, Some(ONLY)),
            ]
        );
        assert_eq!(d.stat().files, 3);
        // A patch is matched by its header: one for another file (the tree
        // changed between git's runs) or quoted by git matches nothing; one cut
        // off the end leaves its file without.
        let notes = |changes: Vec<Change>, patches: &[&str], cut| {
            let patches = patches.iter().map(|p| p.to_string()).collect();
            let d = assemble(
                &base,
                changes,
                patches,
                cut,
                vec![],
                &now,
                RepoState::default(),
            );
            let notes = d.files.iter().map(|f| (f.path.clone(), f.note.clone()));
            notes.collect::<Vec<_>>()
        };
        let unavailable = Some("patch unavailable".to_string());
        assert_eq!(
            notes(vec![change("c", false)], &[&p("d")], false),
            [("c".into(), unavailable.clone())]
        );
        let quoted = "diff --git \"a/t\\\"q\" \"b/t\\\"q\"\n";
        assert_eq!(
            notes(vec![change("t\"q", false)], &[quoted], false),
            [("t\"q".into(), unavailable)]
        );
        assert_eq!(
            notes(
                vec![change("c", false), change("d", false)],
                &[&p("c")],
                true
            ),
            [
                ("c".into(), None),
                ("d".into(), Some("patch too large to read".into()))
            ]
        );
        // A file swapped for a symlink: two patches under one header, both kept.
        let swap = format!("{}deleted file mode 100644\n", p("c"));
        let d = assemble(
            &base,
            vec![change("c", false)],
            vec![swap.clone(), p("c")],
            false,
            vec![],
            &now,
            RepoState::default(),
        );
        assert_eq!(d.files[0].patch, Some(format!("{swap}{}", p("c"))));
    }

    /// `git` in `dir`, for setting up repos; panics on failure.
    fn sh_git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A real repo: a task starts with one file already dirty, then edits, adds,
    /// deletes, renames and commits; its diff tells its own changes from the earlier one.
    #[tokio::test]
    async fn diff_of_a_real_repo_against_its_base() {
        let dir = std::env::temp_dir().join(format!("mis-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = |p: &str, b: &[u8]| std::fs::write(dir.join(p), b).unwrap();
        sh_git(&dir, &["init", "-q", "-b", "main"]);
        sh_git(&dir, &["config", "core.autocrlf", "false"]);
        w("keep.txt", b"a\nb\nc\n");
        w("gone.txt", b"bye\n");
        w("old name.txt", b"same\nsame\nsame\n");
        w("crlf.txt", b"one\r\ntwo\r\n");
        w("dirty.txt", b"x\n");
        sh_git(&dir, &["add", "."]);
        sh_git(&dir, &["commit", "-qm", "init"]);
        sh_git(
            &dir,
            &["remote", "add", "origin", "git@github.com:demo/r.git"],
        );
        // The user's own edit and file, before the task.
        w("dirty.txt", b"x\nmine\n");
        w("secret.env", b"TOKEN=1\n");
        let cwd = dir.to_string_lossy().into_owned();
        let roots = [dir.canonicalize().unwrap()];
        let base = base(&cwd, &roots, None).await.expect("a base");
        assert!(is_commit(&base.head));
        assert_eq!(
            base.dirty.keys().collect::<Vec<_>>(),
            ["dirty.txt", "secret.env"]
        );

        w("keep.txt", b"a\nB\nc\nd\n");
        w("crlf.txt", b"one\r\nTWO\r\n");
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        sh_git(&dir, &["mv", "old name.txt", "nouveau é.txt"]);
        sh_git(&dir, &["commit", "-qm", "rename"]);
        w("new file.rs", b"fn main() {}\n");
        w("pic.bin", b"\0\x01\x02");
        let d = diff(&base).await.expect("a diff");
        let got: Vec<_> = (d.files.iter())
            .map(|f| {
                (
                    f.path.as_str(),
                    f.status,
                    f.added,
                    f.removed,
                    f.untracked,
                    f.before,
                )
            })
            .collect();
        use FileStatus::*;
        assert_eq!(
            got,
            [
                ("crlf.txt", Modified, 1, 1, false, Before::Clean),
                ("gone.txt", Deleted, 0, 1, false, Before::Clean),
                ("keep.txt", Modified, 2, 1, false, Before::Clean),
                ("new file.rs", Added, 1, 0, true, Before::Clean),
                ("nouveau é.txt", Renamed, 0, 0, false, Before::Clean),
                ("pic.bin", Added, 0, 0, true, Before::Clean),
                ("dirty.txt", Modified, 1, 0, false, Before::Only),
                ("secret.env", Added, 0, 0, true, Before::Only),
            ]
        );
        // Earlier changes are the user's: never read, nor sent.
        for p in ["dirty.txt", "secret.env"] {
            let f = d.files.iter().find(|f| f.path == p).unwrap();
            assert_eq!((f.patch.as_deref(), f.note.as_deref()), (None, Some(ONLY)));
        }
        let file = |p: &str| d.files.iter().find(|f| f.path == p).unwrap();
        assert_eq!(file("nouveau é.txt").from.as_deref(), Some("old name.txt"));
        assert!(file("crlf.txt")
            .patch
            .as_ref()
            .unwrap()
            .contains("+TWO\r\n"));
        assert!(file("keep.txt")
            .patch
            .as_ref()
            .unwrap()
            .contains("@@ -1,3 +1,4 @@"));
        assert!(file("pic.bin").binary);
        let r = &d.repo;
        assert_eq!(
            (
                r.branch.as_deref(),
                r.commits,
                r.github,
                r.ahead,
                r.uncommitted
            ),
            (Some("main"), 1, true, 0, true)
        );
        let s = d.stat();
        assert_eq!((s.files, s.added, s.removed), (6, 4, 3));

        // A reply in the thread starts from the same base; a bogus one is replaced.
        assert_eq!(
            super::base(&cwd, &roots, Some(base.clone())).await,
            Some(base.clone())
        );
        let bogus = DiffBase {
            head: "--output=x".into(),
            ..base.clone()
        };
        assert_ne!(
            super::base(&cwd, &roots, Some(bogus)).await.unwrap().head,
            "--output=x"
        );
        // A repo reaching above the worker's roots isn't diffed.
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let inner = [sub.canonicalize().unwrap()];
        assert_eq!(
            super::base(&sub.to_string_lossy(), &inner, None).await,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
        // Outside a repo there is no base.
        let plain = std::env::temp_dir().join(format!("mis-nogit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&plain).unwrap();
        let roots = [plain.canonicalize().unwrap()];
        assert_eq!(
            super::base(&plain.to_string_lossy(), &roots, None).await,
            None
        );
        let _ = std::fs::remove_dir_all(&plain);
    }
}

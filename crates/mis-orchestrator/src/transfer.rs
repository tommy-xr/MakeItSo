//! `export` / `import`: an install (state dir, routines, env file) as one `.tar.gz`, to move it
//! to another machine. What goes in and where each entry lands are pure ([`skipped`],
//! [`dest`], [`clash`], [`check_manifest`], [`missing_paths`]); [`export`] and [`import`] are
//! the filesystem shell. Both hold the state dir's lock, so neither runs beside an orchestrator.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};

use crate::apps::{entry_kind, entry_names, Kind};
use crate::ledger::persist;
use crate::store;

const MANIFEST: &str = "manifest.json";
const STATE: &str = "state";
const ROUTINES: &str = "routines";
const CONFIG: &str = "config";
const ENV: &str = "orchestrator.env";

/// `manifest.json`: what made the archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// The exporting build's [`crate::VERSION`].
    pub version: String,
    /// Its task history format ([`persist::VERSION`]).
    pub ledger_version: u32,
}

/// Where an install's files are (export) or go (import).
pub struct Paths {
    pub state: PathBuf,
    pub routines: PathBuf,
    pub env: PathBuf,
}

/// A folder of the install the archive mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Root {
    State,
    Routines,
}

/// Where an archive entry goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dest {
    Manifest,
    Env,
    /// `/`-separated path under the root; empty: the root itself.
    In(Root, String),
}

/// A temp name an interrupted write leaves: `x.tmp`, `.tmp-<id>`, `.draft-<id>`.
fn temp(name: &str) -> bool {
    name.ends_with(".tmp") || name.starts_with(".tmp-") || name.starts_with(".draft-")
}

/// Whether path `rel` under `root` stays out of an export. In the state dir: mis-up's
/// `builds/`, the lock, a ledger moved aside, and `notes/` (re-cloned; its history says what
/// was published). Anywhere: temp files, and SQLite's `-shm` (rebuilt from the `-wal`, kept).
pub(crate) fn skipped(root: Root, rel: &[&str]) -> bool {
    let Some(name) = rel.last() else {
        return false;
    };
    let state_only = root == Root::State
        && match rel {
            ["builds" | "notes", ..] | ["ledger.lock"] => true,
            [top] => top.starts_with("ledger.json.bad-"),
            _ => false,
        };
    state_only || temp(name) || name.ends_with(".sqlite-shm")
}

/// Where entry `names` (an [`entry_names`] path) of `kind` goes. Ok(None): a parent folder
/// only (the top, `config/`). Err: nothing an export holds.
pub(crate) fn dest(names: &[&str], kind: Kind) -> Result<Option<Dest>, String> {
    let root = |r: Root, rest: &[&str]| {
        let ok = !skipped(r, rest) && (kind == Kind::Dir || !rest.is_empty());
        ok.then(|| Dest::In(r, rest.join("/")))
    };
    let d = match (names, kind) {
        ([] | [CONFIG], Kind::Dir) => return Ok(None),
        ([MANIFEST], Kind::File) => Some(Dest::Manifest),
        ([CONFIG, ENV], Kind::File) => Some(Dest::Env),
        ([STATE, rest @ ..], _) => root(Root::State, rest),
        ([ROUTINES, rest @ ..], _) => root(Root::Routines, rest),
        _ => None,
    };
    d.map(Some)
        .ok_or_else(|| format!("{}: not part of an export", names.join("/")))
}

/// The first clash among entries `seen` (dest -> its kind), as an error: a path a file and
/// also a folder with something in it. (A file listed twice is caught as it is seen.)
pub(crate) fn clash(seen: &BTreeMap<(Root, String), Kind>) -> Option<String> {
    for (root, path) in seen.keys() {
        let mut parents = path.match_indices('/').map(|(i, _)| &path[..i]);
        let file = |p: &&str| seen.get(&(*root, p.to_string())) == Some(&Kind::File);
        if let Some(p) = parents.find(file) {
            return Some(format!("{p}: both a file and a folder"));
        }
    }
    None
}

/// `raw` (the archive's `manifest.json`), checked: this build reads its task history.
pub(crate) fn check_manifest(raw: Option<&[u8]>) -> Result<Manifest, String> {
    let raw = raw.ok_or("no manifest.json: not an orchestrator export")?;
    let m: Manifest = serde_json::from_slice(raw).map_err(|e| format!("manifest.json: {e}"))?;
    if m.ledger_version != persist::VERSION {
        return Err(format!(
            "exported by mis-orchestrator {} with task history format {}; this build ({}) reads {}: \
             import with a build that reads it",
            m.version,
            m.ledger_version,
            crate::VERSION,
            persist::VERSION
        ));
    }
    Ok(m)
}

/// The env file's absolute path values that `exists` says aren't here, as `KEY=value`: the
/// old machine's home, state dir or repo, to change for this one.
pub(crate) fn missing_paths(
    env: &str,
    exists: impl Fn(&Path) -> bool,
) -> Result<Vec<String>, String> {
    let vars = mis_types::env_file::parse(env)?;
    Ok((vars.into_iter())
        .filter(|(_, v)| Path::new(v).is_absolute() && !exists(Path::new(v)))
        .map(|(k, v)| format!("{k}={v}"))
        .collect())
}

/// What [`export`] wrote.
#[derive(Debug)]
pub struct Exported {
    /// The env file's included.
    pub files: usize,
    pub env: bool,
    pub routines: bool,
}

/// The install at `from` into new file `out` (`0600`), not inside the state dir. Refused while
/// an orchestrator uses the state dir ([`store::Locked`]).
pub fn export(out: &Path, from: &Paths) -> anyhow::Result<Exported> {
    ensure!(
        from.state.is_dir(),
        "no state dir at {}",
        from.state.display()
    );
    let _lock = store::lock(&from.state)?;
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let (parent, state) = (parent.canonicalize()?, from.state.canonicalize()?);
    ensure!(
        !parent.starts_with(&state),
        "{} is inside the state dir; write it elsewhere",
        out.display()
    );
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let file = opts
        .open(out)
        .with_context(|| format!("creating {}", out.display()))?;
    let written = write_archive(file, from);
    if written.is_err() {
        let _ = std::fs::remove_file(out);
    }
    written
}

fn write_archive(file: File, from: &Paths) -> anyhow::Result<Exported> {
    let gz = flate2::write::GzEncoder::new(BufWriter::new(file), flate2::Compression::default());
    let mut ar = tar::Builder::new(gz);
    let manifest = Manifest {
        version: crate::VERSION.into(),
        ledger_version: persist::VERSION,
    };
    let json = serde_json::to_vec_pretty(&manifest)?;
    append_file(&mut ar, MANIFEST, &json[..], json.len() as u64, None)?;
    let env = from.env.is_file();
    if env {
        let f = File::open(&from.env).with_context(|| format!("reading {}", from.env.display()))?;
        let meta = f.metadata()?;
        append_file(
            &mut ar,
            &format!("{CONFIG}/{ENV}"),
            f,
            meta.len(),
            Some(&meta),
        )?;
    }
    let mut files = env as usize + append_tree(&mut ar, &from.state, Root::State, &mut vec![])?;
    let routines = from.routines.is_dir();
    if routines {
        files += append_tree(&mut ar, &from.routines, Root::Routines, &mut vec![])?;
    }
    let mut out = ar.into_inner()?.finish()?.into_inner()?;
    out.flush()?;
    out.sync_all()?;
    Ok(Exported {
        files,
        env,
        routines,
    })
}

/// `dir/rel` and everything under it but what [`skipped`] leaves out, in name order; the
/// files appended. Anything but a file or folder (a link) stops the export.
fn append_tree<W: Write>(
    ar: &mut tar::Builder<W>,
    dir: &Path,
    root: Root,
    rel: &mut Vec<String>,
) -> anyhow::Result<usize> {
    let top = match root {
        Root::State => STATE,
        Root::Routines => ROUTINES,
    };
    let name = std::iter::once(top).chain(rel.iter().map(String::as_str));
    let name = name.collect::<Vec<_>>().join("/");
    let path = rel.iter().fold(dir.to_path_buf(), |p, n| p.join(n));
    let meta = std::fs::symlink_metadata(&path)?;
    if meta.is_file() {
        let f = File::open(&path).with_context(|| format!("reading {}", path.display()))?;
        append_file(ar, &name, f, meta.len(), Some(&meta))?;
        return Ok(1);
    }
    ensure!(
        meta.is_dir(),
        "{}: not a file or folder; move it out of the way",
        path.display()
    );
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_mode(0o700);
    h.set_size(0);
    ar.append_data(&mut h, format!("{name}/"), std::io::empty())?;
    let mut names = Vec::new();
    for e in std::fs::read_dir(&path)? {
        let n = e?.file_name();
        match n.to_str() {
            Some(n) => names.push(n.to_string()),
            None => bail!("{}: not a UTF-8 name", path.join(&n).display()),
        }
    }
    names.sort();
    let mut files = 0;
    for n in names {
        rel.push(n);
        let names: Vec<&str> = rel.iter().map(String::as_str).collect();
        if !skipped(root, &names) {
            files += append_tree(ar, dir, root, rel)?;
        }
        rel.pop();
    }
    Ok(files)
}

fn append_file<W: Write>(
    ar: &mut tar::Builder<W>,
    name: &str,
    data: impl Read,
    len: u64,
    meta: Option<&std::fs::Metadata>,
) -> anyhow::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mode(0o600);
    h.set_size(len);
    let mtime = meta.and_then(|m| m.modified().ok());
    let secs = mtime.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    h.set_mtime(secs.map_or(0, |d| d.as_secs()));
    // Stopped orchestrator: nothing grows or shrinks under the lock.
    ar.append_data(&mut h, name, data.take(len))?;
    Ok(())
}

/// What [`import`] placed.
#[derive(Debug)]
pub struct Imported {
    pub manifest: Manifest,
    pub files: usize,
    pub tasks: usize,
    pub env: bool,
    pub routines: bool,
    /// The env file's [`missing_paths`], or why it couldn't be read.
    pub missing: Result<Vec<String>, String>,
}

/// Archive `from` (an [`export`]) into `to`: checked whole first (entries, manifest, task
/// history), then written, files `0600` and folders `0700` whatever the archive says.
/// Refused if the state dir has anything in it, or the archive holds an env file or routines
/// and `to`'s exist.
pub fn import(from: &Path, to: &Paths) -> anyhow::Result<Imported> {
    // First pass: check everything, keep the few files judged before writing.
    let mut seen = BTreeMap::new();
    let (mut manifest, mut ledger, mut env) = (None, None, None);
    each_entry(from, |dest, kind, entry| {
        let mut read = || -> anyhow::Result<Vec<u8>> {
            let mut b = Vec::new();
            entry.read_to_end(&mut b)?;
            Ok(b)
        };
        let twice = |what: &str| anyhow::anyhow!("{what}: listed twice");
        let key = match dest {
            Dest::Manifest => return ensure_once(&mut manifest, read()?, || twice(MANIFEST)),
            Dest::Env => return ensure_once(&mut env, read()?, || twice(ENV)),
            Dest::In(root, p) => (root, p),
        };
        if key == (Root::State, store::LEDGER_FILE.to_string()) {
            ledger = Some(read()?);
        }
        // A folder may be listed again; a file can't.
        let before = seen.insert(key.clone(), kind);
        if before.is_some() && (before, kind) != (Some(Kind::Dir), Kind::Dir) {
            return Err(twice(&key.1));
        }
        Ok(())
    })?;
    let manifest = check_manifest(manifest.as_deref()).map_err(anyhow::Error::msg)?;
    if let Some(c) = clash(&seen) {
        bail!("{}: {c}", from.display());
    }
    let tasks = match &ledger {
        Some(json) => {
            let json = std::str::from_utf8(json).context("state/ledger.json: not UTF-8")?;
            let l = crate::readable(json).map_err(|e| anyhow::anyhow!("state/ledger.json: {e}"))?;
            match l.status() {
                mis_types::ServerMsg::Status { tasks, .. } => tasks.len(),
                _ => 0,
            }
        }
        None => 0,
    };
    let routines = seen.keys().any(|(r, _)| *r == Root::Routines);
    refuse_nonempty(&to.state, "state dir")?;
    if routines {
        refuse_nonempty(&to.routines, "routines dir")?;
    }
    if env.is_some() {
        ensure!(
            !to.env.exists(),
            "{} exists; move it aside to import",
            to.env.display()
        );
    }
    private_dir(&to.state)?;
    let _lock = store::lock(&to.state)?;
    let mut files = 0;
    let wrote = each_entry(from, |dest, kind, entry| {
        let path = match dest {
            Dest::Manifest => return Ok(()),
            Dest::Env => to.env.clone(),
            Dest::In(root, p) => {
                let dir = if root == Root::State {
                    &to.state
                } else {
                    &to.routines
                };
                p.split('/')
                    .filter(|n| !n.is_empty())
                    .fold(dir.clone(), |d, n| d.join(n))
            }
        };
        if kind == Kind::Dir {
            return private_dir(&path);
        }
        private_dir(path.parent().expect("under a root"))?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        std::io::copy(entry, &mut f)?;
        // Images expire, and captures dedupe, by age.
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.header().mtime()?);
        f.set_modified(mtime)?;
        f.sync_all()?;
        files += 1;
        Ok(())
    });
    wrote.with_context(|| {
        format!(
            "import cut short: remove what it wrote in {} before trying again",
            to.state.display()
        )
    })?;
    let missing = match &env {
        Some(text) => std::str::from_utf8(text)
            .map_err(|_| "not UTF-8".to_string())
            .and_then(|t| missing_paths(t, Path::exists)),
        None => Ok(vec![]),
    };
    Ok(Imported {
        manifest,
        files,
        tasks,
        env: env.is_some(),
        routines,
        missing,
    })
}

/// `slot = Some(v)`, unless it already was: `twice()`.
fn ensure_once(
    slot: &mut Option<Vec<u8>>,
    v: Vec<u8>,
    twice: impl FnOnce() -> anyhow::Error,
) -> anyhow::Result<()> {
    match slot.replace(v) {
        None => Ok(()),
        Some(_) => Err(twice()),
    }
}

type Entry<'a> = tar::Entry<'a, flate2::read::GzDecoder<BufReader<File>>>;

/// Each entry of archive `path` that goes somewhere, checked by [`entry_kind`],
/// [`entry_names`] and [`dest`], in order.
fn each_entry(
    path: &Path,
    mut f: impl FnMut(Dest, Kind, &mut Entry) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file)));
    let bad = |e: String| anyhow::anyhow!("{}: {e}", path.display());
    for entry in ar.entries()? {
        let mut entry = entry?;
        let raw = entry.path_bytes().into_owned();
        let shown = String::from_utf8_lossy(&raw).into_owned();
        let Some(kind) =
            (entry_kind(entry.header().entry_type())).map_err(|e| bad(format!("{shown}: {e}")))?
        else {
            continue;
        };
        let names = entry_names(&raw).map_err(bad)?;
        if let Some(dest) = dest(&names, kind).map_err(bad)? {
            f(dest, kind, &mut entry)?;
        }
    }
    Ok(())
}

fn refuse_nonempty(dir: &Path, what: &str) -> anyhow::Result<()> {
    let used = match std::fs::read_dir(dir) {
        Ok(mut d) => d.next().is_some(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(anyhow::Error::from(e).context(dir.display().to_string())),
    };
    ensure!(
        !used,
        "{what} {} isn't empty; import only into a new install",
        dir.display()
    );
    Ok(())
}

/// `dir` and its missing parents, made `0700`.
fn private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(p: &str) -> Vec<&str> {
        p.split('/').filter(|n| !n.is_empty()).collect()
    }

    #[test]
    fn what_stays_out_of_an_export() {
        let state = |p: &str| skipped(Root::State, &names(p));
        for p in [
            "builds/abc/mis-worker",
            "notes",
            "notes/.git/HEAD",
            "ledger.lock",
            "ledger.json.bad-1700000000",
            "ledger.tmp",
            "notes.tmp",
            "apps/pool/releases/.tmp-1",
            "apps/pool/.draft-1",
            "apps/pool/data.sqlite-shm",
        ] {
            assert!(state(p), "{p}");
        }
        for p in [
            "",
            "ledger.json",
            "transcripts/1.json",
            "vapid.pem",
            "apps/pool/data.sqlite",
            "apps/pool/data.sqlite-wal",
            "apps/notes/x",
            "captures/published.json",
        ] {
            assert!(!state(p), "{p}");
        }
        // Only the state dir has a lock, builds and a notes clone.
        assert!(!skipped(Root::Routines, &names("notes/a.md")));
        assert!(skipped(Root::Routines, &names("a.md.tmp")));
    }

    #[test]
    fn each_entry_has_one_place() {
        use Kind::*;
        let at = |p: &str, k| dest(&names(p), k);
        let under = |r, p: &str| Ok(Some(Dest::In(r, p.into())));
        assert_eq!(at("manifest.json", File), Ok(Some(Dest::Manifest)));
        assert_eq!(at("config/orchestrator.env", File), Ok(Some(Dest::Env)));
        assert_eq!(at("config", Dir), Ok(None));
        assert_eq!(at("", Dir), Ok(None));
        assert_eq!(at("state", Dir), under(Root::State, ""));
        assert_eq!(at("state/apps/x", File), under(Root::State, "apps/x"));
        assert_eq!(
            at("routines/daily.md", File),
            under(Root::Routines, "daily.md")
        );
        for (p, k) in [
            ("state", File),
            ("manifest.json", Dir),
            ("config/worker.toml", File),
            ("config/orchestrator.env", Dir),
            ("state/builds/x", File),
            ("state/ledger.lock", File),
            ("other/x", File),
            (".bashrc", File),
        ] {
            assert!(
                at(p, k).unwrap_err().contains("not part of an export"),
                "{p}"
            );
        }
    }

    #[test]
    fn a_file_with_something_in_it_clashes() {
        let seen = |e: &[(&str, Kind)]| {
            (e.iter())
                .map(|(p, k)| ((Root::State, p.to_string()), *k))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(clash(&seen(&[("a", Kind::Dir), ("a/b", Kind::File)])), None);
        let got = clash(&seen(&[("a", Kind::File), ("a/b/c", Kind::File)]));
        assert_eq!(got.as_deref(), Some("a: both a file and a folder"));
        // Each root its own tree.
        let mut two = seen(&[("a", Kind::File)]);
        two.insert((Root::Routines, "a/b".into()), Kind::File);
        assert_eq!(clash(&two), None);
    }

    #[test]
    fn manifest_must_name_this_builds_task_history_format() {
        let json = |v: u32| format!(r#"{{"version":"0.1.0+old","ledger_version":{v}}}"#);
        let ok = check_manifest(Some(json(persist::VERSION).as_bytes())).unwrap();
        assert_eq!(ok.version, "0.1.0+old");
        let v = persist::VERSION;
        let newer = check_manifest(Some(json(v + 1).as_bytes())).unwrap_err();
        let want = format!("0.1.0+old with task history format {}; ", v + 1);
        assert!(
            newer.contains(&want) && newer.contains(&format!("reads {v}")),
            "{newer}"
        );
        assert!(check_manifest(None)
            .unwrap_err()
            .contains("not an orchestrator export"));
        assert!(check_manifest(Some(b"{}"))
            .unwrap_err()
            .starts_with("manifest.json:"));
    }

    #[test]
    fn env_paths_missing_here_are_named() {
        let env = "MIS_STATE_DIR=/old/home/.local/state/mis\nMIS_BIND=127.0.0.1:7474\n\
                   MIS_NOTES_REMOTE=/here/notes.git\nMIS_ROUTINES_DIR=relative/dir\n";
        let got = missing_paths(env, |p| p.starts_with("/here")).unwrap();
        assert_eq!(got, ["MIS_STATE_DIR=/old/home/.local/state/mis"]);
        assert!(missing_paths("X=$HOME", |_| true).is_err());
    }
}

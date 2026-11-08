//! Snapshots of each app's records in `<state>/apps/<slug>/backups/`: `<name>.sqlite` (copied
//! through SQLite's backup API, `0600`, no WAL) beside `<name>.policy.json` (its members then).
//! Names, scheduling and retention are pure; the rest is the shell [`crate::apps::Apps`] calls
//! under its lock.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime, TimeZone};
use mis_types::{AppBackup, BackupKind, Restored};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::app_access::Policy;
use crate::records_db::Records;

const DIR: &str = "backups";
const DB: &str = ".sqlite";
const POLICY: &str = ".policy.json";
/// UTC, sortable, no `-`: `20261108T031500.123Z`.
const STAMP: &str = "%Y%m%dT%H%M%S%.3fZ";

/// What a snapshot is taken for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    Daily,
    /// Before release n goes live.
    PreActivation(u32),
    PreRestore,
    Manual,
}

impl Take {
    pub fn kind(self) -> BackupKind {
        match self {
            Self::Daily => BackupKind::Daily,
            Self::PreActivation(_) => BackupKind::PreActivation,
            Self::PreRestore => BackupKind::PreRestore,
            Self::Manual => BackupKind::Manual,
        }
    }
}

/// How many of a kind are kept, newest first.
pub fn keep(kind: BackupKind) -> usize {
    match kind {
        BackupKind::Daily => 7,
        _ => 5,
    }
}

/// `daily-<stamp>`, `pre-v<n>-<stamp>`, `pre-restore-<stamp>`, `manual-<stamp>`; Unix millis `at`.
pub fn name(take: Take, at: u64) -> String {
    let stamp = chrono::DateTime::from_timestamp_millis(at as i64)
        .expect("a Unix millis time")
        .format(STAMP);
    match take {
        Take::Daily => format!("daily-{stamp}"),
        Take::PreActivation(n) => format!("pre-v{n}-{stamp}"),
        Take::PreRestore => format!("pre-restore-{stamp}"),
        Take::Manual => format!("manual-{stamp}"),
    }
}

/// What [`name`] made `s` from. None for anything else, so a valid name holds no path.
pub fn parse(s: &str) -> Option<(Take, u64)> {
    let (prefix, stamp) = s.rsplit_once('-')?;
    let take = match prefix {
        "daily" => Take::Daily,
        "pre-restore" => Take::PreRestore,
        "manual" => Take::Manual,
        p => Take::PreActivation(p.strip_prefix("pre-v")?.parse().ok()?),
    };
    let at = NaiveDateTime::parse_from_str(stamp, STAMP).ok()?;
    let at = u64::try_from(at.and_utc().timestamp_millis()).ok()?;
    (name(take, at) == s).then_some((take, at))
}

/// One app, as [`due_backups`] weighs it.
pub struct Daily<'a> {
    pub slug: &'a str,
    /// Its newest daily snapshot (Unix millis).
    pub last_daily: Option<u64>,
    /// Its records differ from its newest snapshot of any kind (or it has none).
    pub changed: bool,
}

/// The apps owed a daily snapshot at `now`: changed, and none taken yet this local day in `tz`.
pub fn due_backups<'a, Tz: TimeZone>(apps: &[Daily<'a>], now: u64, tz: &Tz) -> Vec<&'a str> {
    let day = |ms: u64| -> NaiveDate {
        tz.timestamp_millis_opt(ms as i64)
            .unwrap()
            .date_naive()
    };
    (apps.iter())
        .filter(|a| a.changed && a.last_daily.is_none_or(|at| day(at) < day(now)))
        .map(|a| a.slug)
        .collect()
}

/// The `kind` snapshots in `list` beyond its newest [`keep`]: the ones to delete.
pub fn prune(list: &[AppBackup], kind: BackupKind) -> Vec<String> {
    let mut of: Vec<&AppBackup> = list.iter().filter(|b| b.kind == kind).collect();
    of.sort_by(|a, b| (b.at, &b.name).cmp(&(a.at, &a.name)));
    of.into_iter()
        .skip(keep(kind))
        .map(|b| b.name.clone())
        .collect()
}

/// Why a backup or restore didn't happen.
#[derive(Debug)]
pub enum BackupError {
    NoSuchApp,
    NoSuchBackup,
    /// Not a name [`name`] makes.
    BadName,
    Io(String),
}

impl From<anyhow::Error> for BackupError {
    fn from(e: anyhow::Error) -> Self {
        Self::Io(format!("{e:#}"))
    }
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<rusqlite::Error> for BackupError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Io(e.to_string())
    }
}

fn file(app: &Path, name: &str, ext: &str) -> PathBuf {
    app.join(DIR).join(format!("{name}{ext}"))
}

/// The snapshots of the app in folder `app`, oldest first.
pub fn list(app: &Path) -> Vec<AppBackup> {
    let Ok(entries) = std::fs::read_dir(app.join(DIR)) else {
        return vec![];
    };
    let mut out: Vec<AppBackup> = (entries.flatten())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let name = name.strip_suffix(DB)?;
            let (take, at) = parse(name)?;
            Some(AppBackup {
                name: name.into(),
                kind: take.kind(),
                at,
                size: e.metadata().ok()?.len(),
            })
        })
        .collect();
    out.sort_by(|a, b| (a.at, &a.name).cmp(&(b.at, &b.name)));
    out
}

/// Snapshot app `slug` (in folder `app`) as `take` at `at`, then delete that kind's oldest past
/// [`keep`]. None: it has no records yet.
pub fn take(
    app: &Path,
    records: &Records,
    slug: &str,
    take: Take,
    at: u64,
) -> Result<Option<AppBackup>, BackupError> {
    if !records.has(slug) {
        return Ok(None);
    }
    let db = records.db(slug)?;
    let made = snapshot(app, &db.lock().unwrap(), take, at)?;
    prune_kind(app, take.kind());
    Ok(Some(made))
}

/// Replace `slug`'s records with snapshot `name`'s, and its policy too if `with_policy`. Its
/// records are first snapshotted as pre-restore, in the same hold of the app's lock: each write
/// lands either in that snapshot or after the restore.
pub fn restore(
    app: &Path,
    records: &Records,
    slug: &str,
    name: &str,
    with_policy: bool,
    at: u64,
) -> Result<Restored, BackupError> {
    parse(name).ok_or(BackupError::BadName)?;
    let from = file(app, name, DB);
    if !from.is_file() {
        return Err(BackupError::NoSuchBackup);
    }
    // Read first, so a missing policy fails before anything changes.
    let policy = (with_policy)
        .then(|| std::fs::read(file(app, name, POLICY)))
        .transpose()?;
    let src = Connection::open_with_flags(&from, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let had = records.has(slug);
    let db = records.db(slug)?;
    let mut conn = db.lock().unwrap();
    let pre_restore = (had)
        .then(|| snapshot(app, &conn, Take::PreRestore, at))
        .transpose()?;
    copy_pages(&src, &mut conn)?;
    // Closed before pruning, which may delete the file (Windows can't while it's open).
    drop((conn, src));
    if let Some(policy) = policy {
        crate::push::write_private(&app.join("policy.json"), &policy)?;
    }
    prune_kind(app, BackupKind::PreRestore);
    Ok(Restored { pre_restore })
}

/// Whether `slug`'s records differ from those in snapshot `name`.
pub fn changed_since(
    app: &Path,
    records: &Records,
    slug: &str,
    name: &str,
) -> anyhow::Result<bool> {
    let snap = Connection::open_with_flags(file(app, name, DB), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let then = fingerprint(&snap)?;
    let db = records.db(slug)?;
    let now = fingerprint(&db.lock().unwrap())?;
    Ok(now != then)
}

/// Take app `slug`'s daily snapshot if [`due_backups`] says it's owed one.
pub fn daily<Tz: TimeZone>(
    app: &Path,
    records: &Records,
    slug: &str,
    now: u64,
    tz: &Tz,
) -> Result<Option<AppBackup>, BackupError> {
    if !records.has(slug) {
        return Ok(None);
    }
    let all = list(app);
    let last_daily = (all.iter().rev())
        .find(|b| b.kind == BackupKind::Daily)
        .map(|b| b.at);
    let changed = match all.last() {
        Some(b) => changed_since(app, records, slug, &b.name)?,
        None => true,
    };
    let me = Daily {
        slug,
        last_daily,
        changed,
    };
    match due_backups(&[me], now, tz)[..] {
        [] => Ok(None),
        _ => take(app, records, slug, Take::Daily, now),
    }
}

/// Snapshot `conn` (its app's lock held) and the app's policy into new files.
fn snapshot(app: &Path, conn: &Connection, take: Take, mut at: u64) -> anyhow::Result<AppBackup> {
    crate::images::private_dir(&app.join(DIR))?;
    // Two in the same millisecond each get their own name.
    while file(app, &name(take, at), DB).exists() {
        at += 1;
    }
    let name = name(take, at);
    let policy = match std::fs::read(app.join("policy.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_vec_pretty(&Policy::default()).expect("policy serializes")
        }
        Err(e) => return Err(e.into()),
    };
    let side = file(app, &name, POLICY);
    crate::push::write_private(&side, &policy)?;
    // Copied to a temp name, so a half-written snapshot is never listed.
    let tmp = app.join(DIR).join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let dst = file(app, &name, DB);
    let copied = copy_to(conn, &tmp).and_then(|()| Ok(std::fs::rename(&tmp, &dst)?));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&side);
        return Err(e);
    }
    Ok(AppBackup {
        name,
        kind: take.kind(),
        at,
        size: std::fs::metadata(&dst)?.len(),
    })
}

fn prune_kind(app: &Path, kind: BackupKind) {
    for old in prune(&list(app), kind) {
        for ext in [DB, POLICY] {
            if let Err(e) = std::fs::remove_file(file(app, &old, ext)) {
                tracing::warn!("removing backup {old}{ext}: {e}");
            }
        }
    }
}

/// `conn`'s database into new file `to` (`0600`), in rollback-journal mode so it is one file.
fn copy_to(conn: &Connection, to: &Path) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(to)?;
    let mut dst = Connection::open(to)?;
    copy_pages(conn, &mut dst)?;
    let mode: String = dst.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
    anyhow::ensure!(mode == "delete", "{}: journal mode {mode}", to.display());
    Ok(())
}

/// Every page of `from` into `to`, in one step so no write can land between pages.
fn copy_pages(from: &Connection, to: &mut Connection) -> anyhow::Result<()> {
    let step = Backup::new(from, to)?.step(-1)?;
    anyhow::ensure!(step == StepResult::Done, "backup stopped: {step:?}");
    Ok(())
}

/// Equal exactly when the `records` tables hold the same rows.
fn fingerprint(conn: &Connection) -> rusqlite::Result<[u8; 32]> {
    let mut stmt = conn.prepare("SELECT * FROM records ORDER BY space, owner, collection, id")?;
    let n = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut h = Sha256::new();
    while let Some(r) = rows.next()? {
        for i in 0..n {
            // Debug tags each value's type and quotes text, so no two rows hash alike.
            h.update(format!("{:?}\n", r.get::<_, rusqlite::types::Value>(i)?));
        }
    }
    Ok(h.finalize().into())
}

/// Daily snapshots of every app: checked at start, then hourly, by this machine's local day.
pub async fn run_daily(apps: std::sync::Arc<crate::apps::Apps>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let (apps, now) = (apps.clone(), crate::unix_ms());
        let _ = tokio::task::spawn_blocking(move || apps.daily_backups(now, &chrono::Local)).await;
    }
}

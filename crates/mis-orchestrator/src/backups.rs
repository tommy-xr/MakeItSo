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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};
    use mis_types::{Mutation, Space};
    use serde_json::json;

    fn ms(rfc3339: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis() as u64
    }

    #[test]
    fn names_round_trip_and_nothing_else_parses() {
        let at = ms("2026-11-08T03:15:00.123Z");
        for (take, want) in [
            (Take::Daily, "daily-20261108T031500.123Z"),
            (Take::PreActivation(3), "pre-v3-20261108T031500.123Z"),
            (Take::PreRestore, "pre-restore-20261108T031500.123Z"),
            (Take::Manual, "manual-20261108T031500.123Z"),
        ] {
            assert_eq!(name(take, at), want);
            assert_eq!(parse(want), Some((take, at)));
        }
        for bad in [
            "",
            "manual",
            "manual-",
            "../manual-20261108T031500.123Z",
            "manual-20261108T031500.123Z/..",
            "manual-20261108T031500.123Z.sqlite",
            "manual-20261108T031500Z",
            "Manual-20261108T031500.123Z",
            "pre-v03-20261108T031500.123Z",
            "pre-v-20261108T031500.123Z",
            "weekly-20261108T031500.123Z",
            "manual-20261332T031500.123Z",
            "manual-20261108T031500.123Z\0",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn daily_is_due_once_a_local_day_and_only_when_changed() {
        let ny = FixedOffset::west_opt(5 * 3600).unwrap();
        let app = |slug, last_daily: Option<&str>, changed| Daily {
            slug,
            last_daily: last_daily.map(ms),
            changed,
        };
        // 23:30 on the 7th in New York (04:30 UTC on the 8th).
        let late = ms("2026-11-08T04:30:00Z");
        let apps = [
            app("never", None, true),
            app("unchanged", None, false),
            // 01:00 on the 7th in New York, but the 7th in UTC too.
            app("same-day", Some("2026-11-07T06:00:00Z"), true),
            app("yesterday", Some("2026-11-06T20:00:00Z"), true),
            app("yesterday-unchanged", Some("2026-11-06T20:00:00Z"), false),
        ];
        assert_eq!(due_backups(&apps, late, &ny), ["never", "yesterday"]);
        // In UTC it is already the 8th, so same-day's snapshot is a day old.
        assert_eq!(
            due_backups(&apps, late, &Utc),
            ["never", "same-day", "yesterday"]
        );
        // An hour later it is the 8th in New York too.
        let next = ms("2026-11-08T05:30:00Z");
        assert_eq!(
            due_backups(&apps, next, &ny),
            ["never", "same-day", "yesterday"]
        );
    }

    #[test]
    fn prune_keeps_each_kinds_newest() {
        let b = |kind, at| AppBackup {
            name: format!("{kind:?}-{at}"),
            kind,
            at,
            size: 1,
        };
        let mut list: Vec<AppBackup> = (1..=9).map(|at| b(BackupKind::Daily, at)).collect();
        list.extend((1..=6).map(|at| b(BackupKind::Manual, at * 100)));
        list.extend((1..=5).map(|at| b(BackupKind::PreActivation, at)));
        list.push(b(BackupKind::PreRestore, 1));
        assert_eq!(prune(&list, BackupKind::Daily), ["Daily-2", "Daily-1"]);
        assert_eq!(prune(&list, BackupKind::Manual), ["Manual-100"]);
        assert!(prune(&list, BackupKind::PreActivation).is_empty());
        assert!(prune(&list, BackupKind::PreRestore).is_empty());
        let pre: Vec<_> = (1..=8).map(|at| b(BackupKind::PreRestore, at)).collect();
        assert_eq!(
            prune(&pre, BackupKind::PreRestore),
            ["PreRestore-3", "PreRestore-2", "PreRestore-1"]
        );
    }

    /// An apps folder with app `pool` (no records yet), its store, and `pool`'s folder.
    fn temp() -> (Records, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mis-backups-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("pool")).unwrap();
        (Records::new(dir.clone()), dir.join("pool"), dir)
    }

    fn put(r: &Records, id: &str, rev: u64, v: i64) {
        let op = Mutation::Put {
            space: Space::Shared,
            collection: "notes".into(),
            id: id.into(),
            rev,
            data: json!(v),
        };
        r.mutate("pool", &op, "me", 1).unwrap().unwrap();
    }

    /// `pool`'s shared notes: (id, data).
    fn notes(r: &Records) -> Vec<(String, serde_json::Value)> {
        let (shared, _) = r.visible("pool", "me").unwrap();
        (shared.get("notes").into_iter().flatten())
            .map(|e| (e.id.clone(), e.data.clone()))
            .collect()
    }

    #[test]
    fn snapshots_restore_and_the_pre_restore_one_undoes_it() {
        let (r, app, dir) = temp();
        assert!(take(&app, &r, "pool", Take::Manual, 1).unwrap().is_none());
        assert!(!r.has("pool"), "no database made just to back it up");
        put(&r, "a", 0, 1);
        std::fs::write(app.join("policy.json"), br#"{"members":{"x@y":"editor"}}"#).unwrap();
        let snap = take(&app, &r, "pool", Take::Manual, 1).unwrap().unwrap();
        assert_eq!(snap.name, name(Take::Manual, 1));
        // A second in the same millisecond gets its own name.
        let again = take(&app, &r, "pool", Take::Manual, 1).unwrap().unwrap();
        assert_eq!(again.at, 2);
        put(&r, "a", 1, 2);
        put(&r, "b", 0, 3);
        std::fs::write(app.join("policy.json"), b"{}").unwrap();
        let newer = notes(&r);
        let done = restore(&app, &r, "pool", &snap.name, false, 10).unwrap();
        assert_eq!(notes(&r), [("a".into(), json!(1))]);
        // Members stay unless asked for.
        assert_eq!(std::fs::read(app.join("policy.json")).unwrap(), b"{}");
        let pre = done.pre_restore.unwrap();
        assert_eq!(pre.kind, BackupKind::PreRestore);
        restore(&app, &r, "pool", &pre.name, false, 11).unwrap();
        assert_eq!(notes(&r), newer);
        restore(&app, &r, "pool", &snap.name, true, 12).unwrap();
        assert_eq!(
            std::fs::read(app.join("policy.json")).unwrap(),
            br#"{"members":{"x@y":"editor"}}"#
        );
        // Writes go on from the restored revisions.
        put(&r, "a", 1, 4);
        assert!(matches!(
            restore(&app, &r, "pool", "../data", false, 13),
            Err(BackupError::BadName)
        ));
        assert!(matches!(
            restore(&app, &r, "pool", &name(Take::Daily, 5), false, 13),
            Err(BackupError::NoSuchBackup)
        ));
        // One file each, no WAL beside it, readable only by this user.
        let kinds: Vec<_> = list(&app).iter().map(|b| b.kind).collect();
        assert_eq!(kinds.len(), 5, "{kinds:?}");
        for e in std::fs::read_dir(app.join(DIR)).unwrap().flatten() {
            let name = e.file_name().into_string().unwrap();
            assert!(name.ends_with(DB) || name.ends_with(POLICY), "{name}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = e.metadata().unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{name}");
            }
        }
        drop(r);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn each_kind_keeps_its_newest() {
        let (r, app, dir) = temp();
        put(&r, "a", 0, 1);
        for at in 0..7 {
            take(&app, &r, "pool", Take::Manual, at * 1000).unwrap();
            take(&app, &r, "pool", Take::Daily, at * 1000).unwrap();
        }
        let left = list(&app);
        let of = |k| left.iter().filter(|b| b.kind == k).count();
        assert_eq!((of(BackupKind::Manual), of(BackupKind::Daily)), (5, 7));
        assert_eq!(left[0].at, 0, "the oldest daily stays");
        // Their policies went with them.
        let files = std::fs::read_dir(app.join(DIR)).unwrap().count();
        assert_eq!(files, 24);
        drop(r);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn daily_skips_unchanged_apps_and_takes_one_a_day() {
        let (r, app, dir) = temp();
        let day1 = ms("2026-11-08T12:00:00Z");
        let day2 = ms("2026-11-09T12:00:00Z");
        assert!(daily(&app, &r, "pool", day1, &Utc).unwrap().is_none());
        put(&r, "a", 0, 1);
        assert!(daily(&app, &r, "pool", day1, &Utc).unwrap().is_some());
        put(&r, "a", 1, 2);
        // Changed, but today's is taken.
        assert!(daily(&app, &r, "pool", day1 + 3_600_000, &Utc)
            .unwrap()
            .is_none());
        // Unchanged since a manual snapshot: nothing to keep.
        take(&app, &r, "pool", Take::Manual, day1 + 7_200_000).unwrap();
        assert!(daily(&app, &r, "pool", day2, &Utc).unwrap().is_none());
        // A remove is a change too.
        let rm = Mutation::Remove {
            space: Space::Shared,
            collection: "notes".into(),
            id: "a".into(),
            rev: 2,
        };
        r.mutate("pool", &rm, "me", 1).unwrap().unwrap();
        assert!(daily(&app, &r, "pool", day2, &Utc).unwrap().is_some());
        drop(r);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_during_a_restore_land_before_or_after_it_never_lost() {
        let (r, app, dir) = temp();
        put(&r, "seed", 0, 0);
        let snap = take(&app, &r, "pool", Take::Manual, 1).unwrap().unwrap();
        let r = std::sync::Arc::new(r);
        let writer = {
            let r = r.clone();
            std::thread::spawn(move || {
                for i in 0..300 {
                    put(&r, &format!("w{i:03}"), 0, i);
                }
            })
        };
        while notes(&r).len() < 20 {
            std::thread::yield_now();
        }
        let done = restore(&app, &r, "pool", &snap.name, false, 2).unwrap();
        writer.join().unwrap();
        let pre = done.pre_restore.unwrap();
        let after: Vec<String> = notes(&r).into_iter().map(|(id, _)| id).collect();
        restore(&app, &r, "pool", &pre.name, false, 3).unwrap();
        let before: Vec<String> = notes(&r).into_iter().map(|(id, _)| id).collect();
        // Every write is in exactly one: those before the restore in its pre-restore
        // snapshot, the rest after it.
        let k = before.len() - 1;
        assert!((20..300).contains(&k), "{k}");
        let ids = |r: std::ops::Range<usize>| -> Vec<String> {
            r.map(|i| format!("w{i:03}")).collect()
        };
        assert_eq!(before, [vec!["seed".to_string()], ids(0..k)].concat());
        assert_eq!(after, [vec!["seed".to_string()], ids(k..300)].concat());
        drop(r);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

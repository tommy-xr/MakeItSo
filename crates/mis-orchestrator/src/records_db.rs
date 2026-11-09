//! Each app's records in `<state>/apps/<slug>/data.sqlite` (WAL, `0600`): one connection per
//! app behind a mutex, so its writes are serialized. Blocking; call off the async workers.
//! What a write does and what a query means: [`crate::records`].

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mis_types::{Envelope, Mutation, Page, RecordQuery, Space};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::records::{self, Change, Compiled, Current, Refusal};

/// `data` NULL: a removed record, kept for its revision ([`Current::Gone`]).
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS records (
    space TEXT NOT NULL,
    owner TEXT NOT NULL,
    collection TEXT NOT NULL,
    id TEXT NOT NULL,
    rev INTEGER NOT NULL,
    \"by\" TEXT NOT NULL,
    at INTEGER NOT NULL,
    data TEXT,
    base INTEGER NOT NULL,
    PRIMARY KEY (space, owner, collection, id)
)";

/// The apps folder's databases, opened on first use.
pub struct Records {
    dir: PathBuf,
    open: Mutex<HashMap<String, Arc<Mutex<Connection>>>>,
    /// [`records::MAX_DB`]; smaller in tests.
    cap: u64,
}

/// A batch's records after its ops, or the first op refused (its index).
pub type Batched = Result<Vec<Option<Envelope>>, (usize, Refusal)>;

/// Every record a member can see, by collection: `(shared, me)`.
pub type Visible = (
    BTreeMap<String, Vec<Envelope>>,
    BTreeMap<String, Vec<Envelope>>,
);

impl Records {
    /// `dir`: the apps folder, holding `<slug>/`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            open: Mutex::default(),
            cap: records::MAX_DB,
        }
    }

    /// Forget `slug`'s open connection (its folder is going away): the next use opens afresh.
    pub fn close(&self, slug: &str) {
        self.open.lock().unwrap().remove(slug);
    }

    /// Whether `slug` has a database yet.
    pub fn has(&self, slug: &str) -> bool {
        self.dir.join(slug).join("data.sqlite").exists()
    }

    /// `slug`'s database (a [`crate::apps::valid_slug`] whose folder exists); its lock
    /// serializes every write.
    pub(crate) fn db(&self, slug: &str) -> anyhow::Result<Arc<Mutex<Connection>>> {
        let mut open = self.open.lock().unwrap();
        if let Some(db) = open.get(slug) {
            return Ok(db.clone());
        }
        let path = self.dir.join(slug).join("data.sqlite");
        // Made 0600 before SQLite opens it; its -wal and -shm files take the same mode.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&path)?;
        let conn = Connection::open(&path)?;
        // Another connection (a backup, an overlapping restart) waits rather than fails.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        anyhow::ensure!(mode == "wal", "{}: journal mode {mode}", path.display());
        conn.execute_batch(SCHEMA)?;
        let db = Arc::new(Mutex::new(conn));
        open.insert(slug.into(), db.clone());
        Ok(db)
    }

    pub fn get(
        &self,
        slug: &str,
        space: Space,
        owner: &str,
        collection: &str,
        id: &str,
    ) -> anyhow::Result<Option<Envelope>> {
        let db = self.db(slug)?;
        let conn = db.lock().unwrap();
        Ok(match read(&conn, space, owner, collection, id)? {
            Current::Live { record, .. } => Some(record),
            Current::Gone(_) => None,
        })
    }

    /// One page of `q` (already [`records::compile`]d as `c`).
    pub fn list(&self, slug: &str, q: &RecordQuery, c: &Compiled) -> anyhow::Result<Page> {
        let db = self.db(slug)?;
        let conn = db.lock().unwrap();
        let mut stmt = conn.prepare(&c.sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(&c.params), |r| {
            Ok((envelope(r)?, r.get::<_, rusqlite::types::Value>(5)?))
        })?;
        let mut rows = rows.collect::<Result<Vec<_>, _>>()?;
        let more = rows.len() > c.limit as usize;
        rows.truncate(c.limit as usize);
        let cursor = more
            .then(|| rows.last())
            .flatten()
            .map(|(e, k)| records::cursor(q, records::sql_value(k.clone()), &e.id));
        Ok(Page {
            records: rows.into_iter().map(|(e, _)| e).collect(),
            cursor,
        })
    }

    /// Apply `op` by `actor` at Unix millis `now`: the record after it (None: removed), or why
    /// not. Read, decided and written in one transaction under the app's lock.
    pub fn mutate(
        &self,
        slug: &str,
        op: &Mutation,
        actor: &str,
        now: u64,
    ) -> anyhow::Result<Result<Option<Envelope>, Refusal>> {
        let done = self.mutate_all(slug, std::slice::from_ref(op), actor, now)?;
        Ok(done
            .map(|mut records| records.remove(0))
            .map_err(|(_, r)| r))
    }

    /// [`Self::mutate`] for each of `ops` in order, all in one transaction: every record after
    /// its op, or the first op refused (its index) and nothing written.
    pub fn mutate_all(
        &self,
        slug: &str,
        ops: &[Mutation],
        actor: &str,
        now: u64,
    ) -> anyhow::Result<Batched> {
        let db = self.db(slug)?;
        let mut conn = db.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut records = Vec::with_capacity(ops.len());
        for (i, op) in ops.iter().enumerate() {
            match self.write(&tx, op, actor, now)? {
                Ok(e) => records.push(e),
                // Returning drops `tx`, rolling back the ops before this one too.
                Err(refused) => return Ok(Err((i, refused))),
            }
        }
        tx.commit()?;
        Ok(Ok(records))
    }

    /// One op inside `tx`, which sees the batch's earlier writes.
    fn write(
        &self,
        tx: &Connection,
        op: &Mutation,
        actor: &str,
        now: u64,
    ) -> anyhow::Result<Result<Option<Envelope>, Refusal>> {
        let (space, collection, id) = op.key();
        let owner = records::owner(space, actor);
        let current = read(tx, space, owner, collection, id)?;
        Ok(Ok(match records::apply(&current, op, actor, now) {
            Err(refused) => return Ok(Err(refused)),
            Ok(Change::Unchanged(e)) => e,
            Ok(Change::Remove) => {
                tx.execute(
                    "UPDATE records SET data = NULL \
                     WHERE space = ? AND owner = ? AND collection = ? AND id = ?",
                    params![space.as_str(), owner, collection, id],
                )?;
                None
            }
            Ok(Change::Put { record: e, base }) => {
                tx.execute(
                    "INSERT OR REPLACE INTO records VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        space.as_str(),
                        owner,
                        collection,
                        id,
                        e.rev,
                        e.by,
                        e.at,
                        e.data.to_string(),
                        base
                    ],
                )?;
                // Measured after the write, so a replacement is charged only what it adds.
                if used(tx)? > self.cap {
                    return Ok(Err(Refusal::TooLarge(format!(
                        "this app's records are at their {} MiB cap",
                        self.cap >> 20
                    ))));
                }
                Some(e)
            }
        }))
    }

    /// Every `shared` record, and `subject`'s own `me` ones.
    pub fn visible(&self, slug: &str, subject: &str) -> anyhow::Result<Visible> {
        let db = self.db(slug)?;
        let conn = db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, rev, \"by\", at, data, space, collection FROM records \
             WHERE data IS NOT NULL \
             AND ((space = 'shared' AND owner = '') OR (space = 'me' AND owner = ?)) \
             ORDER BY collection, id",
        )?;
        let rows = stmt.query_map([subject], |r| {
            Ok((envelope(r)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?))
        })?;
        let (mut shared, mut me) = Visible::default();
        for row in rows {
            let (e, space, collection) = row?;
            let into = if space == "me" { &mut me } else { &mut shared };
            into.entry(collection).or_insert_with(Vec::new).push(e);
        }
        Ok((shared, me))
    }
}

/// What an id holds.
fn read(
    conn: &Connection,
    space: Space,
    owner: &str,
    collection: &str,
    id: &str,
) -> rusqlite::Result<Current> {
    let row = conn
        .query_row(
            "SELECT id, rev, \"by\", at, data, base FROM records \
             WHERE space = ? AND owner = ? AND collection = ? AND id = ?",
            params![space.as_str(), owner, collection, id],
            |r| match r.get::<_, Option<String>>(4)? {
                Some(_) => Ok(Current::Live {
                    record: envelope(r)?,
                    base: r.get(5)?,
                }),
                None => Ok(Current::Gone(r.get(1)?)),
            },
        )
        .optional()?;
    Ok(row.unwrap_or(Current::Gone(0)))
}

/// Columns `id, rev, by, at, data` first; `data` not NULL.
fn envelope(r: &rusqlite::Row) -> rusqlite::Result<Envelope> {
    let data: String = r.get(4)?;
    Ok(Envelope {
        id: r.get(0)?,
        rev: r.get(1)?,
        by: r.get(2)?,
        at: r.get(3)?,
        data: serde_json::from_str(&data).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into())
        })?,
    })
}

/// Bytes of pages in use (free pages don't count: SQLite reuses them).
fn used(conn: &Connection) -> rusqlite::Result<u64> {
    let pragma = |name: &str| conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, u64>(0));
    Ok((pragma("page_count")? - pragma("freelist_count")?) * pragma("page_size")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp() -> (Records, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mis-records-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("pool")).unwrap();
        (Records::new(dir.clone()), dir)
    }

    fn put(space: Space, id: &str, rev: u64, data: serde_json::Value) -> Mutation {
        Mutation::Put {
            space,
            collection: "notes".into(),
            id: id.into(),
            rev,
            data,
        }
    }

    fn page(r: &Records, q: serde_json::Value) -> Page {
        let q: RecordQuery = serde_json::from_value(q).unwrap();
        let c = records::compile(Space::Shared, "", "notes", &q).unwrap();
        r.list("pool", &q, &c).unwrap()
    }

    fn ids(p: &Page) -> Vec<&str> {
        p.records.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn writes_read_back_and_persist() {
        let (r, dir) = temp();
        let e = r
            .mutate(
                "pool",
                &put(Space::Shared, "a", 0, json!({"t": 1})),
                "me@x",
                5,
            )
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!((e.rev, e.by.as_str(), e.at), (1, "me@x", 5));
        let stale = r.mutate(
            "pool",
            &put(Space::Shared, "a", 0, json!({"t": 2})),
            "me@x",
            6,
        );
        assert_eq!(stale.unwrap(), Err(Refusal::Conflict(Some(e.clone()))));
        // `me` rows are their writer's alone.
        r.mutate(
            "pool",
            &put(Space::Me, "a", 0, json!({"mine": 1})),
            "me@x",
            7,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            r.get("pool", Space::Me, "you@x", "notes", "a").unwrap(),
            None
        );
        assert!(r
            .get("pool", Space::Me, "me@x", "notes", "a")
            .unwrap()
            .is_some());
        let (shared, me) = r.visible("pool", "you@x").unwrap();
        assert_eq!((shared["notes"].len(), me.len()), (1, 0));
        let (_, me) = r.visible("pool", "me@x").unwrap();
        assert_eq!(me["notes"][0].data, json!({"mine": 1}));
        drop(r);
        // A new store (a restart) reads the same file.
        let again = Records::new(dir.clone());
        assert_eq!(
            again.get("pool", Space::Shared, "", "notes", "a").unwrap(),
            Some(e)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["data.sqlite", "data.sqlite-wal"] {
                let mode = std::fs::metadata(dir.join("pool").join(f))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{f}");
            }
        }
        drop(again); // Windows can't delete an open database
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queries_filter_order_and_page_through_every_row_once() {
        let (r, dir) = temp();
        // Scores with ties, a missing one, a string and a boolean mixed in.
        let rows = [
            ("a", json!({"score": 3, "done": true})),
            ("b", json!({"score": 1})),
            ("c", json!({"score": 3})),
            ("d", json!({"other": 1})),
            ("e", json!({"score": "3"})),
            ("f", json!({"score": 2.5, "done": false})),
            ("g", json!({"score": true})),
        ];
        for (id, data) in rows {
            r.mutate("pool", &put(Space::Shared, id, 0, data), "me", 1)
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            ids(&page(&r, json!({}))),
            ["a", "b", "c", "d", "e", "f", "g"]
        );
        // Typed: 3 matches numbers only, "3" strings only.
        assert_eq!(
            ids(&page(&r, json!({"where": {"score": {"eq": 3}}}))),
            ["a", "c"]
        );
        assert_eq!(
            ids(&page(&r, json!({"where": {"score": {"eq": "3"}}}))),
            ["e"]
        );
        assert_eq!(
            ids(&page(&r, json!({"where": {"score": {"gte": 2, "lt": 3}}}))),
            ["f"]
        );
        assert_eq!(
            ids(&page(&r, json!({"where": {"score": {"gt": 0}}}))),
            ["a", "b", "c", "f"]
        );
        assert_eq!(
            ids(&page(&r, json!({"where": {"done": {"eq": false}}}))),
            ["f"]
        );
        assert_eq!(
            ids(&page(&r, json!({"where": {"score": {"eq": true}}}))),
            ["g"]
        );
        // An injected value matches nothing and breaks nothing.
        let evil = json!({"where": {"score": {"eq": "3' OR '1'='1"}}});
        assert_eq!(ids(&page(&r, evil)), Vec::<&str>::new());
        // Every order, in pages of 2: each row once, in the same order as one big page.
        for dir in ["asc", "desc"] {
            let whole = ids(&page(&r, json!({"order_by": ["score", dir]})))
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>();
            let mut seen = vec![];
            let mut q = json!({"order_by": ["score", dir], "limit": 2});
            loop {
                let p = page(&r, q.clone());
                seen.extend(ids(&p).into_iter().map(String::from));
                match p.cursor {
                    Some(c) => q["cursor"] = c.into(),
                    None => break,
                }
            }
            assert_eq!(seen, whole, "{dir}");
            assert_eq!(seen.len(), 7);
        }
        let asc = ids(&page(&r, json!({"order_by": ["score", "asc"]})))
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        // Missing first, then numbers (true sorts as 1, tied with b by id), then strings.
        assert_eq!(asc, ["d", "b", "g", "f", "a", "c", "e"]);
        // And by id alone.
        let p = page(&r, json!({"limit": 4}));
        assert_eq!(ids(&p), ["a", "b", "c", "d"]);
        let p = page(&r, json!({"limit": 4, "cursor": p.cursor.unwrap()}));
        assert_eq!(ids(&p), ["e", "f", "g"]);
        assert_eq!(p.cursor, None);
        drop(r); // Windows can't delete an open database
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_stop_at_the_database_cap_but_same_size_updates_and_removes_go_through() {
        let (mut r, dir) = temp();
        r.cap = 256 << 10;
        let blob = |c: &str| json!(c.repeat(60 << 10));
        let mut stored = 0;
        let refused = loop {
            let op = put(Space::Shared, &stored.to_string(), 0, blob("x"));
            match r.mutate("pool", &op, "me", 1).unwrap() {
                Ok(_) => stored += 1,
                Err(refused) => break refused,
            }
        };
        assert!(matches!(refused, Refusal::TooLarge(_)), "{refused:?}");
        assert!((1..5).contains(&stored), "{stored}");
        // Nothing of the refused write was kept.
        let gone = r.get("pool", Space::Shared, "", "notes", &stored.to_string());
        assert_eq!(gone.unwrap(), None);
        // At the cap, replacing a record with one its size still fits.
        let op = put(Space::Shared, "0", 1, blob("y"));
        assert_eq!(
            r.mutate("pool", &op, "me", 2)
                .unwrap()
                .unwrap()
                .unwrap()
                .rev,
            2
        );
        let rm = Mutation::Remove {
            space: Space::Shared,
            collection: "notes".into(),
            id: "0".into(),
            rev: 2,
        };
        assert_eq!(r.mutate("pool", &rm, "me", 2).unwrap(), Ok(None));
        // The freed pages make room again.
        let op = put(Space::Shared, "again", 0, blob("x"));
        assert!(r.mutate("pool", &op, "me", 3).unwrap().is_ok());
        drop(r); // Windows can't delete an open database
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_batch_lands_whole_or_not_at_all() {
        let (mut r, dir) = temp();
        let get = |r: &Records, id: &str| r.get("pool", Space::Shared, "", "notes", id).unwrap();
        let a = r
            .mutate("pool", &put(Space::Shared, "a", 0, json!(1)), "me", 1)
            .unwrap()
            .unwrap()
            .unwrap();
        // A conflict mid-batch: the op before it isn't kept either; the index names it.
        let ops = [
            put(Space::Shared, "b", 0, json!(2)),
            put(Space::Shared, "a", 0, json!(9)),
            put(Space::Shared, "c", 0, json!(3)),
        ];
        assert_eq!(
            r.mutate_all("pool", &ops, "me", 2).unwrap(),
            Err((1, Refusal::Conflict(Some(a.clone()))))
        );
        assert_eq!((get(&r, "b"), get(&r, "c")), (None, None));
        // At the stored rev it lands, in order: each op's record back.
        let ops = [
            put(Space::Shared, "b", 0, json!(2)),
            put(Space::Shared, "a", 1, json!(9)),
            Mutation::Remove {
                space: Space::Shared,
                collection: "notes".into(),
                id: "gone".into(),
                rev: 4,
            },
        ];
        let done = r.mutate_all("pool", &ops, "me", 3).unwrap().unwrap();
        let revs: Vec<_> = done.iter().map(|e| e.as_ref().map(|e| e.rev)).collect();
        assert_eq!(revs, [Some(1), Some(2), None]);
        assert_eq!(get(&r, "a").unwrap().data, json!(9));
        // Sent again after its answer was lost: the same answer, nothing new.
        assert_eq!(r.mutate_all("pool", &ops, "me", 4).unwrap(), Ok(done));
        // Over the database cap mid-batch: refused at that op, nothing kept.
        r.cap = 256 << 10;
        let big = |id: &str| put(Space::Shared, id, 0, json!("x".repeat(60 << 10)));
        let ops: Vec<_> = (0..8).map(|i| big(&format!("big{i}"))).collect();
        let (at, why) = r.mutate_all("pool", &ops, "me", 5).unwrap().unwrap_err();
        assert!(
            matches!(why, Refusal::TooLarge(_)) && (1..8).contains(&at),
            "{at} {why:?}"
        );
        assert_eq!(get(&r, "big0"), None);
        assert_eq!(page(&r, json!({})).records.len(), 2);
        drop(r); // Windows can't delete an open database
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removed_records_vanish_but_their_revisions_stay() {
        let (r, dir) = temp();
        let w = |op: &Mutation| r.mutate("pool", op, "me", 1).unwrap();
        w(&put(Space::Shared, "a", 0, json!(1))).unwrap();
        let rm = |rev| Mutation::Remove {
            space: Space::Shared,
            collection: "notes".into(),
            id: "a".into(),
            rev,
        };
        assert_eq!(w(&rm(1)), Ok(None));
        assert_eq!(
            r.get("pool", Space::Shared, "", "notes", "a").unwrap(),
            None
        );
        assert_eq!(page(&r, json!({})).records, []);
        assert!(r.visible("pool", "me").unwrap().0.is_empty());
        // Created again: rev 2, so the first remove, delivered late, conflicts.
        let again = w(&put(Space::Shared, "a", 0, json!(2))).unwrap().unwrap();
        assert_eq!(again.rev, 2);
        assert_eq!(w(&rm(1)), Err(Refusal::Conflict(Some(again.clone()))));
        assert_eq!(page(&r, json!({})).records, [again]);
        drop(r); // Windows can't delete an open database
        std::fs::remove_dir_all(dir).unwrap();
    }
}

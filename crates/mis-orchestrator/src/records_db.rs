//! Each app's records in `<state>/apps/<slug>/data.sqlite` (WAL, `0600`): one connection per
//! app behind a mutex, so its writes are serialized. Blocking; call off the async workers.
//! What a write does and what a query means: [`crate::records`].

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mis_types::{Envelope, Mutation, Page, RecordQuery, Space};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::records::{self, Change, Compiled, Refusal};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS records (
    space TEXT NOT NULL,
    owner TEXT NOT NULL,
    collection TEXT NOT NULL,
    id TEXT NOT NULL,
    rev INTEGER NOT NULL,
    \"by\" TEXT NOT NULL,
    at INTEGER NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (space, owner, collection, id)
) WITHOUT ROWID";

/// The apps folder's databases, opened on first use.
pub struct Records {
    dir: PathBuf,
    open: Mutex<HashMap<String, Arc<Mutex<Connection>>>>,
    /// [`records::MAX_DB`]; smaller in tests.
    cap: u64,
}

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

    /// `slug`'s database (a [`crate::apps::valid_slug`] whose folder exists).
    fn db(&self, slug: &str) -> anyhow::Result<Arc<Mutex<Connection>>> {
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
        Ok(read(&conn, space, owner, collection, id)?)
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
        let (Mutation::Put {
            space,
            collection,
            id,
            ..
        }
        | Mutation::Remove {
            space,
            collection,
            id,
            ..
        }) = op;
        let owner = records::owner(*space, actor);
        let db = self.db(slug)?;
        let mut conn = db.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read(&tx, *space, owner, collection, id)?;
        let reply = match records::apply(current.as_ref(), op, actor, now) {
            Err(refused) => return Ok(Err(refused)),
            Ok(Change::Unchanged(e)) => return Ok(Ok(e)),
            Ok(Change::Remove) => {
                tx.execute(
                    "DELETE FROM records WHERE space = ? AND owner = ? AND collection = ? AND id = ?",
                    params![space.as_str(), owner, collection, id],
                )?;
                None
            }
            Ok(Change::Put(e)) => {
                let data = e.data.to_string();
                if !records::fits(used(&tx)?, data.len(), self.cap) {
                    return Ok(Err(Refusal::TooLarge(format!(
                        "this app's records are at their {} MiB cap",
                        self.cap >> 20
                    ))));
                }
                tx.execute(
                    "INSERT OR REPLACE INTO records VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        space.as_str(),
                        owner,
                        collection,
                        id,
                        e.rev,
                        e.by,
                        e.at,
                        data
                    ],
                )?;
                Some(e)
            }
        };
        tx.commit()?;
        Ok(Ok(reply))
    }

    /// Every `shared` record, and `subject`'s own `me` ones.
    pub fn visible(&self, slug: &str, subject: &str) -> anyhow::Result<Visible> {
        let db = self.db(slug)?;
        let conn = db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, rev, \"by\", at, data, space, collection FROM records \
             WHERE (space = 'shared' AND owner = '') OR (space = 'me' AND owner = ?) \
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

fn read(
    conn: &Connection,
    space: Space,
    owner: &str,
    collection: &str,
    id: &str,
) -> rusqlite::Result<Option<Envelope>> {
    conn.query_row(
        "SELECT id, rev, \"by\", at, data FROM records \
         WHERE space = ? AND owner = ? AND collection = ? AND id = ?",
        params![space.as_str(), owner, collection, id],
        envelope,
    )
    .optional()
}

/// Columns `id, rev, by, at, data` first.
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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_stop_at_the_database_cap_but_removes_go_through() {
        let (mut r, dir) = temp();
        r.cap = 256 << 10;
        let blob = json!("x".repeat(60 << 10));
        let mut stored = 0;
        let refused = loop {
            let op = put(Space::Shared, &stored.to_string(), 0, blob.clone());
            match r.mutate("pool", &op, "me", 1).unwrap() {
                Ok(_) => stored += 1,
                Err(refused) => break refused,
            }
        };
        assert!(matches!(refused, Refusal::TooLarge(_)), "{refused:?}");
        assert!((1..5).contains(&stored), "{stored}");
        let rm = Mutation::Remove {
            space: Space::Shared,
            collection: "notes".into(),
            id: "0".into(),
            rev: 1,
        };
        assert_eq!(r.mutate("pool", &rm, "me", 2).unwrap(), Ok(None));
        // The freed pages make room again.
        let op = put(Space::Shared, "again", 0, blob);
        assert!(r.mutate("pool", &op, "me", 3).unwrap().is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

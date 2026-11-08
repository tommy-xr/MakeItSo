//! An app's records (pure): which writes apply ([`apply`]), what a query compiles to
//! ([`compile`]), and the caps. [`crate::records_db`] is the SQLite shell around it.

use base64ct::{Base64UrlUnpadded, Encoding};
use mis_types::{Direction, Envelope, Mutation, RecordQuery, Space};
use rusqlite::types::Value as Sql;
use serde_json::Value;

/// Largest record `data`, as JSON text.
pub const MAX_DATA: usize = 64 << 10;
/// Largest app database: writes stop growing it past this.
pub const MAX_DB: u64 = 100 << 20;
/// Most records a page holds, and the default.
pub const MAX_PAGE: u32 = 100;

/// 1–128 of `[A-Za-z0-9._:-]`; not `.` or `..`, which a URL path can't carry.
pub fn valid_id(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_alphanumeric() || "._:-".contains(c);
    (1..=128).contains(&s.len()) && s.chars().all(ok) && s != "." && s != ".."
}

/// `[a-z0-9_-]{1,64}`.
pub fn valid_collection(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
    (1..=64).contains(&s.len()) && s.chars().all(ok)
}

/// `[A-Za-z0-9_]{1,64}`: a top-level key of `data`.
pub fn valid_field(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whose rows a request in `space` reaches: `me` is keyed by the asker, `shared` by no one.
pub fn owner(space: Space, subject: &str) -> &str {
    match space {
        Space::Shared => "",
        Space::Me => subject,
    }
}

/// What a mutation does to the stored record.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// Store this (a create or an update).
    Put(Envelope),
    Remove,
    /// Nothing to write; reply with this. A retry of a put that already landed, or a
    /// remove of a missing record.
    Unchanged(Option<Envelope>),
}

#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// 409: the stored revision isn't the one sent. What is stored now.
    Conflict(Option<Envelope>),
    /// 413.
    TooLarge(String),
    /// 400.
    Invalid(String),
}

/// `op` against the `current` record, by `actor` at Unix millis `now`.
pub fn apply(
    current: Option<&Envelope>,
    op: &Mutation,
    actor: &str,
    now: u64,
) -> Result<Change, Refusal> {
    let (Mutation::Put { collection, id, .. } | Mutation::Remove { collection, id, .. }) = op;
    check_key(collection, id)?;
    match op {
        Mutation::Remove { rev, .. } => match current {
            None => Ok(Change::Unchanged(None)),
            Some(c) if c.rev == *rev => Ok(Change::Remove),
            Some(c) => Err(Refusal::Conflict(Some(c.clone()))),
        },
        Mutation::Put { id, rev, data, .. } => {
            let len = data.to_string().len();
            if len > MAX_DATA {
                return Err(Refusal::TooLarge(format!(
                    "data is {len} bytes; the most is {MAX_DATA}"
                )));
            }
            let put = |rev| {
                Ok(Change::Put(Envelope {
                    id: id.clone(),
                    rev,
                    by: actor.into(),
                    at: now,
                    data: data.clone(),
                }))
            };
            match current {
                None if *rev == 0 => put(1),
                None => Err(Refusal::Conflict(None)),
                Some(c) if c.rev == *rev => put(rev + 1),
                // The response to this very write was lost: it landed, so it succeeded.
                Some(c)
                    if Some(c.rev) == rev.checked_add(1) && c.by == actor && c.data == *data =>
                {
                    Ok(Change::Unchanged(Some(c.clone())))
                }
                Some(c) => Err(Refusal::Conflict(Some(c.clone()))),
            }
        }
    }
}

fn check_key(collection: &str, id: &str) -> Result<(), Refusal> {
    if !valid_collection(collection) {
        return Err(Refusal::Invalid(format!(
            "collection {collection:?}: expected [a-z0-9_-]{{1,64}}"
        )));
    }
    if !valid_id(id) {
        return Err(Refusal::Invalid(format!(
            "id {id:?}: expected 1-128 of [A-Za-z0-9._:-]"
        )));
    }
    Ok(())
}

/// Room for `data_len` more bytes in an app database using `used` of `cap` ([`MAX_DB`]).
pub fn fits(used: u64, data_len: usize, cap: u64) -> bool {
    used.saturating_add(data_len as u64) <= cap
}

/// A list query as SQL over `records`, its parameters in order. Rows come back as
/// `id, rev, by, at, data, k` (k: the order value), at most `limit + 1` of them: one more
/// than a page says whether there is a next.
#[derive(Debug, PartialEq)]
pub struct Compiled {
    pub sql: String,
    pub params: Vec<Sql>,
    pub limit: u32,
}

/// Where a page ended, so the next starts after it: the order it was read in, and the last
/// row's order value and id. Base64url JSON, opaque to clients.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Cursor {
    order: Option<(String, Direction)>,
    k: Value,
    id: String,
}

/// `q` over `(space, owner, collection)`. Values and field paths are bound parameters;
/// the SQL text holds only fixed fragments. Err: why `q` is refused (400).
pub fn compile(
    space: Space,
    owner: &str,
    collection: &str,
    q: &RecordQuery,
) -> Result<Compiled, String> {
    if !valid_collection(collection) {
        return Err(format!(
            "collection {collection:?}: expected [a-z0-9_-]{{1,64}}"
        ));
    }
    let limit = q.limit.unwrap_or(MAX_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(format!("limit must be 1-{MAX_PAGE}"));
    }
    let mut sql = String::from("SELECT id, rev, \"by\", at, data, ");
    let mut params = vec![];
    // The order value: the field's, else nothing (ids alone order the rows).
    let order = match &q.order_by {
        Some((field, dir)) => Some((path(field)?, *dir)),
        None => None,
    };
    match &order {
        Some((p, _)) => {
            sql.push_str("json_extract(data, ?)");
            params.push(p.clone());
        }
        None => sql.push_str("NULL"),
    }
    sql.push_str(" FROM records WHERE space = ? AND owner = ? AND collection = ?");
    params.extend([
        Sql::Text(space.as_str().into()),
        Sql::Text(owner.into()),
        Sql::Text(collection.into()),
    ]);
    for (field, cond) in &q.where_ {
        let p = path(field)?;
        let bounds = [
            ("=", &cond.eq),
            (">", &cond.gt),
            (">=", &cond.gte),
            ("<", &cond.lt),
            ("<=", &cond.lte),
        ];
        let mut any = false;
        for (op, v) in bounds {
            let Some(v) = v else { continue };
            any = true;
            let (types, v) = scalar(field, v)?;
            sql.push_str(&format!(
                " AND json_type(data, ?) IN ({types}) AND json_extract(data, ?) {op} ?"
            ));
            params.extend([p.clone(), p.clone(), v]);
        }
        if !any {
            return Err(format!("where.{field}: give eq, gt, gte, lt or lte"));
        }
    }
    let after = match &q.cursor {
        Some(c) => Some(decode(c, &q.order_by)?),
        None => None,
    };
    if let Some(c) = after {
        let id = Sql::Text(c.id);
        match &order {
            None => {
                sql.push_str(" AND id > ?");
                params.push(id);
            }
            Some((p, dir)) => after_key(&mut sql, &mut params, p, *dir, value_sql(&c.k)?, id),
        }
    }
    let dir = |d: Direction| match d {
        Direction::Asc => "ASC",
        Direction::Desc => "DESC",
    };
    match &order {
        None => sql.push_str(" ORDER BY id ASC"),
        Some((p, d)) => {
            sql.push_str(&format!(
                " ORDER BY json_extract(data, ?) {0}, id {0}",
                dir(*d)
            ));
            params.push(p.clone());
        }
    }
    sql.push_str(" LIMIT ?");
    params.push(Sql::Integer(i64::from(limit) + 1));
    Ok(Compiled { sql, params, limit })
}

/// Rows after `(k, id)` in `dir` order. SQLite sorts NULL first ascending (last
/// descending), and `NULL > x` is never true, so a NULL order value is handled apart.
fn after_key(sql: &mut String, params: &mut Vec<Sql>, path: &Sql, dir: Direction, k: Sql, id: Sql) {
    let key = "json_extract(data, ?)";
    let (op, nulls_after) = match dir {
        Direction::Asc => (">", false),
        Direction::Desc => ("<", true),
    };
    if k == Sql::Null {
        // Among the NULLs, by id; ascending, every non-NULL comes after them too.
        let rest = if nulls_after {
            ""
        } else {
            &format!(" OR {key} IS NOT NULL")
        };
        sql.push_str(&format!(" AND (({key} IS NULL AND id {op} ?){rest})"));
        params.extend([path.clone(), id]);
        if !nulls_after {
            params.push(path.clone());
        }
    } else {
        let nulls = if nulls_after {
            &format!(" OR {key} IS NULL")
        } else {
            ""
        };
        sql.push_str(&format!(
            " AND ({key} {op} ? OR ({key} = ? AND id {op} ?){nulls})"
        ));
        params.extend([path.clone(), k.clone(), path.clone(), k, id]);
        if nulls_after {
            params.push(path.clone());
        }
    }
}

/// `score` -> `$.score`, bound as a parameter.
fn path(field: &str) -> Result<Sql, String> {
    if !valid_field(field) {
        return Err(format!("field {field:?}: expected [A-Za-z0-9_]{{1,64}}"));
    }
    Ok(Sql::Text(format!("$.{field}")))
}

/// A condition's value and the `json_type`s it compares with: a filter on a number never
/// matches a string (SQLite would order every string after every number).
fn scalar(field: &str, v: &Value) -> Result<(&'static str, Sql), String> {
    Ok(match v {
        Value::String(s) => ("'text'", Sql::Text(s.clone())),
        Value::Number(_) => ("'integer', 'real'", value_sql(v)?),
        // json_extract gives booleans as 1 and 0.
        Value::Bool(b) => ("'true', 'false'", Sql::Integer(i64::from(*b))),
        _ => {
            return Err(format!(
                "where.{field}: values are strings, numbers or booleans"
            ))
        }
    })
}

/// A cursor's order value back as SQL.
fn value_sql(v: &Value) -> Result<Sql, String> {
    Ok(match v {
        Value::Null => Sql::Null,
        Value::String(s) => Sql::Text(s.clone()),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Sql::Integer(i),
            (None, Some(f)) => Sql::Real(f),
            _ => return Err("number out of range".into()),
        },
        _ => return Err("bad cursor".into()),
    })
}

/// A row's order value (column `k`) as JSON, for its cursor.
pub fn sql_value(v: Sql) -> Value {
    match v {
        Sql::Integer(i) => i.into(),
        Sql::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        Sql::Text(s) => s.into(),
        Sql::Null | Sql::Blob(_) => Value::Null,
    }
}

/// The cursor after a page whose last row has order value `k` and `id`, read by `q`.
pub fn cursor(q: &RecordQuery, k: Value, id: &str) -> String {
    let c = Cursor {
        order: q.order_by.clone(),
        k,
        id: id.into(),
    };
    Base64UrlUnpadded::encode_string(serde_json::to_string(&c).expect("serializes").as_bytes())
}

fn decode(raw: &str, order: &Option<(String, Direction)>) -> Result<Cursor, String> {
    let bytes = Base64UrlUnpadded::decode_vec(raw).map_err(|_| "bad cursor")?;
    let c: Cursor = serde_json::from_slice(&bytes).map_err(|_| "bad cursor")?;
    if c.order != *order {
        return Err("cursor is from a query with another order_by".into());
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(rev: u64, by: &str, data: Value) -> Envelope {
        Envelope {
            id: "a".into(),
            rev,
            by: by.into(),
            at: 9,
            data,
        }
    }

    fn put(rev: u64, data: Value) -> Mutation {
        Mutation::Put {
            space: Space::Shared,
            collection: "notes".into(),
            id: "a".into(),
            rev,
            data,
        }
    }

    fn remove(rev: u64) -> Mutation {
        Mutation::Remove {
            space: Space::Shared,
            collection: "notes".into(),
            id: "a".into(),
            rev,
        }
    }

    #[test]
    fn create_update_and_stale_writes() {
        let d = json!({"t": 1});
        // Create: rev 0 on nothing.
        assert_eq!(
            apply(None, &put(0, d.clone()), "me", 9),
            Ok(Change::Put(env(1, "me", d.clone())))
        );
        // A rev on nothing: it was removed (or never was).
        assert_eq!(
            apply(None, &put(3, d.clone()), "me", 9),
            Err(Refusal::Conflict(None))
        );
        // Update at the stored rev: next rev, new by and at.
        let cur = env(4, "other", json!({"t": 0}));
        assert_eq!(
            apply(Some(&cur), &put(4, d.clone()), "me", 9),
            Ok(Change::Put(env(5, "me", d.clone())))
        );
        // Stale: 409 with what is there.
        for rev in [0, 2, 3, 5, 6] {
            assert_eq!(
                apply(Some(&cur), &put(rev, d.clone()), "me", 9),
                Err(Refusal::Conflict(Some(cur.clone()))),
                "{rev}"
            );
        }
    }

    #[test]
    fn create_only_conflicts_unless_it_is_the_same_create_retried() {
        let d = json!({"t": 1});
        let mine = env(1, "me", d.clone());
        // My own create, retried after a lost response.
        assert_eq!(
            apply(Some(&mine), &put(0, d.clone()), "me", 9),
            Ok(Change::Unchanged(Some(mine.clone())))
        );
        // Someone else's record, or other data, or already moved on: a conflict.
        for cur in [
            env(1, "other", d.clone()),
            env(1, "me", json!({"t": 2})),
            env(2, "me", d.clone()),
        ] {
            assert_eq!(
                apply(Some(&cur), &put(0, d.clone()), "me", 9),
                Err(Refusal::Conflict(Some(cur.clone())))
            );
        }
    }

    #[test]
    fn an_update_retried_after_it_landed_succeeds() {
        let d = json!({"t": 1, "n": [1, 2]});
        let landed = env(5, "me", d.clone());
        assert_eq!(
            apply(
                Some(&landed),
                &put(4, json!({"n": [1, 2], "t": 1})),
                "me",
                9
            ),
            Ok(Change::Unchanged(Some(landed.clone())))
        );
        assert!(apply(Some(&landed), &put(4, json!({"t": 2})), "me", 9).is_err());
        assert!(apply(Some(&landed), &put(4, d.clone()), "you", 9).is_err());
        assert!(apply(Some(&landed), &put(3, d.clone()), "me", 9).is_err());
        // No overflow at the top.
        let top = env(u64::MAX, "me", d.clone());
        assert!(apply(Some(&top), &put(u64::MAX - 1, d.clone()), "me", 9).is_ok());
        assert!(apply(Some(&env(0, "me", d.clone())), &put(u64::MAX, d), "me", 9).is_err());
    }

    #[test]
    fn removes() {
        let cur = env(2, "x", json!(1));
        assert_eq!(apply(Some(&cur), &remove(2), "me", 9), Ok(Change::Remove));
        assert_eq!(
            apply(Some(&cur), &remove(1), "me", 9),
            Err(Refusal::Conflict(Some(cur.clone())))
        );
        // Missing (already removed, or never there): done.
        assert_eq!(
            apply(None, &remove(7), "me", 9),
            Ok(Change::Unchanged(None))
        );
    }

    #[test]
    fn caps_and_keys() {
        let big = json!({"s": "x".repeat(MAX_DATA)});
        assert!(matches!(
            apply(None, &put(0, big), "me", 9),
            Err(Refusal::TooLarge(_))
        ));
        // Exactly at the cap is fine: `"…"` is the string plus its quotes.
        let edge = json!("x".repeat(MAX_DATA - 2));
        assert!(apply(None, &put(0, edge), "me", 9).is_ok());
        let keyed = |collection: &str, id: &str| Mutation::Remove {
            space: Space::Me,
            collection: collection.into(),
            id: id.into(),
            rev: 0,
        };
        assert!(matches!(
            apply(None, &keyed("Notes", "a"), "me", 9),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            apply(None, &keyed("notes", "a/b"), "me", 9),
            Err(Refusal::Invalid(_))
        ));
        assert!(fits(MAX_DB - 10, 10, MAX_DB) && !fits(MAX_DB - 10, 11, MAX_DB));
        assert!(!fits(u64::MAX, 1, MAX_DB));
    }

    #[test]
    fn ids_collections_and_fields() {
        for ok in ["a", "A-1", "2026-11-08", "x.y:z_w", "...", &"a".repeat(128)] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a b",
            "é",
            "a%2F",
            "a?b",
            &"a".repeat(129),
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
        for ok in ["notes", "a_b-2", &"a".repeat(64)] {
            assert!(valid_collection(ok), "{ok}");
        }
        for bad in ["", "Notes", "a.b", "a b", "a/b", &"a".repeat(65)] {
            assert!(!valid_collection(bad), "{bad}");
        }
        for ok in ["score", "Due_At", "x1"] {
            assert!(valid_field(ok), "{ok}");
        }
        for bad in ["", "a.b", "a-b", "a'b", "$.x", "a[0]", &"a".repeat(65)] {
            assert!(!valid_field(bad), "{bad}");
        }
        assert_eq!(owner(Space::Me, "me@x"), "me@x");
        assert_eq!(owner(Space::Shared, "me@x"), "");
    }

    fn q(v: Value) -> RecordQuery {
        serde_json::from_value(v).unwrap()
    }

    fn compiled(v: Value) -> Result<Compiled, String> {
        compile(Space::Shared, "", "notes", &q(v))
    }

    #[test]
    fn a_plain_list_orders_by_id() {
        let c = compiled(json!({})).unwrap();
        assert_eq!(
            c.sql,
            "SELECT id, rev, \"by\", at, data, NULL FROM records \
             WHERE space = ? AND owner = ? AND collection = ? ORDER BY id ASC LIMIT ?"
        );
        let text = |s: &str| Sql::Text(s.into());
        assert_eq!(
            c.params,
            [text("shared"), text(""), text("notes"), Sql::Integer(101)]
        );
        assert_eq!(c.limit, 100);
    }

    #[test]
    fn filters_and_order_bind_every_value() {
        let c = compiled(json!({
            "where": {"done": {"eq": false}, "score": {"gte": 1, "lt": 2.5}},
            "order_by": ["score", "desc"],
            "limit": 10,
        }))
        .unwrap();
        assert_eq!(c.sql.matches('?').count(), c.params.len());
        assert!(c
            .sql
            .contains("json_type(data, ?) IN ('true', 'false') AND json_extract(data, ?) = ?"));
        assert!(c
            .sql
            .contains("IN ('integer', 'real') AND json_extract(data, ?) >= ?"));
        assert!(c
            .sql
            .ends_with("ORDER BY json_extract(data, ?) DESC, id DESC LIMIT ?"));
        assert!(c.params.contains(&Sql::Real(2.5)));
        assert!(c.params.contains(&Sql::Integer(0)));
        assert_eq!(c.params.last(), Some(&Sql::Integer(11)));
    }

    #[test]
    fn injection_attempts_are_refused_or_stay_parameters() {
        // Field names can't carry SQL or JSON-path syntax.
        for field in [
            "a') OR 1=1 --",
            "a.b",
            "a\"",
            "$",
            "a[0]",
            "x;DROP TABLE records",
        ] {
            let e = compiled(json!({"where": {field: {"eq": 1}}})).unwrap_err();
            assert!(e.contains("field"), "{e}");
            let e = compiled(json!({"order_by": [field, "asc"]})).unwrap_err();
            assert!(e.contains("field"), "{e}");
        }
        // A value is never in the SQL text.
        let evil = "x' OR '1'='1";
        let c = compiled(json!({"where": {"t": {"eq": evil}}})).unwrap();
        assert!(!c.sql.contains(evil));
        assert!(c.params.contains(&Sql::Text(evil.into())));
        // Neither is a cursor's.
        let cur = cursor(&q(json!({})), Value::Null, evil);
        let c = compiled(json!({ "cursor": cur })).unwrap();
        assert!(!c.sql.contains(evil) && c.params.contains(&Sql::Text(evil.into())));
        // Directions are an enum.
        assert!(
            serde_json::from_value::<RecordQuery>(json!({"order_by": ["a", "asc; --"]})).is_err()
        );
        assert!(compile(Space::Shared, "", "No'pe", &RecordQuery::default()).is_err());
    }

    #[test]
    fn refuses_bad_queries() {
        for (v, why) in [
            (json!({"limit": 0}), "limit"),
            (json!({"limit": 101}), "limit"),
            (json!({"where": {"a": {}}}), "give eq"),
            // null is the same as leaving the bound out.
            (json!({"where": {"a": {"eq": null}}}), "give eq"),
            (json!({"where": {"a": {"eq": [1]}}}), "values are"),
            (json!({"where": {"a": {"gt": {"x": 1}}}}), "values are"),
            (json!({"cursor": "%%%"}), "bad cursor"),
            (json!({"cursor": "e30"}), "bad cursor"), // `{}`
        ] {
            let e = compiled(v.clone()).unwrap_err();
            assert!(e.contains(why), "{v}: {e}");
        }
        // Unknown keys and operators: the query doesn't parse.
        for v in [
            json!({"where": {"a": {"like": "x"}}}),
            json!({"filter": {}}),
            json!({"order_by": ["a"]}),
        ] {
            assert!(
                serde_json::from_value::<RecordQuery>(v.clone()).is_err(),
                "{v}"
            );
        }
        // A cursor only continues the order it came from.
        let by_score = q(json!({"order_by": ["score", "asc"]}));
        let cur = cursor(&by_score, json!(3), "a");
        assert!(compiled(json!({"order_by": ["score", "desc"], "cursor": cur})).is_err());
        assert!(compiled(json!({ "cursor": cur })).is_err());
        assert!(compiled(json!({"order_by": ["score", "asc"], "cursor": cur})).is_ok());
    }

    #[test]
    fn cursors_resume_after_the_last_row() {
        let at = |k: Value, dir: &str| {
            let order = json!({"order_by": ["score", dir]});
            let cur = cursor(&q(order.clone()), k, "b");
            let mut v = order;
            v["cursor"] = cur.into();
            compiled(v).unwrap()
        };
        let c = at(json!(3), "asc");
        assert!(c
            .sql
            .contains("AND (json_extract(data, ?) > ? OR (json_extract(data, ?) = ? AND id > ?))"));
        assert_eq!(c.sql.matches('?').count(), c.params.len());
        let c = at(json!(3), "desc");
        assert!(c
            .sql
            .contains("= ? AND id < ?) OR json_extract(data, ?) IS NULL)"));
        assert_eq!(c.sql.matches('?').count(), c.params.len());
        let c = at(Value::Null, "asc");
        assert!(c
            .sql
            .contains("IS NULL AND id > ?) OR json_extract(data, ?) IS NOT NULL)"));
        assert_eq!(c.sql.matches('?').count(), c.params.len());
        let c = at(Value::Null, "desc");
        assert!(c
            .sql
            .contains("AND ((json_extract(data, ?) IS NULL AND id < ?))"));
        assert_eq!(c.sql.matches('?').count(), c.params.len());
        assert_eq!(sql_value(Sql::Real(1.5)), json!(1.5));
        assert_eq!(sql_value(Sql::Text("x".into())), json!("x"));
        assert_eq!(value_sql(&json!(2)), Ok(Sql::Integer(2)));
    }
}

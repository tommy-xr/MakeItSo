//! An app's records on the apps listener (`/a/<slug>/api/v1/…`): JSON documents by
//! space, collection and client-chosen id, each written only at the revision it was read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One record as stored and returned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    /// 1–128 of `[A-Za-z0-9._:-]`, chosen by the client.
    pub id: String,
    /// 1 for a new id, +1 each write (a removed id's continues when it is created again).
    pub rev: u64,
    /// Who wrote it last: their email, or `local`.
    pub by: String,
    /// Unix millis of the last write.
    pub at: u64,
    pub data: Value,
}

/// `shared`: every member's. `me`: only its writer's; no other member sees it, owners neither.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Space {
    Shared,
    Me,
}

impl Space {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Me => "me",
        }
    }
}

/// `POST /a/{slug}/api/v1/mutations`. `rev` is the revision the client last read (0: the
/// record must not exist yet); any other stored revision is a conflict (409).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
pub enum Mutation {
    Put {
        space: Space,
        /// `[a-z0-9_-]{1,64}`.
        collection: String,
        id: String,
        rev: u64,
        data: Value,
    },
    Remove {
        space: Space,
        collection: String,
        id: String,
        rev: u64,
    },
}

/// Most ops in a [`Batch`].
pub const MAX_BATCH: usize = 100;

/// `POST /a/{slug}/api/v1/mutations` with several writes: applied in order in one
/// transaction, all or none. Each id at most once, so a whole batch can be retried.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    /// At most [`MAX_BATCH`].
    pub ops: Vec<Mutation>,
}

impl Mutation {
    /// Which record it writes: `(space, collection, id)`.
    pub fn key(&self) -> (Space, &str, &str) {
        let (Self::Put {
            space,
            collection,
            id,
            ..
        }
        | Self::Remove {
            space,
            collection,
            id,
            ..
        }) = self;
        (*space, collection, id)
    }
}

/// 200 to a mutation: the stored record; None after a remove.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Written {
    pub record: Option<Envelope>,
}

/// 200 to a [`Batch`]: each op's [`Written::record`], in order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchWritten {
    pub records: Vec<Option<Envelope>>,
}

/// 409 to a mutation: what is stored now (None: nothing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conflict {
    pub current: Option<Envelope>,
    /// In a [`Batch`]: which op conflicted (nothing was written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

/// `?q=<json>` on `GET /a/{slug}/api/v1/records/{space}/{collection}`. Fields are top-level
/// keys of `data`, `[A-Za-z0-9_]{1,64}`; a condition matches only values of its own JSON type.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecordQuery {
    /// Field -> condition; all must hold.
    #[serde(default, rename = "where", skip_serializing_if = "BTreeMap::is_empty")]
    pub where_: BTreeMap<String, Condition>,
    /// Field and direction; ties (and no order) go by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<(String, Direction)>,
    /// 1..=100; default 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// From the previous [`Page`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// At least one bound; several make a range (`{"gte": 1, "lt": 5}`). Values are strings,
/// numbers or booleans; `null` is the same as leaving the bound out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Asc,
    Desc,
}

/// A list's answer. `cursor`: pass it back for the next page; None: this was the last.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Page {
    pub records: Vec<Envelope>,
    pub cursor: Option<String>,
}

/// `GET /a/{slug}/api/v1/export`: every record the caller can see, by collection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Export {
    /// 1.
    pub version: u32,
    pub app: String,
    /// Whose `me` records these are.
    pub user: String,
    /// Unix millis.
    pub at: u64,
    pub shared: BTreeMap<String, Vec<Envelope>>,
    pub me: BTreeMap<String, Vec<Envelope>>,
}

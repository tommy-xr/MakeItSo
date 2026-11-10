//! Boards: references (files, links, notes) collected for what an agent makes, kept so it can
//! be made from them and later refined with more. An app's now; the owner kind is generic.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Reference;

/// Most items one board keeps.
pub const MAX_BOARD_ITEMS: usize = 100;
/// Most file bytes one board keeps.
pub const MAX_BOARD_BYTES: u64 = 200 << 20;

/// What a board item is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoardRef {
    /// A file kept with the board: an image or a document.
    File { name: String, bytes: u64 },
    /// A link or note, as a task takes it.
    #[serde(untagged)]
    Ref(Reference),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardItem {
    pub id: Uuid,
    /// Unix millis it was added.
    pub at: u64,
    /// Gone to a task already; else it is new since the last create or update.
    #[serde(default)]
    pub sent: bool,
    pub what: BoardRef,
}

/// `GET /api/boards/{kind}/{id}`. Oldest item first.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Board {
    #[serde(default)]
    pub items: Vec<BoardItem>,
}

/// `POST /api/boards/{kind}/{id}/items`: an upload (`POST /api/upload` id) to keep, or a
/// link or note.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewBoardItem {
    Upload {
        upload: Uuid,
    },
    #[serde(untagged)]
    Ref(Reference),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_tagged_by_kind_links_and_notes_as_a_task_takes_them() {
        let file = BoardRef::File {
            name: "Chores.pdf".into(),
            bytes: 9,
        };
        let note = BoardRef::Ref(Reference::Note { text: "hi".into() });
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            serde_json::json!({ "kind": "file", "name": "Chores.pdf", "bytes": 9 })
        );
        let json = serde_json::json!({ "kind": "note", "text": "hi" });
        assert_eq!(serde_json::to_value(&note).unwrap(), json);
        assert_eq!(serde_json::from_value::<BoardRef>(json).unwrap(), note);
        let up: NewBoardItem = serde_json::from_str(
            r#"{"kind":"upload","upload":"00000000-0000-0000-0000-000000000000"}"#,
        )
        .unwrap();
        assert_eq!(
            up,
            NewBoardItem::Upload {
                upload: Uuid::nil()
            }
        );
        let link: NewBoardItem =
            serde_json::from_str(r#"{"kind":"link","url":"https://x"}"#).unwrap();
        assert!(matches!(link, NewBoardItem::Ref(Reference::Link { .. })));
        assert!(serde_json::from_str::<NewBoardItem>(r#"{"kind":"file","name":"x"}"#).is_err());
    }
}

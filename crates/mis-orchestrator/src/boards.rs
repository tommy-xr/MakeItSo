//! Boards kept on disk: `<dir>/<kind>/<id>/board.json` and its files as `files/<item>.<ext>`.
//! The checks are pure; `Boards` is the filesystem shell around them.
//!
//! A new app's board is `app/_new` until Create sends it, then `app/_<thread>` (the task's)
//! until that thread creates its app, whose board it becomes (`app/<slug>`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mis_types::{
    check_references, valid_slug, Board, BoardItem, BoardRef, Reference, UploadKind, MAX_ATTACHED,
    MAX_ATTACHED_BYTES, MAX_BOARD_BYTES, MAX_BOARD_ITEMS, MAX_REFERENCES,
};
use uuid::Uuid;

use crate::images::private_dir;

/// Whose board: the next new app's, or app `slug`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    NewApp,
    App(String),
}

impl Owner {
    /// From `/api/boards/{kind}/{id}`: `app/_new`, or `app/<slug>`.
    pub fn parse(kind: &str, id: &str) -> Option<Self> {
        match (kind, id) {
            ("app", "_new") => Some(Self::NewApp),
            ("app", s) if valid_slug(s) => Some(Self::App(s.into())),
            _ => None,
        }
    }

    /// Its folder under the boards dir. Never `_<thread>`: a slug can't start with `_`.
    fn path(&self) -> String {
        match self {
            Self::NewApp => "app/_new".into(),
            Self::App(slug) => format!("app/{slug}"),
        }
    }
}

/// Err: why `board` has no room for `what`. What isn't sent yet must fit one task: Create or
/// Update sends it all.
pub fn room(board: &Board, what: &BoardRef) -> Result<(), String> {
    if board.items.len() >= MAX_BOARD_ITEMS {
        return Err(format!("a board keeps at most {MAX_BOARD_ITEMS} items"));
    }
    let new = || board.items.iter().filter(|i| !i.sent).map(|i| &i.what);
    let bytes = |w: &BoardRef| match w {
        BoardRef::File { bytes, .. } => *bytes,
        BoardRef::Ref(_) => 0,
    };
    match what {
        BoardRef::File { bytes: more, .. } => {
            let kept: u64 = board.items.iter().map(|i| bytes(&i.what)).sum();
            if kept + more > MAX_BOARD_BYTES {
                return Err(format!(
                    "a board keeps at most {} MB of files",
                    MAX_BOARD_BYTES >> 20
                ));
            }
            let files = new().filter(|w| matches!(w, BoardRef::File { .. }));
            if files.count() >= MAX_ATTACHED {
                return Err(format!("at most {MAX_ATTACHED} new files at a time"));
            }
            if new().map(bytes).sum::<u64>() + more > MAX_ATTACHED_BYTES as u64 {
                let mb = MAX_ATTACHED_BYTES >> 20;
                return Err(format!("at most {mb} MB of new files at a time"));
            }
            Ok(())
        }
        BoardRef::Ref(r) => {
            check_references(std::slice::from_ref(r))?;
            if new().filter(|w| matches!(w, BoardRef::Ref(_))).count() >= MAX_REFERENCES {
                return Err(format!(
                    "at most {MAX_REFERENCES} new links and notes at a time"
                ));
            }
            Ok(())
        }
    }
}

/// What `board` hasn't sent: its items' ids; its files' ids and names; its links and notes.
pub fn unsent(board: &Board) -> (Vec<Uuid>, Vec<(Uuid, String)>, Vec<Reference>) {
    let (mut ids, mut files, mut refs) = (vec![], vec![], vec![]);
    for i in board.items.iter().filter(|i| !i.sent) {
        ids.push(i.id);
        match &i.what {
            BoardRef::File { name, .. } => files.push((i.id, name.clone())),
            BoardRef::Ref(r) => refs.push(r.clone()),
        }
    }
    (ids, files, refs)
}

/// `board` with items `ids` sent.
pub fn mark_sent(mut board: Board, ids: &[Uuid]) -> Board {
    for i in board.items.iter_mut().filter(|i| ids.contains(&i.id)) {
        i.sent = true;
    }
    board
}

pub struct Boards {
    dir: PathBuf,
    /// One change at a time: each reads, changes and writes `board.json`.
    lock: Mutex<()>,
}

impl Boards {
    /// Use `dir`, creating it readable only by this user.
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        private_dir(&dir)?;
        Ok(Self {
            dir,
            lock: Mutex::new(()),
        })
    }

    fn folder(&self, path: &str) -> PathBuf {
        self.dir.join(path)
    }

    /// The board in `folder`; empty if it has none yet (or it can't be read).
    fn load(folder: &Path) -> Board {
        let read = std::fs::read(folder.join("board.json"));
        read.ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(folder: &Path, board: &Board) -> std::io::Result<()> {
        private_dir(folder).map_err(std::io::Error::other)?;
        let json = serde_json::to_vec_pretty(board).expect("a board serializes");
        crate::push::write_private(&folder.join("board.json"), &json)
    }

    fn file_path(folder: &Path, id: Uuid, name: &str) -> Option<PathBuf> {
        let kind = UploadKind::from_name(name)?;
        Some(folder.join("files").join(format!("{id}.{}", kind.ext())))
    }

    pub fn get(&self, owner: &Owner) -> Board {
        Self::load(&self.folder(&owner.path()))
    }

    /// Keep `what` on `owner`'s board (with `bytes`, a file's), added `at`. Err: why not.
    pub fn add(
        &self,
        owner: &Owner,
        what: BoardRef,
        bytes: Option<&[u8]>,
        at: u64,
    ) -> Result<BoardItem, String> {
        let _held = self.lock.lock().unwrap();
        let folder = self.folder(&owner.path());
        let mut board = Self::load(&folder);
        room(&board, &what)?;
        let item = BoardItem {
            id: Uuid::new_v4(),
            at,
            sent: false,
            what,
        };
        let failed = |e: std::io::Error| {
            tracing::warn!("keeping a board item: {e}");
            "couldn't keep it".to_string()
        };
        if let (BoardRef::File { name, .. }, Some(bytes)) = (&item.what, bytes) {
            let path = Self::file_path(&folder, item.id, name).ok_or("not a kind of file kept")?;
            private_dir(&folder.join("files")).map_err(|e| failed(std::io::Error::other(e)))?;
            crate::push::write_private(&path, bytes).map_err(failed)?;
        }
        board.items.push(item.clone());
        Self::save(&folder, &board).map_err(failed)?;
        Ok(item)
    }

    /// Drop item `id` from `owner`'s board, and its file; whether it was there.
    pub fn remove(&self, owner: &Owner, id: Uuid) -> bool {
        let _held = self.lock.lock().unwrap();
        let folder = self.folder(&owner.path());
        let mut board = Self::load(&folder);
        let Some(at) = board.items.iter().position(|i| i.id == id) else {
            return false;
        };
        let item = board.items.remove(at);
        if Self::save(&folder, &board).is_err() {
            return false;
        }
        if let BoardRef::File { name, .. } = &item.what {
            let _ = Self::file_path(&folder, id, name).map(std::fs::remove_file);
        }
        true
    }

    /// File item `id` on `owner`'s board: its kind, name and bytes.
    pub fn file(&self, owner: &Owner, id: Uuid) -> Option<(UploadKind, String, Vec<u8>)> {
        let folder = self.folder(&owner.path());
        let board = Self::load(&folder);
        let BoardRef::File { name, .. } = &board.items.iter().find(|i| i.id == id)?.what else {
            return None;
        };
        let kind = UploadKind::from_name(name)?;
        let bytes = std::fs::read(Self::file_path(&folder, id, name)?).ok()?;
        Some((kind, name.clone(), bytes))
    }

    /// Mark items `ids` of `owner`'s board sent. Then, if `thread` (a new app's task) is given,
    /// set the board aside for the app that thread creates (see [`Self::adopt`]).
    pub fn sent(&self, owner: &Owner, ids: &[Uuid], thread: Option<Uuid>) -> std::io::Result<()> {
        let _held = self.lock.lock().unwrap();
        let folder = self.folder(&owner.path());
        let board = mark_sent(Self::load(&folder), ids);
        if board.items.is_empty() {
            return Ok(()); // nothing to keep
        }
        Self::save(&folder, &board)?;
        match thread {
            Some(t) => std::fs::rename(&folder, self.folder(&format!("app/_{t}"))),
            None => Ok(()),
        }
    }

    /// Thread `thread` created app `slug`: the board it was sent from becomes the app's,
    /// unless the app has one already.
    pub fn adopt(&self, thread: Uuid, slug: &str) {
        let _held = self.lock.lock().unwrap();
        let (from, to) = (
            self.folder(&format!("app/_{thread}")),
            self.folder(&format!("app/{slug}")),
        );
        if from.is_dir() && !to.exists() {
            if let Err(e) = std::fs::rename(&from, &to) {
                tracing::warn!("giving {slug} its board: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(t: &str) -> BoardRef {
        BoardRef::Ref(Reference::Note { text: t.into() })
    }
    fn file(bytes: u64) -> BoardRef {
        BoardRef::File {
            name: "a.pdf".into(),
            bytes,
        }
    }
    fn board(items: Vec<(BoardRef, bool)>) -> Board {
        Board {
            items: (items.into_iter())
                .map(|(what, sent)| BoardItem {
                    id: Uuid::new_v4(),
                    at: 0,
                    sent,
                    what,
                })
                .collect(),
        }
    }

    #[test]
    fn owners_are_the_new_app_or_an_app_by_slug() {
        assert_eq!(Owner::parse("app", "_new"), Some(Owner::NewApp));
        assert_eq!(Owner::parse("app", "pool"), Some(Owner::App("pool".into())));
        for (k, id) in [
            ("app", "../x"),
            ("app", "_x"),
            ("app", ""),
            ("project", "x"),
        ] {
            assert_eq!(Owner::parse(k, id), None, "{k}/{id}");
        }
    }

    #[test]
    fn what_is_new_must_fit_one_task_and_the_board_its_caps() {
        let empty = board(vec![]);
        assert_eq!(room(&empty, &file(1)), Ok(()));
        assert!(room(&empty, &note(" ")).is_err(), "checked as a task's");
        let full = board(vec![(file(1), false); MAX_ATTACHED]);
        assert!(room(&full, &file(1)).unwrap_err().contains("new files"));
        assert_eq!(room(&full, &note("x")), Ok(()));
        // Sent files no longer count against the next task.
        let sent = board(vec![(file(1), true); MAX_ATTACHED]);
        assert_eq!(room(&sent, &file(1)), Ok(()));
        let big = board(vec![(file(MAX_ATTACHED_BYTES as u64), false)]);
        assert!(room(&big, &file(1)).unwrap_err().contains("MB of new"));
        let kept = board(vec![(file(MAX_BOARD_BYTES), true)]);
        assert!(room(&kept, &file(1)).unwrap_err().contains("keeps at most"));
        let many = board(vec![(note("x"), true); MAX_BOARD_ITEMS]);
        assert!(room(&many, &note("x")).unwrap_err().contains("items"));
    }

    #[test]
    fn only_unsent_items_go_and_sending_marks_them() {
        let b = board(vec![
            (file(1), true),
            (note("old"), true),
            (note("new"), false),
            (file(2), false),
        ]);
        let (ids, files, refs) = unsent(&b);
        assert_eq!(ids, [b.items[2].id, b.items[3].id]);
        assert_eq!(files, [(b.items[3].id, "a.pdf".to_string())]);
        assert_eq!(refs, [Reference::Note { text: "new".into() }]);
        let b = mark_sent(b.clone(), &[b.items[2].id]);
        let (ids, files, refs) = unsent(&b);
        assert_eq!((ids.len(), files.len(), refs.len()), (1, 1, 0));
    }

    #[test]
    fn a_new_apps_board_keeps_files_and_goes_to_the_app_its_thread_creates() {
        let dir = std::env::temp_dir().join(format!("mis-boards-test-{}", Uuid::new_v4()));
        let boards = Boards::open(dir.clone()).unwrap();
        let new = Owner::NewApp;
        let pdf = BoardRef::File {
            name: "Chores.pdf".into(),
            bytes: 8,
        };
        let f = boards.add(&new, pdf, Some(b"%PDF-1.4"), 1).unwrap();
        let n = boards.add(&new, note("10 stars = $1"), None, 2).unwrap();
        let (kind, name, bytes) = boards.file(&new, f.id).unwrap();
        assert_eq!(
            (kind.ext(), name.as_str(), bytes.as_slice()),
            ("pdf", "Chores.pdf", &b"%PDF-1.4"[..])
        );
        assert_eq!(boards.file(&new, n.id), None, "a note has no file");
        let thread = Uuid::new_v4();
        boards.sent(&new, &[f.id, n.id], Some(thread)).unwrap();
        assert_eq!(
            boards.get(&new),
            Board::default(),
            "the next new app starts empty"
        );
        let pool = Owner::App("pool".into());
        boards.adopt(Uuid::new_v4(), "pool");
        assert_eq!(
            boards.get(&pool),
            Board::default(),
            "another thread's app gets nothing"
        );
        boards.adopt(thread, "pool");
        let got = boards.get(&pool);
        assert!(got.items.iter().all(|i| i.sent) && got.items.len() == 2);
        assert!(boards.file(&pool, f.id).is_some());
        assert!(boards.remove(&pool, f.id));
        assert!(!boards.remove(&pool, f.id));
        assert_eq!(boards.get(&pool).items.len(), 1);
        assert_eq!(boards.file(&pool, f.id), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}

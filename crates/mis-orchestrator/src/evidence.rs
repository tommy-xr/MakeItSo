//! Evidence files agents attach: checked again here (a worker's word is never
//! trusted), stored as `<dir>/<uuid>.<ext>`, served back, swept after 30 days.
//! `check` is pure; `Store` is the filesystem shell around it.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use base64ct::{Base64, Encoding};
use mis_types::evidence::*;
use mis_types::{first_line, Evidence, EvidenceDraft};
use uuid::Uuid;

use crate::images::{private_dir, sweep_dir};

/// Older files are deleted. Task history is kept as long, so its evidence goes with it.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// A draft's contents, checked, as files to store.
#[derive(Debug)]
pub struct Checked {
    title: String,
    pub files: Vec<(FileKind, Vec<u8>)>,
    shape: Shape,
}

#[derive(Debug)]
enum Shape {
    Image,
    Compare,
    Table { rows: usize, cols: usize },
    Output { lines: usize, truncated: bool },
}

/// `draft` as files for a task already `held` this evidence: media must sniff
/// as accepted media, a table is rebuilt from its texts, output re-bounded,
/// all within the task's caps.
pub fn check(draft: EvidenceDraft, held: &[Evidence]) -> Result<Checked, String> {
    let title = first_line(&draft.title, MAX_TITLE_CHARS);
    if title.is_empty() {
        return Err("no title".into());
    }
    if held.len() >= MAX_EVIDENCE {
        return Err(format!("at most {MAX_EVIDENCE} files per task"));
    }
    let decode = |data: &str| {
        // Refused before decoding: base64 is 4 chars per 3 bytes.
        if data.len() > MAX_FILE_BYTES.div_ceil(3) * 4 {
            return Err(format!("files are limited to {} MB", MAX_FILE_BYTES >> 20));
        }
        let bytes = Base64::decode_vec(data).map_err(|_| "media isn't base64".to_string())?;
        Ok::<_, String>((media(&bytes)?, bytes))
    };
    let (files, shape) = match draft.item {
        DraftItem::Image { data } => (vec![decode(&data)?], Shape::Image),
        DraftItem::Compare { before, after } => {
            let files = vec![decode(&before)?, decode(&after)?];
            if !files.iter().all(|(k, _)| k.is_image()) {
                return Err("compare takes two images".into());
            }
            (files, Shape::Compare)
        }
        DraftItem::Table { table } => {
            let (header, rows) = table.texts();
            let t = Table::new(header, rows, table.before, table.after)?;
            let shape = Shape::Table {
                rows: t.rows.len(),
                cols: t.header.len(),
            };
            let json = serde_json::to_vec(&t).expect("a table serializes");
            (vec![(FileKind::Table, json)], shape)
        }
        DraftItem::Output { text, truncated } => {
            let (text, cut) = bound_output(&text);
            let shape = Shape::Output {
                lines: text.lines().count(),
                truncated: truncated || cut,
            };
            (vec![(FileKind::Text, text.into_bytes())], shape)
        }
    };
    let held_bytes: u64 = held.iter().flat_map(|e| e.files()).map(|f| f.bytes).sum();
    let bytes: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
    if held_bytes + bytes > MAX_TASK_BYTES {
        return Err(format!(
            "at most {} MB of files per task",
            MAX_TASK_BYTES >> 20
        ));
    }
    Ok(Checked {
        title,
        files,
        shape,
    })
}

impl Checked {
    /// Listed as evidence `id` stored at Unix millis `at`, its files stored as
    /// `file_ids`, one per file in order.
    pub fn evidence(self, id: Uuid, file_ids: Vec<Uuid>, at: u64) -> Evidence {
        let mut files = (self.files.iter().zip(file_ids)).map(|((kind, b), id)| EvidenceFile {
            id,
            mime: *kind,
            bytes: b.len() as u64,
        });
        let mut next = || files.next().expect("an id per file");
        let item = match self.shape {
            Shape::Image => EvidenceItem::Image { file: next() },
            Shape::Compare => {
                let before = next();
                EvidenceItem::Compare {
                    before,
                    after: next(),
                }
            }
            Shape::Table { rows, cols } => EvidenceItem::Table {
                file: next(),
                rows,
                cols,
            },
            Shape::Output { lines, truncated } => EvidenceItem::Output {
                file: next(),
                lines,
                truncated,
            },
        };
        Evidence {
            id,
            title: self.title,
            at,
            item,
        }
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Use `dir`, creating it readable only by this user.
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        private_dir(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, id: Uuid, kind: FileKind) -> PathBuf {
        self.dir.join(format!("{id}.{}", kind.ext()))
    }

    /// Keep `bytes` under a fresh id.
    pub fn store(&self, kind: FileKind, bytes: &[u8]) -> std::io::Result<Uuid> {
        let id = Uuid::new_v4();
        crate::push::write_private(&self.path(id, kind), bytes)?;
        Ok(id)
    }

    /// File `id`, if it is still here.
    pub fn read(&self, id: Uuid) -> Option<(FileKind, Vec<u8>)> {
        let kind = (FileKind::ALL.into_iter()).find(|k| self.path(id, *k).is_file())?;
        Some((kind, std::fs::read(self.path(id, kind)).ok()?))
    }

    /// Delete expired files; how many went.
    pub fn sweep(&self, now: SystemTime) -> usize {
        sweep_dir(&self.dir, now, MAX_AGE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn draft(item: DraftItem) -> EvidenceDraft {
        EvidenceDraft {
            title: "Home\nsecond".into(),
            item,
        }
    }

    fn b64(b: &[u8]) -> String {
        Base64::encode_string(b)
    }

    #[test]
    fn media_must_really_be_media() {
        let ok = check(draft(DraftItem::Image { data: b64(PNG) }), &[]).unwrap();
        assert_eq!(ok.files, [(FileKind::Png, PNG.to_vec())]);
        let e = ok.evidence(Uuid::nil(), vec![Uuid::max()], 9);
        assert_eq!(e.title, "Home");
        let EvidenceItem::Image { file } = &e.item else {
            panic!()
        };
        assert_eq!(
            (file.id, file.mime, file.bytes),
            (Uuid::max(), FileKind::Png, 16)
        );
        // A worker claiming an image, sending SVG or HTML, or not base64.
        for data in [b64(b"<svg onload=alert(1)>"), b64(b"<html>"), "%%%".into()] {
            assert!(check(draft(DraftItem::Image { data }), &[]).is_err());
        }
        let huge = "A".repeat(MAX_FILE_BYTES.div_ceil(3) * 4 + 4);
        let e = check(draft(DraftItem::Image { data: huge }), &[]).unwrap_err();
        assert!(e.contains("10 MB"), "{e}");
        let gif = b64(b"GIF89a\x01\0");
        let cmp = DraftItem::Compare {
            before: b64(PNG),
            after: gif,
        };
        let EvidenceItem::Compare { before, after } = check(draft(cmp), &[])
            .unwrap()
            .evidence(Uuid::nil(), vec![Uuid::nil(), Uuid::max()], 0)
            .item
        else {
            panic!()
        };
        assert_eq!((before.mime, after.id), (FileKind::Png, Uuid::max()));
    }

    #[test]
    fn tables_and_output_are_rebuilt_not_trusted() {
        let table = Table {
            header: vec!["case".into(), "ms".into()],
            rows: vec![vec![
                Cell {
                    text: "  a ".into(),
                    num: Some(99.0),
                },
                Cell {
                    text: "12 ms".into(),
                    num: None,
                },
            ]],
            before: None,
            after: Some(1),
        };
        let c = check(draft(DraftItem::Table { table }), &[]).unwrap();
        let t: Table = serde_json::from_slice(&c.files[0].1).unwrap();
        assert_eq!(
            t.rows[0][0],
            Cell {
                text: "a".into(),
                num: None
            }
        );
        assert_eq!(t.rows[0][1].num, Some(12.0));
        let bad = Table {
            header: vec!["a".into()],
            rows: vec![],
            before: Some(4),
            after: None,
        };
        assert!(check(draft(DraftItem::Table { table: bad }), &[]).is_err());
        let text: String = (0..1000).map(|i| format!("{i}\n")).collect();
        let c = check(
            draft(DraftItem::Output {
                text,
                truncated: false,
            }),
            &[],
        )
        .unwrap();
        assert_eq!(c.files[0].0, FileKind::Text);
        let EvidenceItem::Output {
            lines, truncated, ..
        } = c.evidence(Uuid::nil(), vec![Uuid::nil()], 0).item
        else {
            panic!()
        };
        assert_eq!((lines, truncated), (MAX_OUTPUT_LINES, true));
    }

    #[test]
    fn a_task_is_capped_in_items_and_bytes() {
        let held = |n: usize, bytes: u64| -> Vec<Evidence> {
            let file = EvidenceFile {
                id: Uuid::nil(),
                mime: FileKind::Png,
                bytes,
            };
            let e = Evidence {
                id: Uuid::nil(),
                title: "t".into(),
                at: 0,
                item: EvidenceItem::Image { file },
            };
            vec![e; n]
        };
        let img = || draft(DraftItem::Image { data: b64(PNG) });
        assert!(check(img(), &held(MAX_EVIDENCE - 1, 1)).is_ok());
        assert!(check(img(), &held(MAX_EVIDENCE, 1))
            .unwrap_err()
            .contains("at most 32"));
        let e = check(img(), &held(5, MAX_TASK_BYTES / 5)).unwrap_err();
        assert!(e.contains("50 MB"), "{e}");
        let untitled = EvidenceDraft {
            title: " ".into(),
            ..img()
        };
        assert!(check(untitled, &[]).is_err());
    }

    #[test]
    fn stores_reads_and_sweeps() {
        let dir = std::env::temp_dir().join(format!("mis-evidence-test-{}", Uuid::new_v4()));
        let store = Store::open(dir.clone()).unwrap();
        let id = store.store(FileKind::Text, b"\x1b[32mok\x1b[0m").unwrap();
        assert_eq!(
            store.read(id),
            Some((FileKind::Text, b"\x1b[32mok\x1b[0m".to_vec()))
        );
        assert_eq!(store.read(Uuid::new_v4()), None);
        let day = Duration::from_secs(24 * 3600);
        assert_eq!(store.sweep(SystemTime::now() + 29 * day), 0);
        assert_eq!(store.sweep(SystemTime::now() + 31 * day), 1);
        assert_eq!(store.read(id), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

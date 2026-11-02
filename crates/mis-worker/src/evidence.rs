//! Files an agent attaches with `report_evidence`: paths resolved inside the
//! task's folder or the worker's roots, media told apart by its bytes, tables
//! parsed, output bounded. Only `Files::read` touches the disk.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64ct::{Base64, Encoding};
use mis_types::evidence::{self, DraftItem, EvidenceDraft, Table, MAX_FILE_BYTES};
use serde_json::Value;

/// Where a task's agent may attach files from.
#[derive(Clone)]
pub struct Files {
    /// The task's folder, canonical.
    pub cwd: PathBuf,
    /// The worker's roots, canonical.
    pub roots: Arc<Vec<PathBuf>>,
}

impl Files {
    /// Whether a canonical path is inside the task's folder or a root.
    fn allows(&self, real: &Path) -> bool {
        real.starts_with(&self.cwd) || self.roots.iter().any(|r| real.starts_with(r))
    }

    /// The bytes of `path` (relative to the task's folder): a regular file of at
    /// most `MAX_FILE_BYTES` whose real path, symlinks resolved, is allowed.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let real = (self.cwd.join(path).canonicalize()).map_err(|e| format!("{path}: {e}"))?;
        if !self.allows(&real) {
            return Err(format!(
                "{path} is outside the task's folder and the worker's roots"
            ));
        }
        // Checked before opening: opening a FIFO would block.
        let meta = std::fs::metadata(&real).map_err(|e| format!("{path}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{path} is not a regular file"));
        }
        if meta.len() > MAX_FILE_BYTES as u64 {
            return Err(format!("files are limited to {} MB", MAX_FILE_BYTES >> 20));
        }
        let mut bytes = vec![];
        (std::fs::File::open(&real).map_err(|e| format!("{path}: {e}"))?)
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{path}: {e}"))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!("files are limited to {} MB", MAX_FILE_BYTES >> 20));
        }
        Ok(bytes)
    }
}

/// Reads a path's bytes; `Files::read` in the worker, a map in tests.
pub type Reader<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

/// `report_evidence` arguments of kind image, compare, table or output as a
/// draft for the orchestrator.
pub fn draft(args: &Value, title: String, read: Reader) -> Result<EvidenceDraft, String> {
    let path = |k: &str| {
        (args[k].as_str())
            .filter(|p| !p.trim().is_empty())
            .ok_or(format!("{k} must name a file in your folder"))
    };
    let media = |k: &str| {
        let p = path(k)?;
        let bytes = read(p)?;
        let kind = evidence::media(&bytes).map_err(|e| format!("{p}: {e}"))?;
        Ok::<_, String>((kind, Base64::encode_string(&bytes)))
    };
    let item = match args["kind"].as_str() {
        Some("image") => DraftItem::Image {
            data: media("path")?.1,
        },
        Some("compare") => {
            let (before, after) = (media("before")?, media("after")?);
            if !(before.0.is_image() && after.0.is_image()) {
                return Err("compare takes two images".into());
            }
            DraftItem::Compare {
                before: before.1,
                after: after.1,
            }
        }
        Some("table") => DraftItem::Table {
            table: table(args, read)?,
        },
        Some("output") => {
            let text = match (args["path"].as_str(), args["text"].as_str()) {
                (Some(p), None) => String::from_utf8_lossy(&read(p)?).into_owned(),
                (None, Some(t)) => t.to_string(),
                _ => return Err("output takes a path or text".into()),
            };
            let (text, truncated) = evidence::bound_output(&text);
            DraftItem::Output { text, truncated }
        }
        _ => return Err("kind must be link, test, note, image, compare, table or output".into()),
    };
    Ok(EvidenceDraft { title, item })
}

/// A table from a `.csv`, `.json` or `.md` file or inline `rows`, header
/// first; `before`/`after` name its baseline and new columns.
fn table(args: &Value, read: Reader) -> Result<Table, String> {
    let cells = match (args["path"].as_str(), &args["rows"]) {
        (Some(p), Value::Null) => {
            let bytes = read(p)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| format!("{p} is not UTF-8"))?;
            let ext = Path::new(p).extension().and_then(|e| e.to_str());
            match ext.map(str::to_ascii_lowercase).as_deref() {
                Some("csv") => csv(text),
                Some("json") => {
                    json_rows(&serde_json::from_str(text).map_err(|e| format!("{p}: {e}"))?)?
                }
                Some("md" | "markdown") => markdown(text)?,
                _ => return Err("a table file is .csv, .json or .md".into()),
            }
        }
        (None, rows @ Value::Array(_)) => json_rows(rows)?,
        _ => return Err("table takes a path or rows".into()),
    };
    let mut cells = cells.into_iter();
    let header = cells.next().ok_or("the table is empty")?;
    let col = |k: &str| match args[k].as_str() {
        None => Ok(None),
        Some(name) => (header.iter().position(|h| h.trim() == name.trim()))
            .map(Some)
            .ok_or(format!("no column named {name}")),
    };
    let (before, after) = (col("before")?, col("after")?);
    Table::new(header, cells.collect(), before, after)
}

/// CSV rows: quoted fields with `""` escapes and commas or newlines inside;
/// blank lines skipped.
fn csv(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut field, mut quoted) = (vec![], vec![], String::new(), false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\r' if !quoted => {}
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|r: &Vec<String>| r.iter().any(|c| !c.trim().is_empty()));
    rows
}

/// The first pipe table in `text`: a header row, its `|---|` line, then rows.
fn markdown(text: &str) -> Result<Vec<Vec<String>>, String> {
    let split = |l: &str| -> Vec<String> {
        let l = l.trim();
        let l = l.strip_prefix('|').unwrap_or(l);
        let l = l.strip_suffix('|').unwrap_or(l);
        // `\|` is a pipe inside a cell.
        let l = l.replace("\\|", "\0");
        l.split('|').map(|c| c.trim().replace('\0', "|")).collect()
    };
    let rule = |l: &str| {
        split(l)
            .iter()
            .all(|c| c.contains('-') && c.chars().all(|ch| "-: ".contains(ch)))
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = (lines.windows(2))
        .position(|w| w[0].contains('|') && rule(w[1]))
        .ok_or("no markdown table: a header row, then |---|")?;
    let rows = lines[start + 2..].iter().take_while(|l| l.contains('|'));
    Ok(std::iter::once(split(lines[start]))
        .chain(rows.map(|l| split(l)))
        .collect())
}

/// JSON rows: an array of arrays, header first, or `{"header": [..], "rows": [[..]]}`.
fn json_rows(v: &Value) -> Result<Vec<Vec<String>>, String> {
    let bad = || "a JSON table is an array of rows (header first) or {header, rows}".to_string();
    let rows: Vec<&Value> = match v {
        Value::Array(rows) => rows.iter().collect(),
        Value::Object(o) => {
            let header = o.get("header").ok_or_else(bad)?;
            let rows = o.get("rows").and_then(Value::as_array).ok_or_else(bad)?;
            std::iter::once(header).chain(rows).collect()
        }
        _ => return Err(bad()),
    };
    let cell = |c: &Value| match c {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Ok(String::new()),
        _ => Err("table cells are strings or numbers".to_string()),
    };
    (rows.into_iter())
        .map(|r| r.as_array().ok_or_else(bad)?.iter().map(cell).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mis_types::evidence::{Cell, FileKind};
    use serde_json::json;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    /// Fresh canonical dir with a `task` folder, `task/shot.png` and an outside
    /// `secret.png`; removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let d = std::env::temp_dir().join(format!("mis-evidence-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(d.join("root/task")).unwrap();
            let d = d.canonicalize().unwrap();
            std::fs::write(d.join("root/task/shot.png"), PNG).unwrap();
            std::fs::write(d.join("root/other.png"), PNG).unwrap();
            std::fs::write(d.join("secret.png"), PNG).unwrap();
            Tmp(d)
        }
        fn files(&self) -> Files {
            Files {
                cwd: self.0.join("root/task"),
                roots: Arc::new(vec![self.0.join("root")]),
            }
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_only_regular_files_inside_the_folder_or_roots() {
        let t = Tmp::new();
        let f = t.files();
        assert_eq!(f.read("shot.png").unwrap(), PNG);
        assert_eq!(
            f.read("../other.png").unwrap(),
            PNG,
            "elsewhere under a root"
        );
        let abs = t.0.join("root/task/shot.png");
        assert_eq!(f.read(abs.to_str().unwrap()).unwrap(), PNG);
        for p in ["../../secret.png", t.0.join("secret.png").to_str().unwrap()] {
            assert!(f.read(p).unwrap_err().contains("outside"), "{p}");
        }
        assert!(f.read("missing.png").is_err());
        assert!(f.read(".").unwrap_err().contains("not a regular file"));
        let big = t.0.join("root/task/big.png");
        std::fs::write(&big, vec![0; MAX_FILE_BYTES + 1]).unwrap();
        assert!(f.read("big.png").unwrap_err().contains("10 MB"));
        // The task's folder counts even when no root holds it.
        let alone = Files {
            cwd: t.0.join("root/task"),
            roots: Arc::new(vec![]),
        };
        assert!(alone.read("shot.png").is_ok() && alone.read("../other.png").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_may_not_escape() {
        let t = Tmp::new();
        let link = |name: &str, to: &Path| {
            std::os::unix::fs::symlink(to, t.0.join("root/task").join(name)).unwrap()
        };
        link("leak.png", &t.0.join("secret.png"));
        link("dir", &t.0);
        link("ok.png", &t.0.join("root/task/shot.png"));
        let f = t.files();
        assert!(f.read("leak.png").unwrap_err().contains("outside"));
        assert!(f.read("dir/secret.png").unwrap_err().contains("outside"));
        assert_eq!(f.read("ok.png").unwrap(), PNG, "a link within is fine");
        // A FIFO is refused without blocking on open.
        let fifo = t.0.join("root/task/pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            assert!(f.read("pipe").unwrap_err().contains("not a regular file"));
        }
    }

    /// A reader over an in-memory folder.
    fn reader(files: &[(&str, &[u8])]) -> impl Fn(&str) -> Result<Vec<u8>, String> {
        let files: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_vec()))
            .collect();
        move |p: &str| {
            let hit = files.iter().find(|(q, _)| q == p);
            hit.map(|(_, b)| b.clone()).ok_or(format!("{p}: not found"))
        }
    }

    fn drafted(args: Value, files: &[(&str, &[u8])]) -> Result<DraftItem, String> {
        draft(&args, "t".into(), &reader(files)).map(|d| d.item)
    }

    #[test]
    fn media_is_sniffed_not_named() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>";
        let files: &[(&str, &[u8])] = &[("a.png", PNG), ("fake.png", svg), ("page.png", b"<html>")];
        let img = drafted(json!({"kind": "image", "path": "a.png"}), files).unwrap();
        assert_eq!(
            img,
            DraftItem::Image {
                data: Base64::encode_string(PNG)
            }
        );
        for p in ["fake.png", "page.png"] {
            let e = drafted(json!({"kind": "image", "path": p}), files).unwrap_err();
            assert!(e.contains("not a PNG"), "{e}");
        }
        let cmp = json!({"kind": "compare", "before": "a.png", "after": "a.png"});
        assert!(matches!(drafted(cmp, files), Ok(DraftItem::Compare { .. })));
        let half = json!({"kind": "compare", "before": "a.png"});
        assert!(drafted(half, files).unwrap_err().contains("after"));
        assert!(drafted(json!({"kind": "image"}), files).is_err());
        assert_eq!(FileKind::sniff(PNG), Some(FileKind::Png));
    }

    fn table_of(args: Value, files: &[(&str, &[u8])]) -> Table {
        match drafted(args, files) {
            Ok(DraftItem::Table { table }) => table,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tables_parse_from_csv_json_md_and_rows() {
        let csv = b"case,before,after\r\n\"cold, start\",120 ms,80 ms\n\nwarm,\"\"\"x\"\"\",9\n";
        let md = b"# Bench\n\n| case | before | after |\n|:--|--:|--:|\n| cold \\| start | 120 ms | 80 ms |\n| warm | 10 | 9 |\n\nafter text | x\n";
        let json = br#"{"header": ["case", "before", "after"], "rows": [["cold", 120, 80.5], ["warm", null, true]]}"#;
        let files: &[(&str, &[u8])] = &[
            ("b.csv", csv),
            ("b.md", md),
            ("b.json", json),
            ("b.txt", b"x"),
        ];
        let t = table_of(
            json!({"kind": "table", "path": "b.csv", "before": "before", "after": "after"}),
            files,
        );
        assert_eq!(t.header, ["case", "before", "after"]);
        assert_eq!((t.before, t.after), (Some(1), Some(2)));
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0][0].text, "cold, start");
        assert_eq!(
            t.rows[0][2],
            Cell {
                text: "80 ms".into(),
                num: Some(80.0)
            }
        );
        assert_eq!(t.rows[1][1].text, "\"x\"");
        let t = table_of(json!({"kind": "table", "path": "b.md"}), files);
        assert_eq!(t.rows.len(), 2, "stops at the table's end");
        assert_eq!(t.rows[0][0].text, "cold | start");
        assert_eq!(t.rows[1][2].num, Some(9.0));
        let t = table_of(json!({"kind": "table", "path": "b.json"}), files);
        assert_eq!(t.rows[0][2].num, Some(80.5));
        assert_eq!(
            (t.rows[1][1].text.as_str(), t.rows[1][2].text.as_str()),
            ("", "true")
        );
        let rows = json!([["n", "ms"], ["a", 1.5]]);
        let t = table_of(json!({"kind": "table", "rows": rows, "after": "ms"}), files);
        assert_eq!((t.rows[0][1].num, t.after), (Some(1.5), Some(1)));
        for (args, why) in [
            (
                json!({"kind": "table", "path": "b.txt"}),
                ".csv, .json or .md",
            ),
            (
                json!({"kind": "table", "path": "b.csv", "before": "nope"}),
                "no column named nope",
            ),
            (json!({"kind": "table"}), "path or rows"),
            (
                json!({"kind": "table", "rows": [[{"a": 1}]]}),
                "strings or numbers",
            ),
            (json!({"kind": "table", "rows": []}), "empty"),
            (
                json!({"kind": "table", "rows": {"header": ["a"]}}),
                "path or rows",
            ),
        ] {
            let e = drafted(args.clone(), files).unwrap_err();
            assert!(e.contains(why), "{args}: {e}");
        }
        assert!(markdown("no table | here\n").is_err());
    }

    #[test]
    fn output_from_text_or_file_is_bounded() {
        let log: String = (0..1000)
            .map(|i| format!("\x1b[31mline {i}\x1b[0m\n"))
            .collect();
        let files: &[(&str, &[u8])] = &[("log.txt", log.as_bytes()), ("bin", b"\xff\xfeok")];
        let Ok(DraftItem::Output { text, truncated }) =
            drafted(json!({"kind": "output", "path": "log.txt"}), files)
        else {
            panic!()
        };
        assert!(truncated && text.ends_with("line 999\x1b[0m\n"));
        let Ok(DraftItem::Output { text, truncated }) =
            drafted(json!({"kind": "output", "text": "ok\n"}), files)
        else {
            panic!()
        };
        assert_eq!((text.as_str(), truncated), ("ok\n", false));
        let Ok(DraftItem::Output { text, .. }) =
            drafted(json!({"kind": "output", "path": "bin"}), files)
        else {
            panic!()
        };
        assert_eq!(text, "\u{fffd}\u{fffd}ok");
        let both = json!({"kind": "output", "path": "log.txt", "text": "x"});
        assert!(drafted(both, files).is_err());
    }
}

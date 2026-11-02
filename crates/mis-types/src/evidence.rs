//! Evidence files an agent attaches (screenshots, before/after pairs, benchmark
//! tables, command output) and its own closing summary. Worker and orchestrator
//! run the same pure checks: the worker to answer the agent, the orchestrator
//! because it never trusts what a worker sends.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{first_line, ImageKind};

/// Largest evidence file, in bytes.
pub const MAX_FILE_BYTES: usize = 10 << 20;
/// Most evidence bytes one task may store.
pub const MAX_TASK_BYTES: u64 = 50 << 20;
/// Most evidence items one task may carry.
pub const MAX_EVIDENCE: usize = 32;
/// Longest video accepted, in seconds.
pub const MAX_VIDEO_SECS: f64 = 20.5;
/// Command output keeps at most its last this many lines and bytes.
pub const MAX_OUTPUT_LINES: usize = 400;
pub const MAX_OUTPUT_BYTES: usize = 64 << 10;
pub const MAX_TABLE_ROWS: usize = 200;
pub const MAX_TABLE_COLS: usize = 20;
pub const MAX_CELL_CHARS: usize = 80;
pub const MAX_HEADLINE_WORDS: usize = 12;
pub const MAX_POINTS: usize = 3;
pub const MAX_POINT_CHARS: usize = 120;
/// Longest evidence title.
pub const MAX_TITLE_CHARS: usize = 200;

/// A stored evidence file's format, named by its MIME type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileKind {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/webp")]
    Webp,
    #[serde(rename = "image/gif")]
    Gif,
    #[serde(rename = "video/mp4")]
    Mp4,
    /// A [`Table`] as JSON.
    #[serde(rename = "application/json")]
    Table,
    /// UTF-8 command output, ANSI escapes kept.
    #[serde(rename = "text/plain")]
    Text,
}

impl FileKind {
    pub const ALL: [Self; 7] = [
        Self::Png,
        Self::Jpeg,
        Self::Webp,
        Self::Gif,
        Self::Mp4,
        Self::Table,
        Self::Text,
    ];

    /// The `Content-Type` to serve it with.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
            Self::Mp4 => "video/mp4",
            Self::Table => "application/json",
            Self::Text => "text/plain; charset=utf-8",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
            Self::Table => "json",
            Self::Text => "txt",
        }
    }

    pub fn is_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg | Self::Webp | Self::Gif)
    }

    /// The media format `bytes` are in, by magic number; None for anything
    /// else (SVG, HTML, any other video).
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if let Some(k) = ImageKind::sniff(bytes) {
            return Some(match k {
                ImageKind::Png => Self::Png,
                ImageKind::Jpeg => Self::Jpeg,
                ImageKind::Webp => Self::Webp,
                ImageKind::Gif => Self::Gif,
            });
        }
        let brand = bytes.get(8..12)?;
        let mp4 = [
            b"isom", b"iso2", b"iso4", b"iso5", b"iso6", b"mp41", b"mp42", b"avc1", b"M4V ",
        ];
        (bytes.get(4..8)? == b"ftyp" && mp4.iter().any(|b| *b == brand)).then_some(Self::Mp4)
    }
}

/// The media file `bytes` are, if accepted: an image, or an MP4 of at most
/// `MAX_VIDEO_SECS`, within `MAX_FILE_BYTES`.
pub fn media(bytes: &[u8]) -> Result<FileKind, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!("files are limited to {} MB", MAX_FILE_BYTES >> 20));
    }
    let kind = FileKind::sniff(bytes).ok_or("not a PNG, JPEG, WebP, GIF or MP4 file")?;
    if kind == FileKind::Mp4 {
        match mp4_seconds(bytes) {
            Some(s) if s <= MAX_VIDEO_SECS => {}
            Some(s) => return Err(format!("video is {s:.0} s; at most 20 s")),
            None => return Err("can't read the video's length".into()),
        }
    }
    Ok(kind)
}

/// ISO-BMFF boxes in `b`: (type, payload). Stops at the first malformed one.
fn boxes(mut b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    std::iter::from_fn(move || {
        let be = |s: &[u8]| s.iter().fold(0u64, |a, x| a << 8 | *x as u64);
        let (head, size) = match be(b.get(..4)?) {
            0 => (8, b.len() as u64),
            1 => (16, be(b.get(8..16)?)),
            n => (8, n),
        };
        if size < head || size > b.len() as u64 {
            return None;
        }
        let (this, rest) = b.split_at(size as usize);
        b = rest;
        Some((&this[4..8], &this[head as usize..]))
    })
}

/// An MP4's length from its `moov/mvhd` box; None if it has none readable.
pub fn mp4_seconds(bytes: &[u8]) -> Option<f64> {
    let (_, moov) = boxes(bytes).find(|(k, _)| *k == b"moov")?;
    let (_, mvhd) = boxes(moov).find(|(k, _)| *k == b"mvhd")?;
    let be = |r: std::ops::Range<usize>| {
        let s = mvhd.get(r)?;
        Some(s.iter().fold(0u64, |a, x| a << 8 | *x as u64))
    };
    // version, flags, created, modified, then timescale and duration (32 or 64 bits).
    let (scale, duration) = match mvhd.first()? {
        0 => (be(12..16)?, be(16..20)?),
        1 => (be(20..24)?, be(24..32)?),
        _ => return None,
    };
    (scale > 0).then(|| duration as f64 / scale as f64)
}

/// The tail of command output: its last `MAX_OUTPUT_LINES` lines within
/// `MAX_OUTPUT_BYTES`, CRLF as LF, NULs dropped, ANSI escapes kept. True if cut.
pub fn bound_output(s: &str) -> (String, bool) {
    let s = s.replace("\r\n", "\n").replace('\0', "");
    let lines: Vec<&str> = s.split_inclusive('\n').collect();
    let (mut bytes, mut keep) = (0, 0);
    for line in lines.iter().rev().take(MAX_OUTPUT_LINES) {
        if bytes + line.len() > MAX_OUTPUT_BYTES {
            break;
        }
        bytes += line.len();
        keep += 1;
    }
    if keep == 0 && !lines.is_empty() {
        // One line longer than the budget: its end.
        let last = lines[lines.len() - 1];
        let mut start = last.len() - MAX_OUTPUT_BYTES;
        while !last.is_char_boundary(start) {
            start += 1;
        }
        return (last[start..].to_string(), true);
    }
    (lines[lines.len() - keep..].concat(), keep < lines.len())
}

/// One table cell: its text, and the number in it if it reads as one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cell {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num: Option<f64>,
}

impl Cell {
    pub fn new(text: &str) -> Self {
        let text = first_line(text.trim(), MAX_CELL_CHARS);
        let num = number(&text);
        Self { text, num }
    }
}

/// The number a cell shows, ignoring a unit after it: `12.5 ms`, `-3%`,
/// `1,234`, `2.1x`, `1e3` read as numbers; `v1.2`, `12-14`, `n/a` don't.
pub fn number(s: &str) -> Option<f64> {
    let s = s.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || "+-.,eE".contains(c)))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(end);
    let unit_ok = unit
        .trim()
        .chars()
        .all(|c| c.is_alphabetic() || "%/µ".contains(c));
    // Commas only as thousands separators, three digits each: `1,234,567.5`.
    let grouped = num.split(',').skip(1).all(|g| {
        let b = g.as_bytes();
        b.len() >= 3
            && b[..3].iter().all(u8::is_ascii_digit)
            && !b.get(3).is_some_and(u8::is_ascii_digit)
    });
    if !unit_ok || !grouped {
        return None;
    }
    num.replace(',', "")
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

/// A benchmark or results table: a header and rows of cells, numbers detected
/// so a client can colour the change from the `before` column to `after`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Table {
    pub header: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Column index of the baseline numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<usize>,
    /// Column index of the new numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<usize>,
}

impl Table {
    /// From cell texts, header first. Short rows are padded; Err past the
    /// size limits, for a longer row, or a hint naming no column.
    pub fn new(
        header: Vec<String>,
        rows: Vec<Vec<String>>,
        before: Option<usize>,
        after: Option<usize>,
    ) -> Result<Self, String> {
        let cols = header.len();
        if cols == 0 {
            return Err("the table has no header".into());
        }
        if cols > MAX_TABLE_COLS {
            return Err(format!("at most {MAX_TABLE_COLS} columns"));
        }
        if rows.len() > MAX_TABLE_ROWS {
            return Err(format!("at most {MAX_TABLE_ROWS} rows"));
        }
        if let Some(i) = rows.iter().position(|r| r.len() > cols) {
            return Err(format!("row {} has more cells than the header", i + 1));
        }
        if before.or(after).is_some_and(|c| c >= cols) {
            return Err("before/after names no column".into());
        }
        let header = header.iter().map(|h| Cell::new(h).text).collect();
        let rows = rows
            .into_iter()
            .map(|mut r| {
                r.resize(cols, String::new());
                r.iter().map(|c| Cell::new(c)).collect()
            })
            .collect();
        Ok(Self {
            header,
            rows,
            before,
            after,
        })
    }

    /// Its cell texts, header first, for rebuilding it with [`Table::new`].
    pub fn texts(&self) -> (Vec<String>, Vec<Vec<String>>) {
        let rows = self.rows.iter();
        let rows = rows.map(|r| r.iter().map(|c| c.text.clone()).collect());
        (self.header.clone(), rows.collect())
    }
}

/// A stored file of an evidence item, served at `/api/evidence/<id>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceFile {
    pub id: Uuid,
    pub mime: FileKind,
    pub bytes: u64,
}

/// One item an agent attached to its task, as clients list it: its files are
/// fetched apart, so status stays small.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    pub id: Uuid,
    pub title: String,
    /// Unix millis when the orchestrator stored it.
    pub at: u64,
    #[serde(flatten)]
    pub item: EvidenceItem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceItem {
    /// A screenshot, GIF or short MP4.
    Image { file: EvidenceFile },
    /// The same view before and after the change.
    Compare {
        before: EvidenceFile,
        after: EvidenceFile,
    },
    /// A [`Table`] as JSON.
    Table {
        file: EvidenceFile,
        rows: usize,
        cols: usize,
    },
    /// Command output, its tail if `truncated`.
    Output {
        file: EvidenceFile,
        lines: usize,
        truncated: bool,
    },
}

impl Evidence {
    pub fn files(&self) -> Vec<&EvidenceFile> {
        match &self.item {
            EvidenceItem::Compare { before, after } => vec![before, after],
            EvidenceItem::Image { file }
            | EvidenceItem::Table { file, .. }
            | EvidenceItem::Output { file, .. } => vec![file],
        }
    }
}

/// Evidence as a worker sends it, contents inline. The orchestrator checks
/// and stores it, then lists it as an [`Evidence`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceDraft {
    pub title: String,
    #[serde(flatten)]
    pub item: DraftItem,
}

/// Media is standard padded base64.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DraftItem {
    Image { data: String },
    Compare { before: String, after: String },
    Table { table: Table },
    Output { text: String, truncated: bool },
}

/// The agent's own summary of its finished task, for a phone: a headline and
/// a few points to drill into. Supersedes the orchestrator's summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskResult {
    pub headline: String,
    #[serde(default)]
    pub points: Vec<String>,
}

impl TaskResult {
    /// Cut to size: one-line headline of at most `MAX_HEADLINE_WORDS` words,
    /// the first `MAX_POINTS` non-blank points of one short line each. Also
    /// says what was cut, for the agent. Err for a blank headline.
    pub fn new(headline: &str, points: &[&str]) -> Result<(Self, Vec<String>), String> {
        let mut cut = vec![];
        let line = first_line(headline, MAX_TITLE_CHARS);
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            return Err("headline must be a non-empty string".into());
        }
        let mut headline = words[..words.len().min(MAX_HEADLINE_WORDS)].join(" ");
        if words.len() > MAX_HEADLINE_WORDS {
            headline.push('…');
            cut.push(format!("headline cut to {MAX_HEADLINE_WORDS} words"));
        }
        let given: Vec<&str> = points
            .iter()
            .copied()
            .filter(|p| !p.trim().is_empty())
            .collect();
        if given.len() > MAX_POINTS {
            cut.push(format!("kept the first {MAX_POINTS} points"));
        }
        let points: Vec<String> = (given.iter().take(MAX_POINTS))
            .map(|p| first_line(p, MAX_POINT_CHARS))
            .collect();
        if points.iter().zip(&given).any(|(p, g)| p != g.trim()) {
            cut.push(format!("points cut to one line of {MAX_POINT_CHARS} chars"));
        }
        Ok((Self { headline, points }, cut))
    }

    /// One line for the task's summary: `Fixed the test · 3 retries removed`.
    pub fn line(&self) -> String {
        std::iter::once(&self.headline)
            .chain(&self.points)
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    /// An ISO-BMFF box.
    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(payload);
        b
    }

    /// An MP4 of `secs` (timescale 1000), its `moov` after a `mdat` like most encoders write.
    fn mp4(secs: u32, version: u8) -> Vec<u8> {
        let mut mvhd = vec![version, 0, 0, 0];
        if version == 0 {
            mvhd.extend([0; 8]);
            mvhd.extend(1000u32.to_be_bytes());
            mvhd.extend((secs * 1000).to_be_bytes());
        } else {
            mvhd.extend([0; 16]);
            mvhd.extend(1000u32.to_be_bytes());
            mvhd.extend((secs as u64 * 1000).to_be_bytes());
        }
        mvhd.extend([0; 80]);
        let mut v = bx(b"ftyp", b"isom\0\0\x02\0isomiso2");
        v.extend(bx(b"mdat", &[7; 64]));
        v.extend(bx(b"moov", &bx(b"mvhd", &mvhd)));
        v
    }

    #[test]
    fn sniffs_media_by_bytes_and_rejects_svg_html_and_other_video() {
        assert_eq!(FileKind::sniff(PNG), Some(FileKind::Png));
        assert_eq!(FileKind::sniff(b"GIF89a\x01\0"), Some(FileKind::Gif));
        assert_eq!(FileKind::sniff(&mp4(3, 0)), Some(FileKind::Mp4));
        for bytes in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\">"[..],
            b"<?xml version=\"1.0\"?><svg>",
            b"<!DOCTYPE html><script>",
            b"\0\0\0\x14ftypqt  \0\0\0\0qt  ",
            b"\0\0\0\x08ftyp",
            b"",
        ] {
            assert_eq!(FileKind::sniff(bytes), None, "{bytes:?}");
            assert!(media(bytes).is_err());
        }
    }

    #[test]
    fn videos_must_be_short_and_readable() {
        assert_eq!(mp4_seconds(&mp4(12, 0)), Some(12.0));
        assert_eq!(mp4_seconds(&mp4(12, 1)), Some(12.0));
        assert_eq!(media(&mp4(20, 0)), Ok(FileKind::Mp4));
        assert!(media(&mp4(21, 1)).unwrap_err().contains("at most 20 s"));
        // Cut off before its moov: length unknown, refused.
        let mut cut = mp4(5, 0);
        cut.truncate(cut.len() - 20);
        assert!(media(&cut).unwrap_err().contains("length"));
        // A box claiming more bytes than there are stops the walk.
        let mut lying = bx(b"ftyp", b"isom\0\0\x02\0");
        lying.extend(u32::MAX.to_be_bytes());
        lying.extend(b"moov");
        assert_eq!(mp4_seconds(&lying), None);
    }

    #[test]
    fn files_are_capped() {
        let mut big = PNG.to_vec();
        big.resize(MAX_FILE_BYTES + 1, 0);
        assert!(media(&big).unwrap_err().contains("10 MB"));
        big.truncate(MAX_FILE_BYTES);
        assert_eq!(media(&big), Ok(FileKind::Png));
    }

    #[test]
    fn output_keeps_its_tail_with_ansi() {
        let (s, cut) = bound_output("\x1b[32mok\x1b[0m\r\nnext\0\n");
        assert_eq!((s.as_str(), cut), ("\x1b[32mok\x1b[0m\nnext\n", false));
        let many: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let (s, cut) = bound_output(&many);
        assert!(cut);
        assert_eq!(s.lines().count(), MAX_OUTPUT_LINES);
        assert!(s.starts_with("line 600\n") && s.ends_with("line 999\n"));
        let wide = format!("{}\n", "é".repeat(MAX_OUTPUT_BYTES));
        let (s, cut) = bound_output(&format!("head\n{wide}"));
        assert!(
            cut && s.len() <= MAX_OUTPUT_BYTES && s.ends_with("é\n"),
            "{}",
            s.len()
        );
        let lines: String = (0..200).map(|_| format!("{}\n", "x".repeat(999))).collect();
        let (s, cut) = bound_output(&lines);
        assert!(cut && s.len() <= MAX_OUTPUT_BYTES && s.lines().count() == 65);
        assert_eq!(bound_output(""), (String::new(), false));
    }

    #[test]
    fn numbers_in_cells() {
        for (s, n) in [
            ("12.5 ms", 12.5),
            ("-3%", -3.0),
            ("1,234,567", 1234567.0),
            ("1,234.5", 1234.5),
            ("2.1x", 2.1),
            ("1e3", 1000.0),
            ("+0.5", 0.5),
            ("42 MB/s", 42.0),
            (".5", 0.5),
        ] {
            assert_eq!(number(s), Some(n), "{s}");
        }
        for s in [
            "v1.2", "12-14", "n/a", "", "1,2", "1,2345", "abc", "1.2.3", "3 (warm)", "inf",
        ] {
            assert_eq!(number(s), None, "{s}");
        }
    }

    #[test]
    fn tables_are_bounded_and_padded() {
        let s = |v: &[&str]| v.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        let t = Table::new(
            s(&["case", "before", "after"]),
            vec![s(&["a", "10 ms", "8 ms"]), s(&["b"])],
            Some(1),
            Some(2),
        )
        .unwrap();
        assert_eq!(
            t.rows[0][2],
            Cell {
                text: "8 ms".into(),
                num: Some(8.0)
            }
        );
        assert_eq!(
            t.rows[1][1],
            Cell {
                text: String::new(),
                num: None
            }
        );
        let json = serde_json::to_string(&t.rows[0][0]).unwrap();
        assert_eq!(json, r#"{"text":"a"}"#);
        let (h, r) = t.texts();
        assert_eq!(Table::new(h, r, t.before, t.after).unwrap(), t);
        assert!(Table::new(vec![], vec![], None, None).is_err());
        assert!(Table::new(s(&["a"]), vec![s(&["1", "2"])], None, None).is_err());
        assert!(Table::new(s(&["a"]), vec![], Some(1), None).is_err());
        let wide = vec![String::new(); MAX_TABLE_COLS + 1];
        assert!(Table::new(wide, vec![], None, None).is_err());
        let long = vec![s(&["1"]); MAX_TABLE_ROWS + 1];
        assert!(Table::new(s(&["a"]), long, None, None).is_err());
        let cell = Cell::new(&format!("{}\nsecond", "z".repeat(200)));
        assert_eq!(cell.text.chars().count(), MAX_CELL_CHARS);
    }

    #[test]
    fn result_is_cut_politely() {
        let (r, cut) =
            TaskResult::new(" Fixed the flaky test\nmore", &["3 retries removed", " "]).unwrap();
        assert_eq!(r.headline, "Fixed the flaky test");
        assert_eq!(r.points, ["3 retries removed"]);
        assert!(cut.is_empty());
        assert_eq!(r.line(), "Fixed the flaky test · 3 retries removed");
        let long = "one two three four five six seven eight nine ten eleven twelve thirteen";
        let (r, cut) = TaskResult::new(long, &["a", "b", "c", "d", &"p".repeat(200)]).unwrap();
        assert_eq!(
            r.headline,
            "one two three four five six seven eight nine ten eleven twelve…"
        );
        assert_eq!(r.points, ["a", "b", "c"]);
        assert_eq!(cut, ["headline cut to 12 words", "kept the first 3 points"]);
        let (r, cut) = TaskResult::new("ok", &[&"p".repeat(200)]).unwrap();
        assert_eq!(r.points[0].chars().count(), MAX_POINT_CHARS);
        assert_eq!(cut.len(), 1);
        assert!(TaskResult::new(" \n", &[]).is_err());
    }

    #[test]
    fn evidence_wire_shape() {
        let file = EvidenceFile {
            id: Uuid::nil(),
            mime: FileKind::Png,
            bytes: 3,
        };
        let e = Evidence {
            id: Uuid::nil(),
            title: "Home".into(),
            at: 5,
            item: EvidenceItem::Compare {
                before: file.clone(),
                after: file,
            },
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "compare");
        assert_eq!(v["before"]["mime"], "image/png");
        assert_eq!(serde_json::from_value::<Evidence>(v).unwrap(), e);
        let d = EvidenceDraft {
            title: "log".into(),
            item: DraftItem::Output {
                text: "x".into(),
                truncated: false,
            },
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "output");
        assert_eq!(serde_json::from_value::<EvidenceDraft>(v).unwrap(), d);
    }
}

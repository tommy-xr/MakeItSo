//! References a task carries beyond its request: uploaded files (images and documents),
//! links and notes. Documents are told apart by extension, then checked by their bytes.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ImageKind, MAX_IMAGES, MAX_IMAGE_BYTES};

/// Most uploads (images and documents) one task may carry.
pub const MAX_ATTACHED: usize = 12;
/// Most upload bytes one task may carry: what a worker's assignment frame has room for.
pub const MAX_ATTACHED_BYTES: usize = MAX_IMAGES * MAX_IMAGE_BYTES;
/// Most links and notes one task may carry.
pub const MAX_REFERENCES: usize = 32;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_LINK_TITLE_CHARS: usize = 200;
pub const MAX_NOTE_CHARS: usize = 4000;
/// Longest upload name kept, in bytes: with a worker's `<uuid>-` prefix it stays a legal file name.
pub const MAX_NAME_BYTES: usize = 120;

/// A document accepted as a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Pdf,
    Text,
    Markdown,
    Csv,
    Json,
}

impl DocKind {
    pub const ALL: [Self; 5] = [Self::Pdf, Self::Text, Self::Markdown, Self::Csv, Self::Json];

    pub fn ext(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Text => "txt",
            Self::Markdown => "md",
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }

    /// From a file name's extension, ignoring case: `Rules.MD`.
    pub fn from_name(name: &str) -> Option<Self> {
        let (_, ext) = name.rsplit_once('.')?;
        Self::ALL
            .into_iter()
            .find(|k| k.ext().eq_ignore_ascii_case(ext))
    }

    /// Whether `bytes` are this kind: a PDF's magic number, else UTF-8 text without NULs.
    pub fn fits(self, bytes: &[u8]) -> bool {
        match self {
            Self::Pdf => bytes.starts_with(b"%PDF-"),
            _ => !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok(),
        }
    }

    /// The `Content-Type` to serve it with: text kinds as plain text, so none renders as a page.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            _ => "text/plain; charset=utf-8",
        }
    }
}

/// An upload's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadKind {
    Image(ImageKind),
    Doc(DocKind),
}

impl UploadKind {
    pub fn all() -> impl Iterator<Item = Self> {
        (ImageKind::ALL.into_iter().map(Self::Image)).chain(DocKind::ALL.into_iter().map(Self::Doc))
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Image(k) => k.ext(),
            Self::Doc(k) => k.ext(),
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Image(k) => k.mime(),
            Self::Doc(k) => k.mime(),
        }
    }
}

/// An attached document's bytes, sent with its task's assignment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachedFile {
    pub id: Uuid,
    /// As uploaded, ending in its kind's extension: `Chore list.pdf`.
    pub name: String,
    /// Standard base64, padded.
    pub data: String,
}

/// A link or note for the agent, put in its prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reference {
    /// An http(s) page.
    Link {
        url: String,
        #[serde(default)]
        title: Option<String>,
    },
    Note {
        text: String,
    },
}

/// Err: why `refs` can't go with a task.
pub fn check_references(refs: &[Reference]) -> Result<(), String> {
    if refs.len() > MAX_REFERENCES {
        return Err(format!("at most {MAX_REFERENCES} links and notes"));
    }
    for r in refs {
        match r {
            Reference::Link { url, title } => {
                let (scheme, rest) = url.split_once("://").unwrap_or_default();
                let web = ["http", "https"]
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(scheme));
                let host = !rest.is_empty() && !rest.starts_with('/');
                if !(web && host) || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(format!("{url:?} isn't an http(s) link"));
                }
                if url.chars().count() > MAX_URL_CHARS {
                    return Err(format!("links are at most {MAX_URL_CHARS} characters"));
                }
                let title = title.as_deref().unwrap_or_default();
                if title.chars().count() > MAX_LINK_TITLE_CHARS || title.contains(char::is_control)
                {
                    return Err(format!(
                        "link titles are one line of at most {MAX_LINK_TITLE_CHARS} characters"
                    ));
                }
            }
            Reference::Note { text } => {
                if text.trim().is_empty() {
                    return Err("a note is empty".into());
                }
                if text.contains(|c: char| c.is_control() && c != '\n' && c != '\t') {
                    return Err("a note has control characters".into());
                }
                if text.chars().count() > MAX_NOTE_CHARS {
                    return Err(format!("notes are at most {MAX_NOTE_CHARS} characters"));
                }
            }
        }
    }
    Ok(())
}

/// `refs` (passed [`check_references`]) as a prompt section; empty without any. Notes are
/// quoted, so their lines stay theirs.
pub fn references_text(refs: &[Reference]) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = refs
        .iter()
        .map(|r| match r {
            Reference::Link { url, title } => match title.as_deref().map(str::trim) {
                Some(t) if !t.is_empty() => format!("- Link: {t} <{url}>"),
                _ => format!("- Link: <{url}>"),
            },
            Reference::Note { text } => {
                let quoted: Vec<String> = text.trim().lines().map(|l| format!("  > {l}")).collect();
                format!("- Note:\n{}", quoted.join("\n"))
            }
        })
        .collect();
    format!("References:\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_go_by_extension_then_bytes() {
        assert_eq!(DocKind::from_name("Chore list.PDF"), Some(DocKind::Pdf));
        assert_eq!(DocKind::from_name("rules.md"), Some(DocKind::Markdown));
        assert_eq!(DocKind::from_name("a.tar.json"), Some(DocKind::Json));
        for bad in ["page.html", "x.svg", "pdf", "notes.", ""] {
            assert_eq!(DocKind::from_name(bad), None, "{bad}");
        }
        assert!(DocKind::Pdf.fits(b"%PDF-1.7\n..."));
        assert!(!DocKind::Pdf.fits(b"<html>%PDF-"));
        assert!(DocKind::Markdown.fits("# Rules\n10 stars = $1 ✓".as_bytes()));
        assert!(!DocKind::Text.fits(b"\xFF\xD8\xFF\xE0"), "not UTF-8");
        assert!(!DocKind::Csv.fits(b"a,b\0c"), "a NUL");
        assert_eq!(DocKind::Markdown.mime(), "text/plain; charset=utf-8");
    }

    #[test]
    fn upload_kinds_have_distinct_extensions() {
        let exts: Vec<&str> = UploadKind::all().map(UploadKind::ext).collect();
        let mut unique = exts.clone();
        unique.sort();
        unique.dedup();
        assert_eq!((exts.len(), unique.len()), (9, 9), "{exts:?}");
    }

    #[test]
    fn links_must_be_web_and_notes_not_blank() {
        let link = |url: &str| Reference::Link {
            url: url.into(),
            title: None,
        };
        let note = |t: &str| Reference::Note { text: t.into() };
        assert_eq!(
            check_references(&[link("https://example.com/a?b=1"), note("x")]),
            Ok(())
        );
        for bad in [
            "javascript:alert(1)",
            "ftp://x",
            "https://a b",
            "example.com",
            "https://",
            "https:///x",
        ] {
            assert!(check_references(&[link(bad)]).is_err(), "{bad}");
        }
        assert!(check_references(&[note("  \n")]).is_err());
        assert!(check_references(&[note("a\x1b[31m")]).is_err());
        assert_eq!(check_references(&[note("a\n\tb")]), Ok(()));
        assert_eq!(check_references(&[link("HTTP://Example.com")]), Ok(()));
        let titled = Reference::Link {
            url: "https://x".into(),
            title: Some("two\nlines".into()),
        };
        assert!(check_references(&[titled]).is_err());
        let many = vec![note("x"); MAX_REFERENCES + 1];
        assert!(check_references(&many).unwrap_err().contains("at most"));
    }

    #[test]
    fn references_read_as_a_list_with_notes_quoted() {
        assert_eq!(references_text(&[]), "");
        let refs = [
            Reference::Link {
                url: "https://example.com/habit-grid".into(),
                title: Some("Habit grid".into()),
            },
            Reference::Link {
                url: "https://example.com/x".into(),
                title: Some(" ".into()),
            },
            Reference::Note {
                text: "Stars for each chore.\n10 stars = $1.\n".into(),
            },
        ];
        assert_eq!(
            references_text(&refs),
            "References:\n- Link: Habit grid <https://example.com/habit-grid>\n\
             - Link: <https://example.com/x>\n- Note:\n  > Stars for each chore.\n  > 10 stars = $1."
        );
    }

    #[test]
    fn references_are_tagged_by_kind() {
        let r: Reference = serde_json::from_str(r#"{"kind":"link","url":"https://x"}"#).unwrap();
        assert_eq!(
            r,
            Reference::Link {
                url: "https://x".into(),
                title: None
            }
        );
        let r: Reference = serde_json::from_str(r#"{"kind":"note","text":"hi"}"#).unwrap();
        assert_eq!(r, Reference::Note { text: "hi".into() });
    }
}

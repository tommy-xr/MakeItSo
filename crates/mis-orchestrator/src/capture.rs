//! Links, text and images shared from a phone, kept in an inbox as
//! `<dir>/YYYY-MM-DD-<slug>.md` plus its images (`…-1.jpg`, …). They are human
//! input: never swept or edited. Checks, naming and the file format are pure;
//! `Inbox` is the filesystem shell around them.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use mis_types::{ImageKind, MAX_IMAGES, MAX_IMAGE_BYTES};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A resend of the same capture within this long gets the first one back.
pub const DEDUPE_WINDOW: Duration = Duration::from_secs(10 * 60);
pub const MAX_TEXT_BYTES: usize = 64 << 10;
pub const MAX_URL_BYTES: usize = 4 << 10;
const MAX_VIA_CHARS: usize = 32;
const SLUG_WORDS: usize = 6;
const SLUG_CHARS: usize = 60;

/// One capture, checked: at least one of url, text, image.
#[derive(Debug)]
pub struct Capture {
    /// `http(s)` URL as sent, trimmed.
    pub url: Option<String>,
    /// As sent, trimmed.
    pub text: Option<String>,
    /// What the share sheet handed over, as text, when it adds to `url`: e.g. a shared
    /// page's text, or a link Shortcuts couldn't type as a URL.
    pub shared: Option<String>,
    pub images: Vec<(ImageKind, Vec<u8>)>,
    /// Each image's SHA-256, hex.
    hashes: Vec<String>,
    /// `[a-z0-9-]`: where it came from, e.g. `shortcut`.
    pub via: String,
}

/// Why a capture is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Invalid(String),
    TooLarge(String),
    NotAnImage(String),
}

impl Capture {
    /// Check form fields. A Shortcut sends every field, so blank ones and empty images
    /// count as absent. Of several URLs (one per line) the first is kept. Images are told
    /// apart by magic bytes (JPEG, PNG, WebP, GIF); `via` defaults to `shortcut`.
    /// Without `url`, the first http(s) link in `shared` is taken as it.
    pub fn new(
        url: Option<String>,
        text: Option<String>,
        shared: Option<String>,
        images: Vec<Vec<u8>>,
        via: Option<String>,
    ) -> Result<Self, Refused> {
        let trimmed = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let url =
            url.and_then(|u| trimmed(u.lines().find(|l| !l.trim().is_empty()).map(Into::into)));
        if let Some(u) = &url {
            if u.len() > MAX_URL_BYTES {
                return Err(Refused::TooLarge(format!(
                    "url is limited to {MAX_URL_BYTES} bytes"
                )));
            }
            match reqwest::Url::parse(u) {
                Ok(p) if matches!(p.scheme(), "http" | "https") => {}
                _ => return Err(Refused::Invalid("url must be http(s)".into())),
            }
        }
        let (text, shared) = (trimmed(text), trimmed(shared));
        if [&text, &shared]
            .iter()
            .any(|t| t.as_ref().is_some_and(|t| t.len() > MAX_TEXT_BYTES))
        {
            return Err(Refused::TooLarge(format!(
                "text is limited to {} KB",
                MAX_TEXT_BYTES >> 10
            )));
        }
        let url = url.or_else(|| shared.as_deref().and_then(first_link));
        // Kept only when it says more than the link taken from it.
        let shared = shared.filter(|s| Some(s) != url.as_ref());
        let images: Vec<_> = images.into_iter().filter(|b| !b.is_empty()).collect();
        if images.len() > MAX_IMAGES {
            return Err(Refused::Invalid(format!("at most {MAX_IMAGES} images")));
        }
        let images = (images.into_iter().enumerate())
            .map(|(i, bytes)| {
                if bytes.len() > MAX_IMAGE_BYTES {
                    let why = format!("images are limited to {} MB", MAX_IMAGE_BYTES >> 20);
                    return Err(Refused::TooLarge(why));
                }
                match ImageKind::sniff(&bytes) {
                    Some(kind) => Ok((kind, bytes)),
                    None => Err(Refused::NotAnImage(format!(
                        "image {} is not JPEG, PNG, WebP or GIF",
                        i + 1
                    ))),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let via = trimmed(via).map_or_else(|| "shortcut".into(), |v| v.to_ascii_lowercase());
        let token = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if via.len() > MAX_VIA_CHARS || !via.chars().all(token) {
            return Err(Refused::Invalid(format!(
                "via is 1-{MAX_VIA_CHARS} of a-z, 0-9, -"
            )));
        }
        if url.is_none() && text.is_none() && shared.is_none() && images.is_empty() {
            return Err(Refused::Invalid("send a url, text or image".into()));
        }
        Ok(Self {
            url,
            text,
            shared,
            hashes: images.iter().map(|(_, b)| sha256(b)).collect(),
            images,
            via,
        })
    }

    /// Same url, text, shared text and images: the same capture.
    pub fn key(&self) -> String {
        key(
            self.url.as_deref(),
            self.text.as_deref(),
            self.shared.as_deref(),
            &self.hashes,
        )
    }
}

/// The first whitespace-separated http(s) URL in `s`, e.g. `see https://a.b/c ok` -> `https://a.b/c`.
fn first_link(s: &str) -> Option<String> {
    s.split_whitespace()
        .filter(|w| w.len() <= MAX_URL_BYTES)
        .find(|w| reqwest::Url::parse(w).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
        .map(Into::into)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn key(url: Option<&str>, text: Option<&str>, shared: Option<&str>, hashes: &[String]) -> String {
    sha256(&serde_json::to_vec(&(url, text, shared, hashes)).expect("serializes"))
}

/// Lowercase ASCII words of `s`, apostrophes dropped: "Don't panic!" -> dont, panic.
fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_ascii_alphanumeric() && c != '\'' && c != '’')
        .map(|w| w.replace(['\'', '’'], "").to_ascii_lowercase())
        .filter(|w| !w.is_empty())
}

/// Up to [`SLUG_WORDS`] words, hyphenated and capped; empty if none.
fn hyphenate(words: impl Iterator<Item = String>) -> String {
    let mut slug = String::new();
    for w in words.take(SLUG_WORDS) {
        if slug.len() + usize::from(!slug.is_empty()) + w.len() > SLUG_CHARS {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&w);
    }
    slug
}

/// `#project` tag, if `word` is one: `#amf,` -> `amf`.
fn tag(word: &str) -> Option<String> {
    let t = word
        .strip_prefix('#')?
        .trim_end_matches(|c: char| c.is_ascii_punctuation());
    let t = t.to_ascii_lowercase();
    let ok = t.starts_with(|c: char| c.is_ascii_alphanumeric())
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then_some(t)
}

/// The first `#slug` in `text`.
pub fn project(text: &str) -> Option<String> {
    text.split_whitespace().find_map(tag)
}

/// File-name slug: the text's first words (tags left out), else the URL's host and
/// last path segment, else `capture`. "liminal inspiration #amf" -> `liminal-inspiration`.
pub fn slug(text: Option<&str>, url: Option<&str>) -> String {
    let from_text = text.map(|t| {
        let untagged = t.split_whitespace().filter(|w| !w.starts_with('#'));
        hyphenate(untagged.flat_map(words))
    });
    let from_url = || {
        let u = reqwest::Url::parse(url?).ok()?;
        let host = u.host_str()?.trim_start_matches("www.");
        let last = u
            .path_segments()?
            .rev()
            .find(|s| !s.is_empty())
            .unwrap_or("");
        Some(hyphenate(words(&format!("{host} {last}"))))
    };
    [from_text, from_url()]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "capture".into())
}

/// Whether a file in `existing` already uses `stem`: `stem.md`, or an image
/// `stem-1.jpg`. Also taken by `stem-2.jpg`, so `stem-2.md` never sits beside
/// another capture's image of that name. Case-insensitive, like macOS file names.
fn taken(stem: &str, existing: &[String]) -> bool {
    existing.iter().any(|name| {
        let base = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
        base == stem
            || base
                .strip_prefix(stem)
                .and_then(|r| r.strip_prefix('-'))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// `base`, else `base-2`, `base-3`, … whichever `existing` doesn't use.
fn free_stem(base: &str, existing: &[String]) -> String {
    std::iter::once(base.to_string())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|s| !taken(s, existing))
        .expect("unbounded")
}

/// Files to write for one capture.
#[derive(Debug)]
pub struct Plan {
    pub id: Uuid,
    /// `YYYY-MM-DD-<slug>.md`.
    pub file: String,
    pub md: String,
    /// One file name per image, in order.
    pub assets: Vec<String>,
}

/// Lay out capture `c` as `id`, received at `now` on local `date`, among the
/// inbox's `existing` file names.
pub fn plan(
    c: &Capture,
    id: Uuid,
    now: DateTime<Utc>,
    date: NaiveDate,
    existing: &[String],
) -> Plan {
    let base = format!(
        "{}-{}",
        date.format("%Y-%m-%d"),
        slug(c.text.as_deref(), c.url.as_deref())
    );
    let stem = free_stem(&base, existing);
    let assets: Vec<String> = (c.images.iter().enumerate())
        .map(|(i, (kind, _))| format!("{stem}-{}.{}", i + 1, kind.ext()))
        .collect();
    // JSON strings are YAML double-quoted scalars: `#`, `: `, `-`, `null`, `123` stay strings.
    let q = |s: &str| serde_json::to_string(s).expect("serializes");
    let mut md = format!(
        "---\nschema: 1\nid: {id}\ncaptured: {}\nvia: {}\n",
        now.to_rfc3339_opts(SecondsFormat::Secs, true),
        q(&c.via)
    );
    if let Some(u) = &c.url {
        md += &format!("url: {}\n", q(u));
    }
    if let Some(s) = &c.shared {
        md += &format!("shared: {}\n", q(s));
    }
    if let Some(p) = c.text.as_deref().and_then(project) {
        md += &format!("project: {}\n", q(&p));
    }
    md += if assets.is_empty() {
        "assets: []\n"
    } else {
        "assets:\n"
    };
    for (name, hash) in assets.iter().zip(&c.hashes) {
        md += &format!("  - path: {name}\n    sha256: {hash}\n");
    }
    md += "---\n";
    md += c.text.as_deref().unwrap_or("");
    Plan {
        id,
        file: format!("{stem}.md"),
        md,
        assets,
    }
}

/// What dedupe needs from a kept capture.
#[derive(Debug, PartialEq, Eq)]
pub struct Kept {
    pub id: Uuid,
    pub captured: DateTime<Utc>,
    pub key: String,
}

/// Read back a capture [`plan`] wrote; None if it isn't one.
pub fn parse(md: &str) -> Option<Kept> {
    let rest = md.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let (front, body) = (&rest[..end + 1], &rest[end + 5..]);
    let (mut id, mut captured, mut url, mut shared) = (None, None, None, None);
    let mut hashes = Vec::new();
    for line in front.lines() {
        if let Some(v) = line.strip_prefix("id: ") {
            id = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("captured: ") {
            captured = DateTime::parse_from_rfc3339(v).ok();
        } else if let Some(v) = line.strip_prefix("url: ") {
            url = Some(serde_json::from_str::<String>(v).ok()?);
        } else if let Some(v) = line.strip_prefix("shared: ") {
            shared = Some(serde_json::from_str::<String>(v).ok()?);
        } else if let Some(v) = line.strip_prefix("    sha256: ") {
            hashes.push(v.to_string());
        }
    }
    let text = (!body.is_empty()).then_some(body);
    Some(Kept {
        id: id?,
        captured: captured?.to_utc(),
        key: key(url.as_deref(), text, shared.as_deref(), &hashes),
    })
}

/// The kept capture `key` repeats, if one came within [`DEDUPE_WINDOW`] of `now`.
pub fn duplicate<'a>(
    key: &str,
    now: DateTime<Utc>,
    kept: &'a [(String, Kept)],
) -> Option<&'a (String, Kept)> {
    let window = chrono::Duration::from_std(DEDUPE_WINDOW).expect("fits");
    kept.iter()
        .find(|(_, k)| k.key == key && (now - k.captured).abs() <= window)
}

/// Where a capture was kept.
#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    pub id: Uuid,
    /// The `.md` file's name in the inbox.
    pub file: String,
    /// False: a resend, answered with the first one.
    pub new: bool,
}

/// Make renames into `dir` survive a power cut (Unix; elsewhere a no-op).
fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

pub struct Inbox {
    dir: PathBuf,
    /// Held while choosing names and writing, so concurrent captures can't collide.
    lock: Mutex<()>,
}

impl Inbox {
    /// Use `dir`, creating it readable only by this user.
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        crate::images::private_dir(&dir)?;
        Ok(Self {
            dir,
            lock: Mutex::new(()),
        })
    }

    /// Keep `c` (images first, the `.md` last; each written atomically and
    /// synced) unless it repeats a recent capture.
    pub fn store(&self, c: &Capture) -> std::io::Result<Saved> {
        let _held = self.lock.lock().unwrap();
        let now = Utc::now();
        let mut names = Vec::new();
        let mut recent = Vec::new();
        let since = SystemTime::now() - DEDUPE_WINDOW - Duration::from_secs(60);
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let fresh = (entry.metadata().and_then(|m| m.modified())).is_ok_and(|t| t >= since);
            if fresh && name.ends_with(".md") {
                let kept = std::fs::read_to_string(entry.path()).ok();
                if let Some(k) = kept.as_deref().and_then(parse) {
                    recent.push((name.clone(), k));
                }
            }
            names.push(name);
        }
        if let Some((file, k)) = duplicate(&c.key(), now, &recent) {
            return Ok(Saved {
                id: k.id,
                file: file.clone(),
                new: false,
            });
        }
        let date = chrono::Local::now().date_naive();
        let p = plan(c, Uuid::new_v4(), now, date, &names);
        for (name, (_, bytes)) in p.assets.iter().zip(&c.images) {
            crate::push::write_private(&self.dir.join(name), bytes)?;
        }
        // Images are durable before the `.md` that names them, and it before the reply.
        sync_dir(&self.dir)?;
        crate::push::write_private(&self.dir.join(&p.file), p.md.as_bytes())?;
        sync_dir(&self.dir)?;
        Ok(Saved {
            id: p.id,
            file: p.file,
            new: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0 rest";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n rest";

    fn cap(url: Option<&str>, text: Option<&str>, images: &[&[u8]]) -> Capture {
        let images = images.iter().map(|b| b.to_vec()).collect();
        Capture::new(
            url.map(Into::into),
            text.map(Into::into),
            None,
            images,
            None,
        )
        .unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-08T20:15:00Z")
            .unwrap()
            .to_utc()
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 8).unwrap()
    }

    fn names(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn slug_from_text_then_url_then_fallback() {
        let s = |t: Option<&str>, u: Option<&str>| slug(t, u);
        assert_eq!(
            s(Some("liminal inspiration for amf"), None),
            "liminal-inspiration-for-amf"
        );
        assert_eq!(
            s(Some("Don't PANIC: one two three four five six seven"), None),
            "dont-panic-one-two-three-four"
        );
        assert_eq!(
            s(Some("#amf liminal  spaces! #art"), None),
            "liminal-spaces"
        );
        assert_eq!(s(Some("café über"), None), "caf-ber");
        let url = Some("https://www.reddit.com/r/LiminalSpace/comments/abc/empty_mall/");
        assert_eq!(s(None, url), "reddit-com-empty-mall");
        assert_eq!(
            s(Some("#amf"), url),
            "reddit-com-empty-mall",
            "only tags: url"
        );
        assert_eq!(
            s(Some("日本語"), Some("https://example.com")),
            "example-com"
        );
        assert_eq!(s(None, None), "capture");
        assert_eq!(s(Some("…"), None), "capture");
        let long = "a".repeat(50) + " " + &"b".repeat(20);
        assert_eq!(s(Some(&long), None), "a".repeat(50), "capped");
        assert_eq!(
            s(Some(&"c".repeat(60)), None),
            "c".repeat(60),
            "one full-length word"
        );
    }

    #[test]
    fn project_is_the_first_valid_tag() {
        assert_eq!(project("see #AMF, and #other").as_deref(), Some("amf"));
        assert_eq!(
            project("# heading #bad_tag #ok-2.").as_deref(),
            Some("ok-2")
        );
        assert_eq!(project("https://x.com/#frag no tags"), None);
        assert_eq!(project("#-x #"), None);
    }

    #[test]
    fn takes_the_link_from_shared_text_and_keeps_the_rest() {
        let new = |url: Option<&str>, shared: &str| {
            Capture::new(url.map(Into::into), None, Some(shared.into()), vec![], None).unwrap()
        };
        // What iOS Shortcuts gives for a page shared from Chrome: the bare link.
        let c = new(None, " https://www.example.com/news/a-b \n");
        assert_eq!(c.url.as_deref(), Some("https://www.example.com/news/a-b"));
        assert_eq!(c.shared, None);
        let c = new(None, "Read this: https://x.com/a and ftp://y.z");
        assert_eq!(c.url.as_deref(), Some("https://x.com/a"));
        assert_eq!(
            c.shared.as_deref(),
            Some("Read this: https://x.com/a and ftp://y.z")
        );
        // No link: kept as shared text alone. A url field wins over shared.
        let c = new(None, "just words");
        assert_eq!((c.url, c.shared.as_deref()), (None, Some("just words")));
        let c = new(Some("https://a.com"), "https://b.com");
        assert_eq!(c.url.as_deref(), Some("https://a.com"));
        assert_ne!(new(None, "one").key(), new(None, "two").key());
        let md = plan(
            &new(None, "see https://x.com/a"),
            Uuid::nil(),
            now(),
            date(),
            &[],
        )
        .md;
        assert!(
            md.contains("url: \"https://x.com/a\"\nshared: \"see https://x.com/a\"\n"),
            "{md}"
        );
        assert_eq!(
            parse(&md).unwrap().key,
            new(None, "see https://x.com/a").key()
        );
    }

    #[test]
    fn checks_fields() {
        let new =
            |url: Option<&str>, text: Option<&str>, images: Vec<Vec<u8>>, via: Option<&str>| {
                Capture::new(
                    url.map(Into::into),
                    text.map(Into::into),
                    None,
                    images,
                    via.map(Into::into),
                )
            };
        let invalid = |r: Result<Capture, Refused>| matches!(r, Err(Refused::Invalid(_)));
        assert!(invalid(new(None, None, vec![], None)));
        assert!(
            invalid(new(Some("  "), Some(" \n"), vec![], None)),
            "blank is absent"
        );
        assert!(invalid(new(
            Some("javascript:alert(1)"),
            None,
            vec![],
            None
        )));
        assert!(invalid(new(Some("ftp://x.com/a"), None, vec![], None)));
        assert!(invalid(new(None, Some("t"), vec![], Some("short cut"))));
        let via = new(None, Some("t"), vec![], Some(" iPhone ")).unwrap().via;
        assert_eq!(via, "iphone");
        assert!(
            invalid(new(None, None, vec![vec![]], Some(" "))),
            "empty image is absent"
        );
        assert!(invalid(new(None, None, vec![JPEG.to_vec(); 5], None)));
        let svg = new(None, None, vec![b"<svg xmlns=".to_vec()], None);
        assert_eq!(
            svg.unwrap_err(),
            Refused::NotAnImage("image 1 is not JPEG, PNG, WebP or GIF".into())
        );
        let mut big = JPEG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(matches!(
            new(None, None, vec![big], None),
            Err(Refused::TooLarge(_))
        ));
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(matches!(
            new(None, Some(&long), vec![], None),
            Err(Refused::TooLarge(_))
        ));
        let c = new(
            Some("\n https://x.com/a \nhttps://y.com/b"),
            Some(" keep  me \n"),
            vec![vec![], PNG.to_vec()],
            Some(" "),
        )
        .unwrap();
        assert_eq!(c.url.as_deref(), Some("https://x.com/a"), "first url");
        assert_eq!(c.text.as_deref(), Some("keep  me"), "trimmed");
        assert_eq!(c.via, "shortcut");
        assert_eq!(c.images.len(), 1);
        assert_eq!(c.images[0].0, ImageKind::Png);
    }

    #[test]
    fn plan_renders_frontmatter_and_names_assets() {
        let id = Uuid::nil();
        let c = cap(
            Some("https://x.com/a#b: c"),
            Some("liminal inspiration for amf #amf\nmore"),
            &[JPEG, PNG],
        );
        let p = plan(&c, id, now(), date(), &[]);
        assert_eq!(p.file, "2026-11-08-liminal-inspiration-for-amf-more.md");
        assert_eq!(
            p.assets,
            [
                "2026-11-08-liminal-inspiration-for-amf-more-1.jpg",
                "2026-11-08-liminal-inspiration-for-amf-more-2.png"
            ]
        );
        let expected = format!(
            "---\nschema: 1\nid: {id}\ncaptured: 2026-11-08T20:15:00Z\nvia: \"shortcut\"\n\
             url: \"https://x.com/a#b: c\"\nproject: \"amf\"\nassets:\n\
             \x20 - path: {}\n    sha256: {}\n\
             \x20 - path: {}\n    sha256: {}\n\
             ---\nliminal inspiration for amf #amf\nmore",
            p.assets[0],
            sha256(JPEG),
            p.assets[1],
            sha256(PNG)
        );
        assert_eq!(p.md, expected);
        let bare = plan(&cap(None, None, &[JPEG]), id, now(), date(), &[]);
        assert_eq!(bare.file, "2026-11-08-capture.md");
        assert!(!bare.md.contains("url:") && !bare.md.contains("project:"));
        assert!(bare.md.ends_with("---\n"));
        let link = plan(
            &cap(Some("https://x.com"), None, &[]),
            id,
            now(),
            date(),
            &[],
        );
        assert!(link.md.contains("assets: []\n"), "{}", link.md);
    }

    #[test]
    fn collisions_get_a_number() {
        let c = cap(None, Some("foo"), &[JPEG]);
        let file = |existing: &[&str]| plan(&c, Uuid::nil(), now(), date(), &names(existing)).file;
        assert_eq!(file(&[]), "2026-11-08-foo.md");
        assert_eq!(file(&["2026-11-08-foo.md"]), "2026-11-08-foo-2.md");
        // Another capture's second image holds `-2`.
        let two = [
            "2026-11-08-foo.md",
            "2026-11-08-foo-1.jpg",
            "2026-11-08-foo-2.jpg",
        ];
        assert_eq!(file(&two), "2026-11-08-foo-3.md");
        // A leftover temp file holds its stem too, and so does another case.
        assert_eq!(file(&["2026-11-08-Foo.md"]), "2026-11-08-foo-2.md");
        assert_eq!(file(&["2026-11-08-foo.tmp"]), "2026-11-08-foo-2.md");
        // Longer slugs and other days don't.
        assert_eq!(
            file(&["2026-11-08-foo-bar.md", "2026-11-07-foo.md"]),
            "2026-11-08-foo.md"
        );
        let p = plan(
            &c,
            Uuid::nil(),
            now(),
            date(),
            &names(&["2026-11-08-foo.md"]),
        );
        assert_eq!(p.assets, ["2026-11-08-foo-2-1.jpg"]);
    }

    #[test]
    fn parse_reads_back_the_dedupe_key() {
        for c in [
            cap(
                Some("https://x.com/\"q\""),
                Some("t #p\n\n---\nafter"),
                &[JPEG, PNG],
            ),
            cap(None, None, &[JPEG]),
            cap(Some("https://x.com"), None, &[]),
        ] {
            let id = Uuid::new_v4();
            let p = plan(&c, id, now(), date(), &[]);
            let kept = parse(&p.md).unwrap();
            assert_eq!(
                kept,
                Kept {
                    id,
                    captured: now(),
                    key: c.key()
                }
            );
        }
        assert_eq!(parse("no frontmatter"), None);
        assert_eq!(parse("---\nschema: 1\n---\n"), None, "no id");
    }

    #[test]
    fn dedupes_same_content_within_the_window() {
        let c = cap(Some("https://x.com"), Some("t"), &[JPEG]);
        let at = |mins: i64| now() + chrono::Duration::minutes(mins);
        let kept = vec![(
            "f.md".to_string(),
            Kept {
                id: Uuid::nil(),
                captured: now(),
                key: c.key(),
            },
        )];
        assert_eq!(duplicate(&c.key(), at(10), &kept).unwrap().0, "f.md");
        assert!(
            duplicate(&c.key(), at(11), &kept).is_none(),
            "window passed"
        );
        for other in [
            cap(Some("https://x.com"), Some("t"), &[PNG]),
            cap(Some("https://x.com"), Some("t2"), &[JPEG]),
            cap(Some("https://y.com"), Some("t"), &[JPEG]),
            cap(None, Some("t"), &[JPEG]),
        ] {
            assert!(duplicate(&other.key(), at(1), &kept).is_none(), "{other:?}");
        }
    }

    #[test]
    fn inbox_keeps_captures_and_answers_resends() {
        let dir = std::env::temp_dir().join(format!("mis-capture-test-{}", Uuid::new_v4()));
        let inbox = Inbox::open(dir.clone()).unwrap();
        let c = cap(None, Some("liminal inspiration for amf"), &[JPEG]);
        let first = inbox.store(&c).unwrap();
        assert!(first.new);
        assert!(
            first.file.ends_with("-liminal-inspiration-for-amf.md"),
            "{}",
            first.file
        );
        let again = inbox.store(&c).unwrap();
        assert_eq!(
            (again.id, &again.file, again.new),
            (first.id, &first.file, false)
        );
        let other = inbox
            .store(&cap(None, Some("liminal inspiration for amf"), &[PNG]))
            .unwrap();
        assert!(
            other.new && other.file.ends_with("-amf-2.md"),
            "{}",
            other.file
        );
        let mut files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files.len(), 4, "{files:?}");
        let md = std::fs::read_to_string(dir.join(&first.file)).unwrap();
        assert_eq!(parse(&md).unwrap().id, first.id);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

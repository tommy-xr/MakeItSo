//! Reading the notes clone for the Notes view. Which paths may be read, what each is, the
//! listing and how a file is served are pure; [`tree`] and [`read`] are the filesystem shell.

use std::path::Path;

use mis_types::{ImageKind, NoteEntry, NoteKind};

/// The folders shown, in menu order. Nothing else in the repo is (`apps/`, `.git`, files at
/// the top).
pub const FOLDERS: [&str; 6] = ["inbox", "projects", "areas", "resources", "archive", "ai"];

/// Whether `path` may be read: a [`crate::apps::clean_path`] (relative, no empty, hidden,
/// `.` or `..` segment, no `\`, `:` or NUL) under one of [`FOLDERS`].
pub fn allowed(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    FOLDERS.contains(&first) && crate::apps::clean_path(path).is_some()
}

/// `name`'s kind, by its extension: the listing never reads a file.
pub fn kind(name: &str, dir: bool) -> NoteKind {
    if dir {
        return NoteKind::Dir;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("md") => NoteKind::Md,
        Some("jpeg") => NoteKind::Image,
        Some(e) if ImageKind::from_ext(e).is_some() => NoteKind::Image,
        _ => NoteKind::Other,
    }
}

/// One entry a walk of the clone found.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: String,
    pub dir: bool,
    pub symlink: bool,
    pub size: u64,
}

/// What the view lists of `found`: readable paths only, no symlinks, sorted by path.
pub fn listing(found: Vec<Found>) -> Vec<NoteEntry> {
    let mut entries: Vec<NoteEntry> = (found.into_iter())
        .filter(|f| !f.symlink && allowed(&f.path))
        .map(|f| NoteEntry {
            kind: kind(&f.path, f.dir),
            size: if f.dir { 0 } else { f.size },
            path: f.path,
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// The `Content-Type` file `path` (`bytes`) is served with: Markdown as text, an image as
/// the format its magic bytes say. None for anything else, so no HTML or SVG is ever served.
pub fn content_type(path: &str, bytes: &[u8]) -> Option<&'static str> {
    match kind(path, false) {
        NoteKind::Md => Some("text/markdown; charset=utf-8"),
        NoteKind::Image => ImageKind::sniff(bytes).map(ImageKind::mime),
        NoteKind::Dir | NoteKind::Other => None,
    }
}

/// Every entry under [`FOLDERS`] in `clone`; empty before the first clone. Symlinks are
/// listed (for [`listing`] to drop) but never followed. A dir that vanishes mid-walk (a
/// rebase) is skipped.
pub fn tree(clone: &Path) -> Vec<NoteEntry> {
    let mut found = Vec::new();
    let mut dirs = Vec::new();
    for f in FOLDERS {
        if let Ok(m) = std::fs::symlink_metadata(clone.join(f)) {
            let (dir, symlink) = (m.is_dir(), m.is_symlink());
            found.push(Found {
                path: f.into(),
                dir,
                symlink,
                size: 0,
            });
            if dir {
                dirs.push(f.to_string());
            }
        }
    }
    while let Some(rel) = dirs.pop() {
        let Ok(read) = std::fs::read_dir(clone.join(&rel)) else {
            continue;
        };
        for e in read.flatten() {
            let (Some(name), Ok(t)) = (e.file_name().to_str().map(String::from), e.file_type())
            else {
                continue;
            };
            let path = format!("{rel}/{name}");
            let size = if t.is_file() {
                e.metadata().map_or(0, |m| m.len())
            } else {
                0
            };
            if t.is_dir() && allowed(&path) {
                dirs.push(path.clone());
            }
            found.push(Found {
                path,
                dir: t.is_dir(),
                symlink: t.is_symlink(),
                size,
            });
        }
    }
    listing(found)
}

/// Why a file isn't served.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not an allowed path, not a file, or reached through a symlink.
    NotFound,
    /// Neither Markdown nor an image whose bytes say so.
    Unsupported,
}

/// File `path` in `clone`, with its `Content-Type`. Read as an app's files are: a regular
/// file, no symlink anywhere below the clone, at most [`crate::apps::MAX_BYTES`].
pub fn read(clone: &Path, path: &str) -> Result<(&'static str, Vec<u8>), Refused> {
    if !allowed(path) {
        return Err(Refused::NotFound);
    }
    let bytes = crate::apps::read_file(clone, path).ok_or(Refused::NotFound)?;
    let ty = content_type(path, &bytes).ok_or(Refused::Unsupported)?;
    Ok((ty, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_relative_paths_under_the_shown_folders_are_allowed() {
        for ok in [
            "inbox",
            "inbox/2026-11-08-a.md",
            "ai/projects/amf/x.md",
            "archive/old/README.md",
            "projects/a b/c.png",
        ] {
            assert!(allowed(ok), "{ok}");
        }
        for no in [
            "",
            "/inbox/a.md",
            "inbox/../apps/x.html",
            "inbox/./a.md",
            "inbox/..",
            "..",
            "inbox//a.md",
            "inbox/",
            "inbox/.git/config",
            ".git/config",
            "apps/todo/index.html",
            "README.md",
            "inbox\\..\\apps",
            "inbox/a\0.md",
            "Inbox/a.md",
            "C:/inbox/a.md",
            "inbox/a.md:stream",
        ] {
            assert!(!allowed(no), "{no:?}");
        }
    }

    #[test]
    fn kinds_go_by_extension() {
        assert_eq!(kind("inbox", true), NoteKind::Dir);
        assert_eq!(kind("inbox/a.md", false), NoteKind::Md);
        assert_eq!(kind("inbox/a.MD", false), NoteKind::Md);
        for img in ["a.jpg", "a.JPEG", "a.png", "a.webp", "a.gif"] {
            assert_eq!(kind(img, false), NoteKind::Image, "{img}");
        }
        for other in ["a.svg", "a.html", "a.txt", "README", "a.md.bak"] {
            assert_eq!(kind(other, false), NoteKind::Other, "{other}");
        }
    }

    #[test]
    fn listing_drops_symlinks_and_hidden_or_unshown_paths() {
        let f = |path: &str, dir, symlink| Found {
            path: path.into(),
            dir,
            symlink,
            size: 7,
        };
        let got = listing(vec![
            f("inbox/b.md", false, false),
            f("inbox", true, false),
            f("inbox/.gitkeep", false, false),
            f("inbox/link.md", false, true),
            f("projects", true, true),
            f("apps/x.html", false, false),
            f("inbox/a-1.jpg", false, false),
        ]);
        let entry = |path: &str, kind, size| NoteEntry {
            path: path.into(),
            kind,
            size,
        };
        assert_eq!(
            got,
            [
                entry("inbox", NoteKind::Dir, 0),
                entry("inbox/a-1.jpg", NoteKind::Image, 7),
                entry("inbox/b.md", NoteKind::Md, 7),
            ]
        );
    }

    #[test]
    fn only_markdown_and_real_images_are_served() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        assert_eq!(
            content_type("inbox/a.md", b"<script>x</script>"),
            Some("text/markdown; charset=utf-8")
        );
        assert_eq!(content_type("inbox/a.png", png), Some("image/png"));
        // The bytes decide the type, not the name.
        assert_eq!(
            content_type("inbox/a.jpg", b"GIF89a\x01\0"),
            Some("image/gif")
        );
        assert_eq!(
            content_type("inbox/a.png", b"<html><script>x</script>"),
            None
        );
        assert_eq!(content_type("inbox/a.svg", b"<svg onload=x>"), None);
        assert_eq!(content_type("inbox/a.gif", b"<svg onload=x>"), None);
        assert_eq!(content_type("inbox/a.html", png), None);
        assert_eq!(content_type("inbox/a.txt", b"hi"), None);
    }

    #[test]
    #[cfg(unix)] // symlinks
    fn read_never_follows_a_symlink_out_of_the_clone() {
        let dir = std::env::temp_dir().join(format!("mis-notes-view-{}", uuid::Uuid::new_v4()));
        let clone = dir.join("notes");
        std::fs::create_dir_all(clone.join("inbox/sub")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("outside/secret.md"), "secret").unwrap();
        std::fs::write(clone.join("inbox/a.md"), "# a").unwrap();
        std::fs::write(clone.join("inbox/fake.png"), "<html>").unwrap();
        let link = |to: &Path, at: &str| std::os::unix::fs::symlink(to, clone.join(at)).unwrap();
        link(&dir.join("outside/secret.md"), "inbox/secret.md");
        link(&dir.join("outside"), "inbox/out");
        link(&dir.join("outside"), "projects");
        link(&clone.join("inbox/a.md"), "inbox/sub/same.md");
        assert_eq!(
            read(&clone, "inbox/a.md"),
            Ok(("text/markdown; charset=utf-8", b"# a".to_vec()))
        );
        for path in [
            "inbox/secret.md",
            "inbox/out/secret.md",
            "projects/secret.md",
            "inbox/sub/same.md",
            "inbox/../../outside/secret.md",
            "inbox/sub",
            "inbox/missing.md",
        ] {
            assert_eq!(read(&clone, path), Err(Refused::NotFound), "{path}");
        }
        assert_eq!(read(&clone, "inbox/fake.png"), Err(Refused::Unsupported));
        // The listing shows none of them, and doesn't walk into them.
        let paths: Vec<String> = tree(&clone).into_iter().map(|e| e.path).collect();
        assert_eq!(
            paths,
            ["inbox", "inbox/a.md", "inbox/fake.png", "inbox/sub"]
        );
        assert!(tree(&dir.join("none")).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

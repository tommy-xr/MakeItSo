//! Filing an inbox note in another folder of the notes repo: where it may go, the name it
//! takes there, the frontmatter it gains and the commit saying so. Pure; [`crate::notes`]
//! makes the move with `git`.

use mis_types::{NoteKind, NoteMove};

/// The longest summary a filing writes.
pub const MAX_SUMMARY_CHARS: usize = 200;

/// Where a note may be filed: the folders directly under `projects/`, `areas/` and
/// `resources/` among `dirs` (repo paths), and `archive`. Never `ai/`, which is the AI's.
pub fn dests(dirs: &[String]) -> Vec<String> {
    let one_down = |p: &&String| {
        let mut parts = p.split('/');
        let top = parts.next().unwrap_or_default();
        ["projects", "areas", "resources"].contains(&top)
            && parts.next().is_some()
            && parts.next().is_none()
            && crate::notes_view::allowed(p)
    };
    let mut dests: Vec<String> = dirs.iter().filter(one_down).cloned().collect();
    dests.push("archive".into());
    dests.sort();
    dests.dedup();
    dests
}

/// Why `m` can't be filed, before reading the repo: it names a note directly in `inbox/`,
/// and its summary is short.
pub fn check(m: &NoteMove) -> Result<(), String> {
    let name = m.path.strip_prefix("inbox/").unwrap_or_default();
    let note = crate::notes_view::kind(name, false) == NoteKind::Md;
    if !note || name.contains('/') || !crate::notes_view::allowed(&m.path) {
        return Err("path must be inbox/<name>.md".into());
    }
    if summary(m).is_some_and(|s| s.chars().count() > MAX_SUMMARY_CHARS) {
        return Err(format!(
            "summary is limited to {MAX_SUMMARY_CHARS} characters"
        ));
    }
    Ok(())
}

/// `m`'s summary, trimmed; None if blank.
fn summary(m: &NoteMove) -> Option<&str> {
    m.summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A move [`plan`] lays out.
#[derive(Debug, PartialEq, Eq)]
pub struct Filing {
    /// (from, to) repo paths: the note, then its images.
    pub moves: Vec<(String, String)>,
    /// The note's text at its new path.
    pub md: String,
    pub message: String,
}

/// Filing `m` (a [`check`]ed note whose text is `md`) in `m.dest`. Its images are the
/// `assets` its frontmatter lists that are in `inbox` (file names there). They keep the
/// note's name unless `taken` (names in `dest`) uses it: then `-2`, `-3`, …, as captures
/// are named. The frontmatter gains `summary:` and, filed in a project, `project: <id>`.
pub fn plan(m: &NoteMove, md: &str, inbox: &[String], taken: &[String]) -> Result<Filing, String> {
    let name = &m.path["inbox/".len()..];
    let (from, ext) = name.split_at(name.len() - ".md".len());
    let kept = crate::capture::parse(md);
    let images: Vec<&String> = (kept.iter().flat_map(|k| &k.assets))
        .filter(|a| a.as_str() != name && inbox.contains(a))
        .collect();
    let to = crate::capture::free_stem(from, taken);
    let mut moves = vec![(m.path.clone(), format!("{}/{to}{ext}", m.dest))];
    let mut renames = Vec::new();
    for a in images {
        let b = crate::notes::renamed(a, from, &to);
        // One not named after the note keeps its name, so may be taken.
        if taken.iter().any(|t| t.eq_ignore_ascii_case(&b)) {
            return Err(format!("{}/{b} exists", m.dest));
        }
        moves.push((format!("inbox/{a}"), format!("{}/{b}", m.dest)));
        renames.push((a.as_str(), b));
    }
    let project = m.dest.strip_prefix("projects/");
    let adds: Vec<(&str, &str)> = [("summary", summary(m)), ("project", project)]
        .into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect();
    let id = kept.as_ref().map_or("-".into(), |k| k.id.to_string());
    let message = format!(
        "file: {} -> {}/\n\nMIS-Writer: human\nMIS-Filed: {id} {} {}\n",
        m.path, m.dest, moves[0].0, moves[0].1
    );
    Ok(Filing {
        md: edit(md, &renames, &adds),
        moves,
        message,
    })
}

/// `md` with its frontmatter's image paths renamed and `adds` set (replacing those keys),
/// as JSON strings, which YAML reads as strings. A note without frontmatter gets one. The
/// text below it is the human's words: never touched.
fn edit(md: &str, renames: &[(&str, String)], adds: &[(&str, &str)]) -> String {
    if renames.is_empty() && adds.is_empty() {
        return md.to_string();
    }
    let (front, body) = split(md);
    let mut front = front.unwrap_or_default().to_string();
    for (a, b) in renames {
        let line = |name: &str| format!("  - path: {name}\n");
        front = front.replace(&line(a), &line(b));
    }
    for (key, value) in adds {
        let own = format!("{key}:");
        front = (front.lines())
            .filter(|l| !l.starts_with(&own))
            .map(|l| format!("{l}\n"))
            .collect();
        front += &format!(
            "{key}: {}\n",
            serde_json::to_string(value).expect("serializes")
        );
    }
    format!("---\n{front}---\n{body}")
}

/// `md`'s frontmatter lines (each ending in `\n`), if it has any, and the text after them.
fn split(md: &str) -> (Option<&str>, &str) {
    let Some(rest) = md.strip_prefix("---\n") else {
        return (None, md);
    };
    match rest.find("\n---\n") {
        Some(i) => (Some(&rest[..i + 1]), &rest[i + 5..]),
        None if rest.ends_with("\n---") => (Some(&rest[..rest.len() - 3]), ""),
        None => (None, md),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(path: &str, dest: &str, summary: Option<&str>) -> NoteMove {
        NoteMove {
            path: path.into(),
            dest: dest.into(),
            summary: summary.map(Into::into),
            hash: None,
        }
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    const CAPTURE: &str = "---\nschema: 1\nid: 00000000-0000-0000-0000-000000000001\n\
        captured: 2026-11-08T09:12:00Z\nvia: \"share\"\nproject: \"mis\"\nassets:\n  \
        - path: a-1.jpg\n    sha256: h\n  - path: a-2.png\n    sha256: h\n---\n\
        Body #mis\n  - path: a-1.jpg\nproject: body\n";

    #[test]
    fn dests_are_folders_one_down_and_the_archive_never_ai() {
        let dirs = names(&[
            "projects/amf",
            "projects/mis",
            "projects/mis/docs",
            "areas/home",
            "resources/rust",
            "ai/projects/amf",
            "archive/2025",
            "projects/.hidden",
            "inbox",
        ]);
        assert_eq!(
            dests(&dirs),
            [
                "archive",
                "areas/home",
                "projects/amf",
                "projects/mis",
                "resources/rust"
            ]
        );
        assert_eq!(dests(&[]), ["archive"]);
    }

    #[test]
    fn only_a_note_directly_in_the_inbox_with_a_short_summary_is_filed() {
        assert_eq!(check(&mv("inbox/a.md", "archive", None)), Ok(()));
        assert_eq!(check(&mv("inbox/A.MD", "archive", Some("  "))), Ok(()));
        for path in [
            "inbox/a-1.jpg",
            "inbox/sub/a.md",
            "projects/amf/a.md",
            "inbox/../a.md",
            "inbox/.a.md",
            "inbox/",
            "a.md",
            "",
        ] {
            assert!(check(&mv(path, "archive", None)).is_err(), "{path}");
        }
        let long = "x".repeat(MAX_SUMMARY_CHARS + 1);
        assert!(check(&mv("inbox/a.md", "archive", Some(&long))).is_err());
        let fits = "é".repeat(MAX_SUMMARY_CHARS);
        assert_eq!(check(&mv("inbox/a.md", "archive", Some(&fits))), Ok(()));
    }

    #[test]
    fn a_capture_moves_with_its_images_under_its_own_name() {
        let inbox = names(&["a.md", "a-1.jpg", "a-2.png", "b.md"]);
        let m = mv("inbox/a.md", "areas/home", None);
        let got = plan(&m, CAPTURE, &inbox, &names(&["boiler.md"])).unwrap();
        assert_eq!(
            got.moves,
            [
                ("inbox/a.md".into(), "areas/home/a.md".into()),
                ("inbox/a-1.jpg".into(), "areas/home/a-1.jpg".into()),
                ("inbox/a-2.png".into(), "areas/home/a-2.png".into()),
            ]
        );
        assert_eq!(got.md, CAPTURE, "nothing to add");
        assert_eq!(
            got.message,
            "file: inbox/a.md -> areas/home/\n\nMIS-Writer: human\n\
             MIS-Filed: 00000000-0000-0000-0000-000000000001 inbox/a.md areas/home/a.md\n"
        );
        // An image the human deleted stays deleted.
        let got = plan(&m, CAPTURE, &names(&["a.md", "a-2.png"]), &[]).unwrap();
        assert_eq!(got.moves.len(), 2);
        assert_eq!(got.moves[1].0, "inbox/a-2.png");
    }

    #[test]
    fn a_taken_name_gets_a_number_images_and_frontmatter_too() {
        let inbox = names(&["a.md", "a-1.jpg", "a-2.png"]);
        let m = mv("inbox/a.md", "projects/amf", Some("Pure cores, by example"));
        let got = plan(&m, CAPTURE, &inbox, &names(&["A.md", "README.md"])).unwrap();
        let to: Vec<&str> = got.moves.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            to,
            [
                "projects/amf/a-2.md",
                "projects/amf/a-2-1.jpg",
                "projects/amf/a-2-2.png"
            ]
        );
        // Images renamed, `project:` replaced and `summary:` added in the frontmatter; the
        // body, which repeats both, untouched.
        assert_eq!(
            got.md,
            "---\nschema: 1\nid: 00000000-0000-0000-0000-000000000001\n\
             captured: 2026-11-08T09:12:00Z\nvia: \"share\"\nassets:\n  \
             - path: a-2-1.jpg\n    sha256: h\n  - path: a-2-2.png\n    sha256: h\n\
             summary: \"Pure cores, by example\"\nproject: \"amf\"\n---\n\
             Body #mis\n  - path: a-1.jpg\nproject: body\n"
        );
        assert!(got.message.ends_with(" inbox/a.md projects/amf/a-2.md\n"));
        // An image of that name counts as taking it.
        let got = plan(&m, CAPTURE, &inbox, &names(&["a-1.webp"])).unwrap();
        assert_eq!(got.moves[0].1, "projects/amf/a-2.md");
    }

    #[test]
    fn a_humans_note_gets_frontmatter_only_when_it_needs_some() {
        let m = mv("inbox/Idea.md", "resources/rust", None);
        let got = plan(&m, "# Idea\n\n---\nnot frontmatter\n", &[], &[]).unwrap();
        assert_eq!(got.md, "# Idea\n\n---\nnot frontmatter\n");
        assert!(got
            .message
            .contains("\nMIS-Filed: - inbox/Idea.md resources/rust/Idea.md\n"));
        let m = mv(
            "inbox/Idea.md",
            "projects/amf",
            Some("A \"quoted\"\nthought"),
        );
        let got = plan(&m, "# Idea\n", &[], &names(&["idea.md"])).unwrap();
        assert_eq!(got.moves[0].1, "projects/amf/Idea-2.md", "any case");
        assert_eq!(
            got.md,
            "---\nsummary: \"A \\\"quoted\\\"\\nthought\"\nproject: \"amf\"\n---\n# Idea\n"
        );
        // Frontmatter ending the file; a `title:` kept.
        let got = plan(&m, "---\ntitle: x\n---", &[], &[]).unwrap();
        assert_eq!(
            got.md,
            "---\ntitle: x\nsummary: \"A \\\"quoted\\\"\\nthought\"\nproject: \"amf\"\n---\n"
        );
    }

    #[test]
    fn an_image_not_named_after_the_note_must_be_free_at_the_dest() {
        let md = "---\nid: 00000000-0000-0000-0000-000000000001\ncaptured: \
                  2026-11-08T09:12:00Z\nassets:\n  - path: photo.jpg\n---\n";
        let m = mv("inbox/renamed.md", "archive", None);
        let inbox = names(&["renamed.md", "photo.jpg"]);
        let got = plan(&m, md, &inbox, &[]).unwrap();
        assert_eq!(
            got.moves[1],
            ("inbox/photo.jpg".into(), "archive/photo.jpg".into())
        );
        assert_eq!(
            plan(&m, md, &inbox, &names(&["Photo.JPG"])),
            Err("archive/photo.jpg exists".into())
        );
    }
}

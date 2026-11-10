//! Filing an inbox note in another folder of the notes repo: where it may go, the name it
//! takes there, the frontmatter it gains and the commit saying so. Pure; [`crate::notes`]
//! makes the move with `git`.

use std::collections::BTreeMap;

use mis_types::{NoteKind, NoteMove};

/// The longest summary a filing writes.
pub const MAX_SUMMARY_CHARS: usize = 200;

/// HEAD's entries a filing looks at: repo path -> git mode.
pub type Modes = BTreeMap<String, String>;

/// A folder's git mode; a file's are [`FILES`]. Symlinks (`120000`) and submodules never move.
const TREE: &str = "040000";
const FILES: [&str; 2] = ["100644", "100755"];
/// Folders whose folders a note may be filed in, besides `archive`.
const PARENTS: [&str; 3] = ["projects", "areas", "resources"];

/// Where a note may be filed, from HEAD's `modes` of [`PARENTS`], `archive` and what's
/// directly in the first three: their folders, if they are folders too, and `archive` if
/// it is one or absent. Never `ai/`, which is the AI's.
pub fn dests(modes: &Modes) -> Vec<String> {
    let tree = |p: &str| modes.get(p).is_some_and(|m| m == TREE);
    let one_down = |p: &str| {
        p.split_once('/')
            .is_some_and(|(top, rest)| PARENTS.contains(&top) && tree(top) && !rest.contains('/'))
            && tree(p)
            && crate::notes_view::allowed(p)
    };
    let mut dests: Vec<String> = modes.keys().filter(|p| one_down(p)).cloned().collect();
    if modes.get("archive").is_none_or(|m| m == TREE) {
        dests.push("archive".into());
    }
    dests.sort();
    dests
}

/// Why `m` can't be filed, before reading the repo: it names a note directly in `inbox/`
/// and a folder [`dests`] could hold, neither with control characters (they would forge
/// commit trailers), and its summary is short.
pub fn check(m: &NoteMove) -> Result<(), String> {
    let plain = |p: &str| crate::notes_view::allowed(p) && !p.chars().any(char::is_control);
    let name = m.path.strip_prefix("inbox/").unwrap_or_default();
    let note = crate::notes_view::kind(name, false) == NoteKind::Md;
    if !note || name.contains('/') || !plain(&m.path) {
        return Err("path must be inbox/<name>.md".into());
    }
    let folder = m
        .dest
        .split_once('/')
        .is_some_and(|(top, rest)| PARENTS.contains(&top) && !rest.contains('/'));
    if !(m.dest == "archive" || folder) || !plain(&m.dest) {
        return Err("dest must be projects/<id>, areas/<id>, resources/<name> or archive".into());
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

/// Filing `m` (a [`check`]ed note whose text is `md`) in `m.dest`. `inbox` is HEAD's
/// `modes` of the inbox's files; `claimed` the pictures its other notes list; `taken` the
/// names in `dest`, committed or not. The note's pictures are the images its frontmatter
/// lists that are files beside it; one another note lists stops the move. All keep the
/// note's name unless a name is taken: then `-2`, `-3`, …, as captures are named. The
/// frontmatter gains `summary:` and, filed in a project, `project: <id>`.
pub fn plan(
    m: &NoteMove,
    md: &str,
    inbox: &Modes,
    claimed: &[String],
    taken: &[String],
) -> Result<Filing, String> {
    let file = |p: &str| inbox.get(p).is_some_and(|m| FILES.contains(&m.as_str()));
    if !file(&m.path) {
        return Err(format!("{} is not a file", m.path));
    }
    let name = &m.path["inbox/".len()..];
    let (from, ext) = name.split_at(name.len() - ".md".len());
    let listed = crate::capture::front(md).map(|f| crate::capture::assets(f.lines));
    let mut seen = std::collections::BTreeSet::new();
    let picture = |a: &&String| {
        crate::notes_view::kind(a, false) == NoteKind::Image
            && !a.contains('/')
            && !a.chars().any(char::is_control)
            && file(&format!("inbox/{a}"))
            && seen.insert(a.to_string())
    };
    let images: Vec<&String> = listed.iter().flatten().filter(picture).collect();
    if let Some(n) = std::iter::once(name)
        .chain(images.iter().map(|a| a.as_str()))
        .find(|n| decomposed(n))
    {
        let why =
            format!("inbox/{n} has a decomposed (NFD) name git on macOS can't move; rename it");
        return Err(why);
    }
    if let Some(a) = images.iter().find(|a| claimed.contains(a)) {
        return Err(format!("another inbox note shows inbox/{a} too"));
    }
    let used = |n: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    // One not named after the note keeps its name, so may be taken.
    if let Some(a) = images.iter().find(|a| !a.starts_with(from) && used(a)) {
        return Err(format!("{}/{a} exists", m.dest));
    }
    let free = |s: &String| {
        !crate::capture::taken(s, taken)
            && !used(&format!("{s}{ext}"))
            && !images
                .iter()
                .any(|a| used(&crate::notes::renamed(a, from, s)))
    };
    let to = std::iter::once(from.to_string())
        .chain((2..).map(|n| format!("{from}-{n}")))
        .find(free)
        .expect("unbounded");
    let mut moves = vec![(m.path.clone(), format!("{}/{to}{ext}", m.dest))];
    let mut renames = Vec::new();
    for a in images {
        let b = crate::notes::renamed(a, from, &to);
        moves.push((format!("inbox/{a}"), format!("{}/{b}", m.dest)));
        renames.push((a.as_str(), b));
    }
    let project = m.dest.strip_prefix("projects/");
    let adds: Vec<(&str, &str)> = [("summary", summary(m)), ("project", project)]
        .into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect();
    let id = crate::capture::parse(md).map_or("-".into(), |k| k.id.to_string());
    let message = format!(
        "file: {} -> {}/\n\nMIS-Writer: human\nMIS-Filed: {id} {} {}\n",
        m.path, m.dest, moves[0].0, moves[0].1
    );
    Ok(Filing {
        md: edit(md, &renames, &adds)?,
        moves,
        message,
    })
}

/// `md` with its frontmatter's image paths renamed and `adds` set, as JSON strings, which
/// YAML reads as strings. A note without frontmatter gets one; one never closed is refused.
/// Line endings are the note's. The text below is the human's words: never touched.
fn edit(md: &str, renames: &[(&str, String)], adds: &[(&str, &str)]) -> Result<String, String> {
    // A byte order mark stays first.
    let bom = if md.starts_with('\u{feff}') {
        "\u{feff}"
    } else {
        ""
    };
    let opens = md[bom.len()..].starts_with("---\n") || md[bom.len()..].starts_with("---\r\n");
    let front = crate::capture::front(md);
    if opens && front.is_none() {
        return Err("its frontmatter has no closing ---".into());
    }
    if renames.is_empty() && adds.is_empty() {
        return Ok(md.to_string());
    }
    let (lines, body, eol) = match front {
        Some(f) => (f.lines, &md[f.body..], f.eol),
        None => (
            "",
            &md[bom.len()..],
            if md.contains("\r\n") { "\r\n" } else { "\n" },
        ),
    };
    let renames = renames.iter().map(|(a, b)| (*a, b.as_str()));
    let mut lines = crate::notes::rename_images(lines, eol, renames);
    for (key, value) in adds {
        lines = set(&lines, eol, key, value);
    }
    Ok(format!("{bom}---{eol}{lines}---{eol}{body}"))
}

/// Whether `s` holds combining marks or conjoining jamo, as decomposed (NFD) names do. Git
/// on macOS precomposes the paths it's given, so it can't move such a file by name.
fn decomposed(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c, '\u{300}'..='\u{36f}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}' | '\u{fe20}'..='\u{fe2f}' | '\u{1160}'..='\u{11ff}')
    })
}

/// Frontmatter `lines` (each ending in `eol`) with `key: <value>` last, in place of any
/// `key:` lines and the lines continuing them: indented, `- ` items, and blank lines
/// before more of those. `summary: |` and its block go whole.
fn set(lines: &str, eol: &str, key: &str, value: &str) -> String {
    let own = |l: &str| {
        l.strip_prefix(key)
            .and_then(|r| r.strip_prefix(':'))
            .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t', '\r', '\n']))
    };
    let more = |l: &str| {
        let l = l.trim_end_matches(['\r', '\n']);
        l.starts_with([' ', '\t']) || l == "-" || l.starts_with("- ")
    };
    let ls: Vec<&str> = lines.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut i = 0;
    while i < ls.len() {
        if !own(ls[i]) {
            out += ls[i];
            i += 1;
            continue;
        }
        i += 1;
        loop {
            let next = (i..ls.len()).find(|&j| !ls[j].trim().is_empty());
            match next {
                Some(j) if more(ls[j]) => i = j + 1,
                _ => break,
            }
        }
    }
    let value = serde_json::to_string(value).expect("serializes");
    out + &format!("{key}: {value}{eol}")
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

    /// Regular files at HEAD, by path.
    fn files(n: &[&str]) -> Modes {
        n.iter().map(|p| (p.to_string(), "100644".into())).collect()
    }

    fn modes(n: &[(&str, &str)]) -> Modes {
        n.iter()
            .map(|(p, m)| (p.to_string(), m.to_string()))
            .collect()
    }

    const CAPTURE: &str = "---\nschema: 1\nid: 00000000-0000-0000-0000-000000000001\n\
        captured: 2026-11-08T09:12:00Z\nvia: \"share\"\nproject: \"mis\"\nassets:\n  \
        - path: a-1.jpg\n    sha256: h\n  - path: a-2.png\n    sha256: h\n---\n\
        Body #mis\n  - path: a-1.jpg\nproject: body\n";

    #[test]
    fn dests_are_real_folders_one_down_and_the_archive_never_ai() {
        let got = dests(&modes(&[
            ("projects", TREE),
            ("areas", TREE),
            ("resources", TREE),
            ("projects/amf", TREE),
            ("projects/mis", TREE),
            ("projects/README.md", "100644"),
            ("projects/link", "120000"),
            ("projects/sub", "160000"),
            ("areas/home", TREE),
            ("resources/rust", TREE),
            ("projects/.hidden", TREE),
            ("archive", TREE),
        ]));
        let want = [
            "archive",
            "areas/home",
            "projects/amf",
            "projects/mis",
            "resources/rust",
        ];
        assert_eq!(got, want);
        assert_eq!(
            dests(&Modes::new()),
            ["archive"],
            "made when first filed in"
        );
        // A link standing in for a folder, or its parent, offers nothing.
        let links = modes(&[
            ("archive", "120000"),
            ("projects", "120000"),
            ("projects/amf", TREE),
        ]);
        assert!(dests(&links).is_empty());
    }

    #[test]
    fn only_a_plain_inbox_note_a_real_folder_and_a_short_summary_pass() {
        assert_eq!(check(&mv("inbox/a.md", "archive", None)), Ok(()));
        assert_eq!(check(&mv("inbox/A.MD", "projects/amf", Some("  "))), Ok(()));
        for path in [
            "inbox/a-1.jpg",
            "inbox/sub/a.md",
            "projects/amf/a.md",
            "inbox/../a.md",
            "inbox/.a.md",
            "inbox/",
            "a.md",
            "",
            "inbox/a\nMIS-Capture: x.md",
            "inbox/a\r.md",
            "inbox/a\u{1b}.md",
        ] {
            assert!(check(&mv(path, "archive", None)).is_err(), "{path:?}");
        }
        for dest in [
            "ai",
            "ai/x",
            "inbox",
            "projects",
            "projects/a/b",
            "archive/2025",
            "areas/a\nb",
        ] {
            assert!(check(&mv("inbox/a.md", dest, None)).is_err(), "{dest:?}");
        }
        let long = "x".repeat(MAX_SUMMARY_CHARS + 1);
        assert!(check(&mv("inbox/a.md", "archive", Some(&long))).is_err());
        let fits = "é".repeat(MAX_SUMMARY_CHARS);
        assert_eq!(check(&mv("inbox/a.md", "archive", Some(&fits))), Ok(()));
    }

    #[test]
    fn a_capture_moves_with_its_images_under_its_own_name() {
        let inbox = files(&["inbox/a.md", "inbox/a-1.jpg", "inbox/a-2.png", "inbox/b.md"]);
        let m = mv("inbox/a.md", "areas/home", None);
        let got = plan(&m, CAPTURE, &inbox, &[], &names(&["boiler.md"])).unwrap();
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
        let got = plan(
            &m,
            CAPTURE,
            &files(&["inbox/a.md", "inbox/a-2.png"]),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(got.moves.len(), 2);
        assert_eq!(got.moves[1].0, "inbox/a-2.png");
    }

    #[test]
    fn only_pictures_that_are_the_notes_own_files_go_with_it() {
        let md = "---\nid: 00000000-0000-0000-0000-000000000001\ncaptured: 2026-11-08T09:12:00Z\n\
                  assets:\n  - path: b.md\n  - path: ../x.jpg\n  - path: link.jpg\n  - path: a-1.jpg\n---\n";
        let mut inbox = files(&["inbox/a.md", "inbox/b.md", "inbox/a-1.jpg"]);
        inbox.insert("inbox/link.jpg".into(), "120000".into());
        let m = mv("inbox/a.md", "archive", None);
        let got = plan(&m, md, &inbox, &[], &[]).unwrap();
        let from: Vec<&str> = got.moves.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(from, ["inbox/a.md", "inbox/a-1.jpg"]);
        // Another note shows the same picture.
        let claimed = names(&["a-1.jpg"]);
        assert!(plan(&m, md, &inbox, &claimed, &[]).is_err());
        // The note itself a link.
        inbox.insert("inbox/a.md".into(), "120000".into());
        assert!(plan(&m, md, &inbox, &[], &[]).is_err());
    }

    #[test]
    fn a_taken_name_gets_a_number_images_and_frontmatter_too() {
        let inbox = files(&["inbox/a.md", "inbox/a-1.jpg", "inbox/a-2.png"]);
        let m = mv("inbox/a.md", "projects/amf", Some("Pure cores, by example"));
        let got = plan(&m, CAPTURE, &inbox, &[], &names(&["A.md", "README.md"])).unwrap();
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
        let got = plan(&m, CAPTURE, &inbox, &[], &names(&["a-1.webp"])).unwrap();
        assert_eq!(got.moves[0].1, "projects/amf/a-2.md");
        // Whole names count, dots and all.
        let m = mv("inbox/v1.2-notes.md", "archive", None);
        let got = plan(
            &m,
            "x",
            &files(&["inbox/v1.2-notes.md"]),
            &[],
            &names(&["V1.2-notes.md"]),
        );
        assert_eq!(got.unwrap().moves[0].1, "archive/v1.2-notes-2.md");
    }

    #[test]
    fn a_humans_note_gets_frontmatter_only_when_it_needs_some() {
        let m = mv("inbox/Idea.md", "resources/rust", None);
        let inbox = files(&["inbox/Idea.md"]);
        let got = plan(&m, "# Idea\n\n---\nnot frontmatter\n", &inbox, &[], &[]).unwrap();
        assert_eq!(got.md, "# Idea\n\n---\nnot frontmatter\n");
        assert!(got
            .message
            .contains("\nMIS-Filed: - inbox/Idea.md resources/rust/Idea.md\n"));
        let m = mv(
            "inbox/Idea.md",
            "projects/amf",
            Some("A \"quoted\"\nthought"),
        );
        let got = plan(&m, "# Idea\r\n", &inbox, &[], &names(&["idea.md"])).unwrap();
        assert_eq!(got.moves[0].1, "projects/amf/Idea-2.md", "any case");
        assert_eq!(
            got.md,
            "---\r\nsummary: \"A \\\"quoted\\\"\\nthought\"\r\nproject: \"amf\"\r\n---\r\n# Idea\r\n"
        );
        // Frontmatter ending the file, or empty; a `title:` kept.
        let got = plan(&m, "---\ntitle: x\n---", &inbox, &[], &[]).unwrap();
        assert_eq!(
            got.md,
            "---\ntitle: x\nsummary: \"A \\\"quoted\\\"\\nthought\"\nproject: \"amf\"\n---\n"
        );
        let got = plan(&m, "---\n---\nhi\n", &inbox, &[], &[]).unwrap();
        assert!(got.md.starts_with("---\nsummary: ") && got.md.ends_with("\"amf\"\n---\nhi\n"));
        // Never closed: refused, not guessed at.
        assert!(plan(&m, "---\ntitle: x\nhi\n", &inbox, &[], &[]).is_err());
    }

    #[test]
    fn a_replaced_key_takes_its_continuation_lines_with_it() {
        let lines = "title: x\nsummary: |\n  old\n\n  text\nproject:\n  - a\n- b\n\ntags: [t]\n\
                     project: \"twice\"\nnote: >\n  kept\n";
        let got = set(&set(lines, "\n", "summary", "new"), "\n", "project", "amf");
        assert_eq!(
            got,
            "title: x\n\ntags: [t]\nnote: >\n  kept\nsummary: \"new\"\nproject: \"amf\"\n"
        );
        // `projects:` is another key.
        assert_eq!(
            set("projects: [a]\n", "\n", "project", "b"),
            "projects: [a]\nproject: \"b\"\n"
        );
    }

    const ID: &str = "id: 00000000-0000-0000-0000-000000000001\ncaptured: 2026-11-08T09:12:00Z\n";

    #[test]
    fn each_picture_is_renamed_once() {
        // `a.jpg` takes `a-2.jpg`'s old name; `a-2.jpg` must not follow it.
        let m = mv("inbox/a.md", "archive", None);
        let inbox = files(&["inbox/a.md", "inbox/a.jpg", "inbox/a-2.jpg"]);
        let md = format!("---\n{ID}assets:\n  - path: a.jpg\n  - path: a-2.jpg\n---\n");
        let got = plan(&m, &md, &inbox, &[], &names(&["a.md"])).unwrap();
        assert_eq!(got.moves[1].1, "archive/a-2.jpg");
        assert_eq!(got.moves[2].1, "archive/a-2-2.jpg");
        let want = format!("---\n{ID}assets:\n  - path: a-2.jpg\n  - path: a-2-2.jpg\n---\n");
        assert_eq!(got.md, want);
    }

    #[test]
    fn a_picture_listed_twice_moves_once() {
        let m = mv("inbox/a.md", "archive", None);
        let inbox = files(&["inbox/a.md", "inbox/a-1.jpg"]);
        let twice = format!("---\n{ID}assets:\n  - path: a-1.jpg\n  - path: a-1.jpg\n---\n");
        assert_eq!(plan(&m, &twice, &inbox, &[], &[]).unwrap().moves.len(), 2);
    }

    #[test]
    fn frontmatter_never_closed_is_refused_even_with_nothing_to_add() {
        let m = mv("inbox/a.md", "archive", None);
        assert!(plan(&m, "---\ntitle: x\nhi\n", &files(&["inbox/a.md"]), &[], &[]).is_err());
    }

    #[test]
    fn a_byte_order_mark_stays_first() {
        let m = mv("inbox/a.md", "projects/amf", None);
        let got = plan(
            &m,
            "\u{feff}---\ntitle: x\n---\nhi\n",
            &files(&["inbox/a.md"]),
            &[],
            &[],
        );
        assert_eq!(
            got.unwrap().md,
            "\u{feff}---\ntitle: x\nproject: \"amf\"\n---\nhi\n"
        );
    }

    #[test]
    fn a_decomposed_name_is_refused_saying_why() {
        // NFD, as Linux may commit it: git on macOS can't move it.
        let nfd = "inbox/cafe\u{301}.md";
        let got = plan(&mv(nfd, "archive", None), "hi", &files(&[nfd]), &[], &[]);
        assert!(got.is_err_and(|e| e.contains("rename")));
    }

    #[test]
    fn an_image_not_named_after_the_note_must_be_free_at_the_dest() {
        let md = "---\nid: 00000000-0000-0000-0000-000000000001\ncaptured: \
                  2026-11-08T09:12:00Z\nassets:\n  - path: photo.jpg\n---\n";
        let m = mv("inbox/renamed.md", "archive", None);
        let inbox = files(&["inbox/renamed.md", "inbox/photo.jpg"]);
        let got = plan(&m, md, &inbox, &[], &[]).unwrap();
        assert_eq!(
            got.moves[1],
            ("inbox/photo.jpg".into(), "archive/photo.jpg".into())
        );
        assert_eq!(
            plan(&m, md, &inbox, &[], &names(&["Photo.JPG"])),
            Err("archive/photo.jpg exists".into())
        );
    }
}

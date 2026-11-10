//! Project records in the notes clone: `projects/<id>/README.md`, archived ones at
//! `archive/<id>/README.md`. Parsing, the name checks and [`resolve`] are pure; [`list`] is
//! the filesystem shell.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use mis_types::{Project, ProjectError};

use crate::routine::{parse_list, strip_comment, unquote};

/// Where records live.
const DIRS: [&str; 2] = ["projects", "archive"];
/// The keys a record's frontmatter gives the project.
const KEYS: [&str; 7] = [
    "schema", "id", "title", "aliases", "status", "repos", "notes",
];

/// The record in folder `dir` (`projects/<id>` or `archive/<id>`) from its README's `text`:
/// `---`, `key: value` lines, `---`, then the description. Lists are `[a, b]` or `- a` lines
/// below their key; ` #` starts a comment; other keys, and anything indented under them, are
/// left alone.
/// `id` defaults to the folder's name and must match it; `title` defaults to the id,
/// `status` to `active`.
pub fn parse(dir: &str, text: &str) -> Result<Project, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim) != Some("---") {
        return Err("must start with a `---` frontmatter line".into());
    }
    let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
    let mut items: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    // The last top-level key, and whether it had no value (so `- x` lines below list it).
    let mut last: Option<(&str, bool)> = None;
    let mut body = None;
    let mut at = text.find('\n').map_or(text.len(), |i| i + 1);
    for raw in lines {
        at += raw.len();
        let line = raw.trim();
        if line == "---" {
            body = Some(&text[at..]);
            break;
        }
        let line = strip_comment(line);
        if line.is_empty() {
            continue;
        }
        // Under another key: its own nested value, e.g. `links:` then `  docs: x`.
        if raw.starts_with([' ', '\t']) && last.is_some_and(|(k, _)| !KEYS.contains(&k)) {
            continue;
        }
        if let Some(item) = line
            .strip_prefix('-')
            .filter(|r| r.is_empty() || r.starts_with(' '))
        {
            let Some((k, true)) = last else {
                return Err(format!("`{line}`: a list item, but not under a key"));
            };
            items.entry(k).or_default().push(unquote(item.trim()));
            continue;
        }
        if raw.starts_with([' ', '\t']) {
            return Err(format!(
                "`{line}`: indented, but not under a key that takes it"
            ));
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| format!("`{line}`: want `key: value`"))?;
        let (k, v) = (k.trim(), v.trim());
        if keys.insert(k, v).is_some() {
            return Err(format!("`{k}` is set twice"));
        }
        last = Some((k, v.is_empty()));
    }
    let body = body.ok_or("the frontmatter needs a closing `---` line")?;
    let one = |k: &str| keys.get(k).map(|v| unquote(v)).filter(|v| !v.is_empty());
    let list = |k: &str| match keys.get(k).copied().filter(|v| !v.is_empty()) {
        Some(v) => parse_list(k, v),
        None => Ok((items.get(k).into_iter().flatten())
            .filter(|i| !i.is_empty())
            .map(|i| i.to_string())
            .collect()),
    };
    if let Some(s) = one("schema").filter(|s| *s != "1") {
        return Err(format!("schema `{s}`: only 1 is known"));
    }
    let (top, folder) = dir.split_once('/').unwrap_or_default();
    let id = one("id").unwrap_or(folder);
    if id != folder {
        return Err(format!("id `{id}` isn't its folder's name, `{folder}`"));
    }
    Ok(Project {
        id: id.into(),
        title: one("title").unwrap_or(id).into(),
        aliases: list("aliases")?,
        status: one("status").unwrap_or("active").into(),
        repos: list("repos")?,
        notes: list("notes")?,
        archived: top == "archive",
        description: body.trim().into(),
    })
}

/// The projects `records` define, and what's wrong with the rest. `records` are (folder,
/// README text or None if unreadable). Those under `projects/` come first, then by folder.
/// A name (id or alias, ignoring case) is the first record's to claim it, ids before
/// aliases: a later record with a taken id is left out, a taken alias is dropped.
pub fn collect(mut records: Vec<(String, Option<String>)>) -> (Vec<Project>, Vec<ProjectError>) {
    records.sort_by_key(|(dir, _)| (dir.starts_with("archive/"), dir.clone()));
    let mut errors = Vec::new();
    // (record path, project)
    let mut projects = Vec::new();
    // Name, lowercased -> the record that has it.
    let mut taken: HashMap<String, String> = HashMap::new();
    for (dir, text) in records {
        let path = format!("{dir}/README.md");
        let parsed = text
            .ok_or_else(|| "can't be read: a symlink, too big, or not UTF-8".to_string())
            .and_then(|t| parse(&dir, &t));
        let p = match parsed {
            Ok(p) => p,
            Err(error) => {
                errors.push(ProjectError { path, error });
                continue;
            }
        };
        if let Some(first) = taken.get(&p.id.to_lowercase()) {
            let error = format!("id `{}` is taken by {first}", p.id);
            errors.push(ProjectError { path, error });
            continue;
        }
        taken.insert(p.id.to_lowercase(), path.clone());
        projects.push((path, p));
    }
    for (path, p) in &mut projects {
        let mut kept = Vec::new();
        for a in std::mem::take(&mut p.aliases) {
            match taken.get(&a.to_lowercase()) {
                Some(first) if first == path => {} // its own id, or said twice
                Some(first) => {
                    let error = format!("alias `{a}` is taken by {first}");
                    errors.push(ProjectError {
                        path: path.clone(),
                        error,
                    });
                }
                None => {
                    taken.insert(a.to_lowercase(), path.clone());
                    kept.push(a);
                }
            }
        }
        p.aliases = kept;
    }
    (projects.into_iter().map(|(_, p)| p).collect(), errors)
}

/// The project named `slug`, by id or alias, ignoring case. [`collect`] leaves each name to
/// one project.
pub fn resolve<'a>(projects: &'a [Project], slug: &str) -> Option<&'a Project> {
    let slug = slug.to_lowercase();
    let is = |n: &String| n.to_lowercase() == slug;
    projects
        .iter()
        .find(|p| is(&p.id) || p.aliases.iter().any(is))
}

/// What has `slug` (ignoring case), so a new project can't: a record whose id or alias it is,
/// e.g. `projects/pool/README.md`, else one of `folders` (`projects/<x>`, `archive/<x>`) named it.
pub fn taken(projects: &[Project], folders: &[String], slug: &str) -> Option<String> {
    if let Some(p) = resolve(projects, slug) {
        let top = if p.archived { "archive" } else { "projects" };
        return Some(format!("{top}/{}/README.md", p.id));
    }
    let named = |f: &&String| {
        f.split_once('/')
            .is_some_and(|(_, n)| n.eq_ignore_ascii_case(slug))
    };
    folders.iter().find(named).cloned()
}

/// Every folder under [`DIRS`] in `clone`, record or not, e.g. `projects/pool`.
pub fn folders(clone: &Path) -> Vec<String> {
    let mut all = Vec::new();
    for top in DIRS {
        for e in std::fs::read_dir(clone.join(top))
            .into_iter()
            .flatten()
            .flatten()
        {
            if let (Some(name), true) = (e.file_name().to_str(), e.path().is_dir()) {
                all.push(format!("{top}/{name}"));
            }
        }
    }
    all
}

/// Every record in `clone`: each folder under [`DIRS`] with a `README.md`. Read as the Notes
/// view serves them ([`crate::notes_view::read`]: no symlinks below the clone); symlinked
/// folders are skipped.
pub fn list(clone: &Path) -> (Vec<Project>, Vec<ProjectError>) {
    let mut records = Vec::new();
    for top in DIRS {
        let shown = std::fs::symlink_metadata(clone.join(top)).is_ok_and(|m| m.is_dir());
        let Some(read) = shown
            .then(|| std::fs::read_dir(clone.join(top)).ok())
            .flatten()
        else {
            continue;
        };
        for e in read.flatten() {
            let (Some(name), Ok(t)) = (e.file_name().to_str().map(String::from), e.file_type())
            else {
                continue;
            };
            let dir = format!("{top}/{name}");
            let readme = format!("{dir}/README.md");
            if !t.is_dir() || !crate::notes_view::allowed(&readme) {
                continue;
            }
            if std::fs::symlink_metadata(clone.join(&readme)).is_err() {
                continue; // a folder, not a record
            }
            let read = crate::notes_view::read(clone, &readme);
            let text = read.ok().and_then(|(_, b)| String::from_utf8(b).ok());
            records.push((dir, text));
        }
    }
    collect(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIS: &str = "---\nschema: 1\nid: make-it-so\ntitle: Make It So\naliases: [mis]\n\
        status: active\nrepos: [tommy-xr/MakeItSo]   # optional\n\
        notes: [~/notes/projects/make-it-so.md, ~/notes/projects/make-it-so/]\n---\n\
        Short description.\n";

    fn project(id: &str, aliases: &[&str]) -> Project {
        Project {
            id: id.into(),
            title: id.into(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            status: "active".into(),
            repos: vec![],
            notes: vec![],
            archived: false,
            description: String::new(),
        }
    }

    #[test]
    fn parses_a_record() {
        let want = Project {
            id: "make-it-so".into(),
            title: "Make It So".into(),
            aliases: vec!["mis".into()],
            status: "active".into(),
            repos: vec!["tommy-xr/MakeItSo".into()],
            notes: vec![
                "~/notes/projects/make-it-so.md".into(),
                "~/notes/projects/make-it-so/".into(),
            ],
            archived: false,
            description: "Short description.".into(),
        };
        assert_eq!(parse("projects/make-it-so", MIS), Ok(want.clone()));
        // CRLF, block lists, quotes and unknown keys; archived by where it lives.
        let crlf = "---\r\ntitle: \"A: B\"\r\naliases:\r\n  - 'x y'\r\n  - z\r\ntags: [w]\r\n\
            status: done\r\n---\r\n\r\nText.\r\n";
        let got = parse("archive/a", crlf).unwrap();
        assert_eq!(
            (got.id.as_str(), got.title.as_str(), got.status.as_str()),
            ("a", "A: B", "done")
        );
        assert_eq!(
            (got.aliases, got.archived),
            (vec!["x y".into(), "z".into()], true)
        );
        assert_eq!(got.description, "Text.");
        // Defaults.
        assert_eq!(parse("projects/b", "---\n---\n"), Ok(project("b", &[])));
        // What's indented under other keys is theirs, not the project's.
        let nested =
            "---\nlinks:\n  status: wip\n  title: Docs\n  - x\nsummary: >\n  some text\n---\n";
        assert_eq!(parse("projects/b", nested), Ok(project("b", &[])));
    }

    #[test]
    fn a_malformed_record_says_why() {
        for (text, why) in [
            ("# no frontmatter", "must start"),
            ("---\ntitle: x\n", "closing `---`"),
            ("---\nschema: 2\n---\n", "schema `2`"),
            (
                "---\nid: other\n---\n",
                "id `other` isn't its folder's name, `a`",
            ),
            ("---\ntitle: x\ntitle: y\n---\n", "`title` is set twice"),
            ("---\naliases: [x\n---\n", "closing `]`"),
            ("---\n- x\n---\n", "not under a key"),
            ("---\ntitle: x\n- y\n---\n", "not under a key"),
            (
                "---\ntitle:\n  sub: y\n---\n",
                "indented, but not under a key that takes it",
            ),
            ("---\n  title: x\n---\n", "indented"),
            ("---\naliases: [\"a, b\"]\n---\n", "no commas inside quotes"),
            ("---\nrepos: [a,,b]\n---\n", "repos: empty item"),
            ("---\njust words\n---\n", "want `key: value`"),
        ] {
            let err = parse("projects/a", text).unwrap_err();
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }

    #[test]
    fn taken_names_are_reported_not_fatal() {
        let rec = |dir: &str, front: &str| (dir.to_string(), Some(format!("---\n{front}\n---\n")));
        let (projects, errors) = collect(vec![
            rec("archive/mis", ""),
            rec("projects/b", "aliases: [MIS, bee, b]"),
            ("projects/bad".into(), Some("nope".into())),
            ("projects/link".into(), None),
            rec("projects/make-it-so", "aliases: [mis, mis-2, MIS-2]"),
            rec("projects/c", "aliases: [BEE, sea]"),
        ]);
        let names: Vec<(&str, Vec<&str>, bool)> = (projects.iter())
            .map(|p| {
                (
                    p.id.as_str(),
                    p.aliases.iter().map(String::as_str).collect(),
                    p.archived,
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("b", vec!["bee"], false),
                ("c", vec!["sea"], false),
                ("make-it-so", vec!["mis-2"], false),
                ("mis", vec![], true),
            ]
        );
        let errors: Vec<(&str, &str)> = (errors.iter())
            .map(|e| (e.path.as_str(), e.error.as_str()))
            .collect();
        assert_eq!(
            errors,
            [
                (
                    "projects/bad/README.md",
                    "must start with a `---` frontmatter line"
                ),
                (
                    "projects/link/README.md",
                    "can't be read: a symlink, too big, or not UTF-8"
                ),
                (
                    "projects/b/README.md",
                    "alias `MIS` is taken by archive/mis/README.md"
                ),
                (
                    "projects/c/README.md",
                    "alias `BEE` is taken by projects/b/README.md"
                ),
                (
                    "projects/make-it-so/README.md",
                    "alias `mis` is taken by archive/mis/README.md"
                ),
            ]
        );
        // The same id archived and active: the active one wins.
        let (projects, errors) = collect(vec![rec("archive/a", ""), rec("projects/a", "")]);
        assert_eq!((projects.len(), projects[0].archived), (1, false));
        assert_eq!(errors[0].error, "id `a` is taken by projects/a/README.md");
    }

    #[test]
    fn resolves_by_id_or_alias_ignoring_case() {
        let ps = [
            project("make-it-so", &["mis"]),
            project("amf", &["All-Must-Fall"]),
        ];
        for (slug, id) in [
            ("make-it-so", "make-it-so"),
            ("MIS", "make-it-so"),
            ("all-must-fall", "amf"),
            ("AMF", "amf"),
        ] {
            assert_eq!(
                resolve(&ps, slug).map(|p| p.id.as_str()),
                Some(id),
                "{slug}"
            );
        }
        assert_eq!(resolve(&ps, "nope"), None);
        assert_eq!(resolve(&ps, ""), None);
    }

    #[test]
    fn a_slug_is_taken_by_an_id_an_alias_or_a_folder() {
        let mut old = project("old", &["Pool"]);
        old.archived = true;
        let list = [project("garden", &[]), old];
        let folders = ["projects/garden".into(), "archive/Notes-Only".into()];
        let taken = |slug| taken(&list, &folders, slug);
        assert_eq!(
            taken("garden").as_deref(),
            Some("projects/garden/README.md")
        );
        assert_eq!(taken("pool").as_deref(), Some("archive/old/README.md"));
        assert_eq!(taken("notes-only").as_deref(), Some("archive/Notes-Only"));
        assert_eq!(taken("new"), None);
    }

    #[test]
    #[cfg(unix)] // symlinks
    fn list_reads_records_without_following_symlinks() {
        let dir = std::env::temp_dir().join(format!("mis-projects-{}", uuid::Uuid::new_v4()));
        let clone = dir.join("notes");
        let write = |at: &str, text: &str| {
            let at = clone.join(at);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, text).unwrap();
        };
        write("projects/make-it-so/README.md", MIS);
        write("projects/make-it-so/plan.md", "# plan");
        write("projects/no-record/plan.md", "# plan");
        write("projects/.hidden/README.md", MIS);
        write("archive/old/README.md", "---\ntitle: Old\n---\nDone.");
        write("archive/bin/README.md", "");
        std::fs::write(clone.join("archive/bin/README.md"), b"\xff").unwrap();
        std::fs::write(dir.join("secret.md"), MIS.replace("make-it-so", "leak")).unwrap();
        std::fs::create_dir_all(clone.join("projects/leak")).unwrap();
        let link = |to: &Path, at: &str| std::os::unix::fs::symlink(to, clone.join(at)).unwrap();
        link(&dir.join("secret.md"), "projects/leak/README.md");
        link(&clone.join("projects/make-it-so"), "projects/twin");
        let (projects, errors) = list(&clone);
        let ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["make-it-so", "old"]);
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["projects/leak/README.md", "archive/bin/README.md"]);
        assert_eq!(list(&dir.join("none")), (vec![], vec![]));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

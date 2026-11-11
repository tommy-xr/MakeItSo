//! Tasks that start a project (Projects `+`): the prompt they run with. Pure.

use mis_types::{guides, valid_slug, ProjectTask, SLUG_RULE};

/// Most characters in a project's title.
const MAX_TITLE_CHARS: usize = 80;

/// What `t` asks for, the record to write, then the projects guide. Err: why it can't be a task.
pub fn prompt(t: &ProjectTask) -> Result<String, String> {
    let (title, slug, description) = (normalize(&t.title), t.slug.trim(), t.description.trim());
    if !valid_slug(slug) {
        return Err(format!("{slug:?}: {SLUG_RULE}"));
    }
    let value = quoted(&title)?;
    if description.is_empty() {
        return Err("say what the project is".into());
    }
    let repo = match t.repo {
        true => format!(
            "GitHub repo: yes, a new private `<owner>/{slug}` (owner: your gh login), cloned \
             beside this checkout."
        ),
        false => "GitHub repo: no. Skip the repo steps; the record lists no repos.".into(),
    };
    let repos = match t.repo {
        true => format!("[OWNER/{slug}]"),
        false => "[]".into(),
    };
    let guide = guides::guide("projects").expect("the projects guide is compiled in");
    Ok(format!(
        "Start a new project, slug `{slug}`. This folder is a checkout of the mis-notes repo. \
         Follow the projects guide below to the end: a mis-notes PR with its record, never \
         merged.\n\nTitle: {title}\n\n{repo}\n\nDescription:\n{description}\n\n\
         Record frontmatter (`projects/{slug}/README.md`, then the description):\n```\n\
         ---\nschema: 1\nid: {slug}\ntitle: {value}\naliases: []\nstatus: active\n\
         repos: {repos}\nnotes: []\n---\n```\n\n{guide}"
    ))
}

/// `title` trimmed, whitespace runs as one space, curly quotes straight. iPhone keyboards type
/// `’`; this keeps `Bryan’s` and `Bryan's` the same title.
fn normalize(title: &str) -> String {
    (title.split_whitespace().collect::<Vec<_>>().join(" "))
        .replace(['‘', '’'], "'")
        .replace(['“', '”'], "\"")
}

/// `title` as a frontmatter value the projects parser (and YAML) reads back as is: `'…'`, each
/// `'` doubled. Err if it is blank or too long.
fn quoted(title: &str) -> Result<String, String> {
    if title.is_empty() {
        return Err("give the project a title".into());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("a title is up to {MAX_TITLE_CHARS} characters"));
    }
    Ok(format!("'{}'", title.replace('\'', "''")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(title: &str, slug: &str, description: &str, repo: bool) -> ProjectTask {
        ProjectTask {
            title: title.into(),
            slug: slug.into(),
            description: description.into(),
            repo,
            harness: "claude".into(),
            model: None,
            worker: None,
        }
    }

    /// The record the prompt says to write, as the agent would: frontmatter, then the description.
    fn record(p: &str, owner: &str) -> String {
        let start = p.find("---\nschema:").unwrap();
        let end = start + p[start..].find("\n---\n").unwrap() + 5;
        let desc = p.split("Description:\n").nth(1).unwrap();
        let desc = &desc[..desc.find("\n\nRecord frontmatter").unwrap()];
        format!("{}\n{desc}\n", p[start..end].replace("OWNER", owner))
    }

    #[test]
    fn a_project_gets_the_request_the_record_then_the_guide() {
        let p = prompt(&task(" Pool robot ", " pool-robot ", " Clean it.\n", true)).unwrap();
        assert!(
            p.starts_with("Start a new project, slug `pool-robot`."),
            "{p}"
        );
        assert!(p.contains("\n\nGitHub repo: yes, a new private `<owner>/pool-robot`"));
        assert!(p.contains("\n\nDescription:\nClean it.\n\n"));
        assert!(p.ends_with(&format!(
            "---\n```\n\n{}",
            guides::guide("projects").unwrap()
        )));
        let got = crate::projects::parse("projects/pool-robot", &record(&p, "me")).unwrap();
        assert_eq!(
            (got.id.as_str(), got.title.as_str(), got.status.as_str()),
            ("pool-robot", "Pool robot", "active")
        );
        assert_eq!(
            (got.repos, got.description),
            (vec!["me/pool-robot".into()], "Clean it.".into())
        );
    }

    #[test]
    fn without_a_repo_the_record_lists_none() {
        let p = prompt(&task("Garden", "garden", "Beds.", false)).unwrap();
        assert!(p.contains("\n\nGitHub repo: no."), "{p}");
        assert!(p.contains("\nrepos: []\n"));
        let got = crate::projects::parse("projects/garden", &record(&p, "me")).unwrap();
        assert!(got.repos.is_empty() && got.aliases.is_empty() && got.notes.is_empty());
    }

    #[test]
    fn titles_read_back_as_typed() {
        for title in [
            "A: b #1",
            "Tom's #1 idea",
            "say \"hi\" # now",
            "it's \"x\" # 'y'",
            "'quoted' \"too\"",
        ] {
            let p = prompt(&task(title, "x", "d", false)).unwrap();
            assert!(p.contains(&format!("\n\nTitle: {title}\n\n")), "{p}");
            let got = crate::projects::parse("projects/x", &record(&p, "me")).unwrap();
            assert_eq!(got.title, title, "{p}");
        }
    }

    #[test]
    fn titles_are_normalized() {
        for (typed, want) in [
            (" Pool  robot\t", "Pool robot"),
            ("a\nb", "a b"),
            ("Bryan’s “Big” ‘Tool’", "Bryan's \"Big\" 'Tool'"),
            ("plain", "plain"),
        ] {
            assert_eq!(normalize(typed), want, "{typed:?}");
        }
        let p = prompt(&task("Bryan’s “Big” Tool", "x", "d", false)).unwrap();
        assert!(p.contains("\ntitle: 'Bryan''s \"Big\" Tool'\n"), "{p}");
        let got = crate::projects::parse("projects/x", &record(&p, "me")).unwrap();
        assert_eq!(got.title, "Bryan's \"Big\" Tool");
    }

    #[test]
    fn bad_slugs_titles_and_blank_descriptions_are_refused() {
        let bad = |t: ProjectTask| prompt(&t).unwrap_err();
        assert!(bad(task("T", "Pool!", "d", true)).contains("a-z"));
        assert!(bad(task("T", "../x", "d", true)).contains("a-z"));
        assert!(bad(task("T", "", "d", true)).contains("a-z"));
        assert!(bad(task(" ", "x", "d", true)).contains("title"));
        assert!(bad(task(&"t".repeat(81), "x", "d", true)).contains("80"));
        assert!(bad(task("T", "x", " \n ", true)).contains("what the project is"));
    }
}

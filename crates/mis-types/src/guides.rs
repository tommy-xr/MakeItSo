//! Guides: how-to prompts for agents (`guides/*.md` at the repo root), compiled in so the CLI
//! prints them and the orchestrator can put them in the prompts of the flows they cover. Each
//! starts with SKILL.md frontmatter (`name`, `description`), so it installs as a skill unchanged.

/// Each guide's name and text.
pub const GUIDES: [(&str, &str); 2] = [
    ("apps", include_str!("../../../guides/apps.md")),
    ("routines", include_str!("../../../guides/routines.md")),
];

/// The guide called `name`, whole.
pub fn guide(name: &str) -> Option<&'static str> {
    GUIDES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, text)| *text)
}

/// `key`'s value in `text`'s frontmatter (`key: value` lines between leading `---` lines).
/// Line by line, so a CRLF checkout (Windows) reads the same.
pub fn front<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let mut lines = text.lines().map(str::trim_end);
    if lines.next()? != "---" {
        return None;
    }
    lines
        .take_while(|l| *l != "---")
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_guide_names_itself_and_says_what_it_is_for() {
        for (name, text) in GUIDES {
            assert_eq!(front(text, "name"), Some(name));
            let what = front(text, "description").unwrap_or_default();
            // Plain YAML scalars, as skill loaders parse them: no `: ` or ` #`.
            assert!(
                what.len() > 40 && !what.contains(": ") && !what.contains(" #"),
                "{name}"
            );
            assert_eq!(guide(name), Some(text));
        }
        assert_eq!(guide("workers"), None);
    }

    #[test]
    fn front_reads_only_the_frontmatter() {
        let text = "---\r\ndescription: d: e\r\n---\r\nname: body\r\n";
        assert_eq!(front(text, "description"), Some("d: e"));
        assert_eq!(front(text, "name"), None);
        assert_eq!(front("---\nname: x\n---\n", "name"), Some("x"));
        assert_eq!(front(text, "nam"), None);
        assert_eq!(front("name: x\n", "name"), None);
    }
}

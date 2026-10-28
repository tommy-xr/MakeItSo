//! GitHub links an agent mentions (PRs, commits, CI runs, issues), pulled out
//! of free text so they outlive transcript eviction and summarization.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Pr,
    Commit,
    CiRun,
    Issue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    /// Canonical: `https://github.com/o/r/pull/42`, whatever subpage was mentioned.
    pub url: String,
    /// `o/r#42`, `o/r@abc1234`, `o/r run 123`.
    pub label: String,
}

const PREFIX: &str = "https://github.com/";

/// Every GitHub PR, commit, CI run and issue URL in `text`, canonicalized,
/// deduped, in first-seen order. Look-alike hosts (`github.com.evil.io`,
/// `notgithub.com`) and URLs embedded mid-word or inside another URL are skipped.
pub fn github_links(text: &str) -> Vec<Link> {
    let mut out: Vec<Link> = vec![];
    let mut seen = std::collections::HashSet::new();
    for (i, _) in text.match_indices(PREFIX) {
        // `xhttps://…` or `https://evil.io/?u=https://github.com/…`.
        let embedded = text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || "/:.=?&%#@+-_~".contains(c));
        if embedded {
            continue;
        }
        let Some(link) = parse(&text[i + PREFIX.len()..]) else {
            continue;
        };
        if seen.insert(link.url.clone()) {
            out.push(link);
        }
    }
    out
}

/// `owner/repo/<kind>/<id>…` right after `https://github.com/`.
fn parse(rest: &str) -> Option<Link> {
    let mut seg = rest.split('/');
    let owner = seg.next().filter(|s| is_name(s, false))?;
    let repo = seg.next().filter(|s| is_name(s, true))?;
    let (kind, id) = match seg.next()? {
        "pull" => (LinkKind::Pr, number(seg.next()?)?),
        "issues" => (LinkKind::Issue, number(seg.next()?)?),
        "commit" => (LinkKind::Commit, sha(seg.next()?)?),
        "actions" if seg.next()? == "runs" => (LinkKind::CiRun, number(seg.next()?)?),
        _ => return None,
    };
    let (path, label) = match kind {
        LinkKind::Pr => ("pull", format!("{owner}/{repo}#{id}")),
        LinkKind::Issue => ("issues", format!("{owner}/{repo}#{id}")),
        LinkKind::Commit => ("commit", format!("{owner}/{repo}@{}", &id[..7])),
        LinkKind::CiRun => ("actions/runs", format!("{owner}/{repo} run {id}")),
    };
    Some(Link {
        kind,
        url: format!("{PREFIX}{owner}/{repo}/{path}/{id}"),
        label,
    })
}

/// Owner: letters, digits, `-`. Repo also allows `.` and `_`.
fn is_name(s: &str, repo: bool) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || (repo && (c == '.' || c == '_')))
}

/// Leading run of chars matching `ok`, if it is followed by a non-word char
/// (`42.`, `42)`, `` 42` ``, `42?x` pass; `42abc` doesn't).
fn token(s: &str, ok: impl Fn(char) -> bool) -> Option<&str> {
    let end = s.find(|c: char| !ok(c)).unwrap_or(s.len());
    let next = s[end..].chars().next();
    (end > 0 && !next.is_some_and(|c| c.is_alphanumeric() || c == '_')).then(|| &s[..end])
}

fn number(s: &str) -> Option<&str> {
    token(s, |c| c.is_ascii_digit())
}

fn sha(s: &str) -> Option<&str> {
    token(s, |c| c.is_ascii_hexdigit()).filter(|h| (7..=40).contains(&h.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        github_links(text).into_iter().map(|l| l.url).collect()
    }

    #[test]
    fn finds_each_kind_with_labels() {
        let sha = "ec3e854a1b2c3d4e5f60718293a4b5c6d7e8f901";
        let text = format!(
            "Opened https://github.com/o/r.rs/pull/42, pushed \
             https://github.com/o/r.rs/commit/{sha}; CI: \
             https://github.com/o/r.rs/actions/runs/987654 for https://github.com/o/r.rs/issues/7"
        );
        let got = github_links(&text);
        let got: Vec<_> = got.iter().map(|l| (l.kind, l.label.as_str())).collect();
        assert_eq!(
            got,
            [
                (LinkKind::Pr, "o/r.rs#42"),
                (LinkKind::Commit, "o/r.rs@ec3e854"),
                (LinkKind::CiRun, "o/r.rs run 987654"),
                (LinkKind::Issue, "o/r.rs#7"),
            ]
        );
        assert_eq!(
            urls(&text)[1],
            format!("https://github.com/o/r.rs/commit/{sha}")
        );
    }

    #[test]
    fn strips_trailing_punctuation_and_markdown() {
        let pr = "https://github.com/o/r/pull/42";
        for text in [
            "see https://github.com/o/r/pull/42.",
            "(https://github.com/o/r/pull/42)",
            "[PR](https://github.com/o/r/pull/42), done",
            "`https://github.com/o/r/pull/42`",
            "<https://github.com/o/r/pull/42>",
            "**https://github.com/o/r/pull/42**",
            "\"https://github.com/o/r/pull/42\"",
            "https://github.com/o/r/pull/42\n",
        ] {
            assert_eq!(urls(text), [pr], "{text}");
        }
    }

    #[test]
    fn subpages_dedupe_to_one_link_in_first_seen_order() {
        let text = "https://github.com/o/r/issues/1 then \
            https://github.com/o/r/pull/42/files and https://github.com/o/r/pull/42#issuecomment-9 \
            https://github.com/o/r/pull/42?w=1 https://github.com/o/r/actions/runs/5/job/6 \
            https://github.com/o/r/actions/runs/5 https://github.com/o/r/issues/1";
        assert_eq!(
            urls(text),
            [
                "https://github.com/o/r/issues/1",
                "https://github.com/o/r/pull/42",
                "https://github.com/o/r/actions/runs/5",
            ]
        );
    }

    #[test]
    fn rejects_look_alikes_and_malformed() {
        for text in [
            "https://github.com.evil.io/o/r/pull/1",
            "https://notgithub.com/o/r/pull/1",
            "http://github.com/o/r/pull/1",
            "xhttps://github.com/o/r/pull/1",
            "https://evil.io/?u=https://github.com/o/r/pull/1",
            "https://evil.io/https://github.com/o/r/pull/1",
            "https://github.com/o/r/pull/abc",
            "https://github.com/o/r/pull/42abc",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/pulls",
            "https://github.com/o/r/commit/abc12",
            "https://github.com/o/r/commit/zzzzzzz",
            "https://github.com/o/r/actions/workflows/ci.yml",
            "https://github.com/o/r/tree/main",
            "https://github.com/o//pull/1",
            "https://github.com/o<x>/r/pull/1",
            "https://github.com/",
            "javascript:alert(1)//https://github.com/o/r/pull/1",
        ] {
            assert_eq!(urls(text), Vec::<String>::new(), "{text}");
        }
    }

    #[test]
    fn multibyte_text_around_links() {
        assert_eq!(
            urls("→https://github.com/o/r/pull/3… é"),
            ["https://github.com/o/r/pull/3"]
        );
    }
}

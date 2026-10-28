//! Evidence of a task's work: GitHub links an agent mentions (PRs, commits, CI
//! runs, issues), pulled out of free text so they outlive transcript eviction
//! and summarization, and items it reports itself (links, test results, notes).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Pr,
    Commit,
    CiRun,
    Issue,
    /// Any other https URL the agent reported, e.g. a deployed preview.
    Url,
    /// A check the agent reported running, e.g. `cargo test`.
    Test,
    /// Short free text the agent reported.
    Note,
}

/// One evidence row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    /// GitHub kinds are canonical: `https://github.com/o/r/pull/42`, whatever
    /// subpage was mentioned. Empty for a note, or a test reported without one.
    #[serde(default)]
    pub url: String,
    /// `o/r#42`, `o/r@abc1234`, `o/r run 123`; reported items: their title.
    pub label: String,
    /// A test's verdict.
    #[serde(default)]
    pub ok: Option<bool>,
    /// A test's result, e.g. `78 passed`.
    #[serde(default)]
    pub detail: Option<String>,
    /// A PR's live state, as the task's worker last read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrStatus>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// The PR's CI checks, rolled up.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Checks {
    Pending,
    Passing,
    Failing,
    /// The head commit has no checks.
    None,
}

/// A PR's state and checks, read with `gh pr view` by the worker that ran the task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrStatus {
    pub state: PrState,
    pub checks: Checks,
    /// Names of failing checks, at most a few.
    #[serde(default)]
    pub failing: Vec<String>,
    /// None while GitHub is still computing it.
    #[serde(default)]
    pub mergeable: Option<bool>,
    /// `approved`, `changes_requested`, `review_required`; None without review rules.
    #[serde(default)]
    pub review: Option<String>,
    /// Head commit sha.
    #[serde(default)]
    pub head: String,
    /// Unix millis when the worker read it.
    #[serde(default)]
    pub observed_at: u64,
}

impl PrStatus {
    /// Same state, whenever it was read.
    pub fn same(&self, other: &PrStatus) -> bool {
        PrStatus {
            observed_at: other.observed_at,
            ..self.clone()
        } == *other
    }
}

impl Link {
    /// One transcript line: `cargo test ✗ 3 failed`, `o/r#42 https://github.com/o/r/pull/42`.
    pub fn line(&self) -> String {
        let verdict = match self.ok {
            Some(true) => " ✓",
            Some(false) => " ✗",
            None => "",
        };
        let label = format!("{}{verdict}", self.label);
        let detail = self.detail.as_deref().unwrap_or("");
        [label.as_str(), detail, &self.url]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
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

/// The GitHub PR, commit, CI run or issue `url` points into, canonicalized;
/// None for any other URL.
pub fn github_link(url: &str) -> Option<Link> {
    url.strip_prefix(PREFIX).and_then(parse)
}

/// `owner/repo/<kind>/<id>…` right after `https://github.com/`.
fn parse(rest: &str) -> Option<Link> {
    let mut seg = rest.split('/');
    let owner = seg.next().filter(|s| is_name(s, false))?;
    let repo = seg.next().filter(|s| is_name(s, true))?;
    let (kind, path, id) = match seg.next()? {
        "pull" => (LinkKind::Pr, "pull", number(seg.next()?)?),
        "issues" => (LinkKind::Issue, "issues", number(seg.next()?)?),
        "commit" => (LinkKind::Commit, "commit", sha(seg.next()?)?),
        "actions" if seg.next()? == "runs" => {
            (LinkKind::CiRun, "actions/runs", number(seg.next()?)?)
        }
        _ => return None,
    };
    let label = match kind {
        LinkKind::Commit => format!("{owner}/{repo}@{}", &id[..7]),
        LinkKind::CiRun => format!("{owner}/{repo} run {id}"),
        _ => format!("{owner}/{repo}#{id}"),
    };
    Some(Link {
        kind,
        url: format!("{PREFIX}{owner}/{repo}/{path}/{id}"),
        label,
        ok: None,
        detail: None,
        pr: None,
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
    fn github_link_classifies_only_github_urls() {
        let pr = github_link("https://github.com/o/r/pull/42/files").unwrap();
        assert_eq!((pr.kind, pr.label.as_str()), (LinkKind::Pr, "o/r#42"));
        assert_eq!(pr.url, "https://github.com/o/r/pull/42");
        for url in [
            "https://github.com/o/r/tree/main",
            "https://github.com.evil.io/o/r/pull/1",
            "https://preview.example.com/pull/1",
        ] {
            assert_eq!(github_link(url), None, "{url}");
        }
    }

    #[test]
    fn lines() {
        let test = Link {
            kind: LinkKind::Test,
            url: String::new(),
            label: "cargo test".into(),
            ok: Some(false),
            detail: Some("3 failed".into()),
            pr: None,
        };
        assert_eq!(test.line(), "cargo test ✗ 3 failed");
        let pr = github_link("https://github.com/o/r/pull/4").unwrap();
        assert_eq!(pr.line(), "o/r#4 https://github.com/o/r/pull/4");
        // Links stored before reported evidence existed still parse.
        let old = r#"{"kind":"pr","url":"https://github.com/o/r/pull/4","label":"o/r#4"}"#;
        assert_eq!(serde_json::from_str::<Link>(old).unwrap(), pr);
    }

    #[test]
    fn multibyte_text_around_links() {
        assert_eq!(
            urls("→https://github.com/o/r/pull/3… é"),
            ["https://github.com/o/r/pull/3"]
        );
    }
}

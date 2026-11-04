//! Merging PRs from routine runs: only through the worker's `merge_pr` tool,
//! which checks a routine's `merge:` policy in code before it merges.

use serde::{Deserialize, Serialize};

/// The worker's MCP tool that merges a PR its policy allows.
pub const MERGE_TOOL: &str = "merge_pr";

/// A routine file's `merge:` block: which PRs `merge_pr` may merge, and how.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergePolicy {
    /// `owner/name`, compared without case.
    pub repos: Vec<String>,
    /// Most changed files a PR may have.
    pub max_files: u32,
    /// Most added plus deleted lines.
    pub max_lines: u32,
    /// PR author logins as `gh` gives them, e.g. `app/dependabot`.
    pub authors: Vec<String>,
    /// Globs no changed path may match: `*` within a path segment, `**` across them.
    #[serde(default)]
    pub forbid_paths: Vec<String>,
    /// Labels the PR must carry, all of them.
    #[serde(default)]
    pub require_labels: Vec<String>,
    pub method: MergeMethod,
    pub approve: MergeApprove,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }
}

/// Who confirms each merge that passes the policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeApprove {
    /// An approval card on the phone; no answer in time is a no.
    Phone,
    /// Nobody: it merges once the checks pass.
    None,
}

/// How a task may merge PRs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeRule {
    /// As its worker's permissions allow: a task the user asked for.
    #[default]
    Any,
    /// A routine run without a policy: never.
    Never,
    /// A routine run: only through `merge_pr`, under this policy.
    Tool(Box<MergePolicy>),
}

/// A PR `merge_pr` found eligible, as its approval card shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeFacts {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub base: String,
    /// Head commit the checks passed on; the merge must match it.
    pub head: String,
    pub files: u64,
    pub additions: u64,
    pub deletions: u64,
    /// e.g. `5 passed, 2 skipped`.
    pub checks: String,
    pub method: MergeMethod,
}

/// The evidence row title of a `merge_pr` attempt on `repo#number` (a test row).
pub fn merge_label(repo: &str, number: u64) -> String {
    format!("merge {repo}#{number}")
}

/// Whether `label` is one `merge_label` makes: only the worker may write those.
pub fn is_merge_label(label: &str) -> bool {
    let Some((repo, n)) = label
        .strip_prefix("merge ")
        .and_then(|l| l.rsplit_once('#'))
    else {
        return false;
    };
    !repo.is_empty() && !repo.contains(char::is_whitespace) && n.parse::<u64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_labels_are_recognised() {
        assert_eq!(merge_label("o/r", 7), "merge o/r#7");
        assert!(is_merge_label("merge o/r#7"));
        for other in [
            "merge o/r",
            "merge #7",
            "merge o r#7",
            "merged o/r#7",
            "merge o/r#x",
        ] {
            assert!(!is_merge_label(other), "{other}");
        }
    }
}

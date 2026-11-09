//! `merge_pr`: the only way a routine run merges a PR. Whether a PR may merge is
//! decided here, in code, against the routine's `merge:` policy; the agent only
//! names it. Pure: the call's arguments, `gh`'s PR JSON, the eligibility check,
//! and what each outcome reports. `Merger` is the shell: it runs `gh` (argv
//! only, never a shell) and asks the user.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use mis_types::{
    first_line, Approval, Link, LinkKind, MergeApprove, MergeFacts, MergeMethod, MergePolicy,
    TaskId, MAX_APPROVAL_DETAIL,
};
use serde::Deserialize;
use serde_json::Value;

use crate::approvals::Approvals;

/// `gh pr view --json` fields `Pr` reads.
pub const FIELDS: &str = "number,title,state,author,files,additions,deletions,changedFiles,\
    isDraft,mergeable,mergeStateStatus,labels,headRefOid,statusCheckRollup,baseRefName";
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// The PR a call names: `repo` as the policy spells it.
#[derive(Debug, Clone, PartialEq)]
pub struct Want {
    pub repo: String,
    pub number: u64,
}

impl Want {
    pub fn url(&self) -> String {
        format!("https://github.com/{}/pull/{}", self.repo, self.number)
    }
}

/// `{repo, number}` from the agent, the repo one the policy lists.
pub fn want(args: &Value, policy: &MergePolicy) -> Result<Want, String> {
    let repo = args["repo"].as_str().map(str::trim);
    let repo = repo.ok_or("repo must be a string: owner/name")?;
    let number = match &args["number"] {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    let number = number
        .filter(|n| *n > 0)
        .ok_or("number must be the PR number")?;
    let repo = (policy.repos.iter())
        .find(|r| r.eq_ignore_ascii_case(repo))
        .ok_or_else(|| format!("repo {repo} isn't in this routine's merge repos"))?;
    Ok(Want {
        repo: repo.clone(),
        number,
    })
}

/// `gh` argv reading `w`.
pub fn view_args(w: &Want) -> Vec<String> {
    let n = w.number.to_string();
    ["pr", "view", &n, "-R", &w.repo, "--json", FIELDS]
        .map(String::from)
        .to_vec()
}

/// `gh` argv merging `w`, only while its head is still `head`.
pub fn merge_args(w: &Want, method: MergeMethod, head: &str) -> Vec<String> {
    let (n, how) = (w.number.to_string(), format!("--{}", method.as_str()));
    let a = [
        "pr",
        "merge",
        &n,
        "-R",
        &w.repo,
        &how,
        "--match-head-commit",
        head,
    ];
    a.map(String::from).to_vec()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub author: Author,
    pub files: Vec<File>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub is_draft: bool,
    pub mergeable: String,
    pub merge_state_status: String,
    pub labels: Vec<Label>,
    pub head_ref_oid: String,
    pub status_check_rollup: Vec<Check>,
    pub base_ref_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Author {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub path: String,
    #[serde(default)]
    pub change_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

/// A check run (`status`, `conclusion`) or a commit status (`state`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    /// `CheckRun` or `StatusContext`.
    #[serde(rename = "__typename")]
    pub kind: String,
    pub name: String,
    pub context: String,
    pub status: String,
    pub conclusion: String,
    pub state: String,
    pub workflow_name: String,
    /// ISO 8601, so it sorts as a string.
    pub started_at: String,
}

impl Check {
    /// Some(true) passed (skipped and neutral count), Some(false) failed, None unfinished.
    fn verdict(&self) -> Option<bool> {
        let result = match self.state.as_str() {
            "" if self.status != "COMPLETED" => return None,
            "" => self.conclusion.as_str(),
            state => state,
        };
        match result {
            "SUCCESS" | "SKIPPED" | "NEUTRAL" => Some(true),
            "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "" => None,
            _ => Some(false),
        }
    }

    fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.context
        } else {
            &self.name
        }
    }
}

pub fn parse(json: &str) -> Result<Pr, String> {
    serde_json::from_str(json).map_err(|e| format!("unexpected gh output: {e}"))
}

/// Whether `path` matches `glob`: `*` any run within a path segment, `**` any
/// run across them (`.github/**` matches everything under `.github/`), `?` one
/// character; anything else literally.
pub fn glob(glob: &str, path: &str) -> bool {
    fn go(g: &[u8], p: &[u8]) -> bool {
        match g {
            [] => p.is_empty(),
            // `**/x` also matches `x` at the top.
            [b'*', b'*', b'/', rest @ ..] => (0..=p.len())
                .filter(|&i| i == 0 || p[i - 1] == b'/')
                .any(|i| go(rest, &p[i..])),
            [b'*', b'*', rest @ ..] => (0..=p.len()).any(|i| go(rest, &p[i..])),
            [b'*', rest @ ..] => (0..=p.len())
                .take_while(|&i| i == 0 || p[i - 1] != b'/')
                .any(|i| go(rest, &p[i..])),
            [b'?', rest @ ..] => p.first().is_some_and(|&c| c != b'/') && go(rest, &p[1..]),
            [c, rest @ ..] => p.first() == Some(c) && go(rest, &p[1..]),
        }
    }
    go(glob.as_bytes(), path.as_bytes())
}

/// Whether `policy` lets `pr` (fetched as `w`) merge now: its facts, or the
/// first rule it fails. Every rule must pass; anything unknown fails.
pub fn check(policy: &MergePolicy, w: &Want, pr: &Pr) -> Result<MergeFacts, String> {
    if pr.number != w.number {
        return Err(format!("gh returned #{} for #{}", pr.number, w.number));
    }
    if pr.state != "OPEN" {
        return Err(format!("it is {}", pr.state.to_lowercase()));
    }
    if pr.is_draft {
        return Err("it is a draft".into());
    }
    let author = &pr.author.login;
    if !(policy.authors.iter()).any(|a| a.eq_ignore_ascii_case(author)) {
        return Err(format!(
            "author {author} isn't in this routine's merge authors"
        ));
    }
    if pr.changed_files > u64::from(policy.max_files) {
        let max = policy.max_files;
        return Err(format!(
            "{} files changed, over max_files {max}",
            pr.changed_files
        ));
    }
    // gh lists at most 100 files: a shorter list than the count can't be checked.
    if pr.files.len() as u64 != pr.changed_files {
        return Err(format!(
            "gh listed {} of its {} files",
            pr.files.len(),
            pr.changed_files
        ));
    }
    let lines = pr.additions + pr.deletions;
    if lines > u64::from(policy.max_lines) {
        return Err(format!(
            "{lines} lines changed, over max_lines {}",
            policy.max_lines
        ));
    }
    for f in &pr.files {
        if let Some(g) = policy.forbid_paths.iter().find(|g| glob(g, &f.path)) {
            return Err(format!(
                "it changes {}, which forbid_paths {g} covers",
                f.path
            ));
        }
        // GitHub names only a renamed or copied file's new path; its old one may be
        // forbidden. Older gh doesn't say how a file changed: that can't be checked either.
        let plain = matches!(
            f.change_type.as_str(),
            "ADDED" | "MODIFIED" | "DELETED" | "CHANGED"
        );
        if !plain && !policy.forbid_paths.is_empty() {
            return Err(format!(
                "it renames or copies {} (or gh didn't say), hiding the old path",
                f.path
            ));
        }
    }
    let has = |l: &String| pr.labels.iter().any(|x| &x.name == l);
    if let Some(missing) = policy.require_labels.iter().find(|l| !has(l)) {
        return Err(format!("it lacks the label {missing}"));
    }
    let checks = newest_checks(pr);
    if checks.is_empty() {
        return Err("no checks ran on it".into());
    }
    checks_settled(&checks)?;
    if !checks
        .iter()
        .any(|c| c.conclusion == "SUCCESS" || c.state == "SUCCESS")
    {
        return Err("no check passed: all were skipped".into());
    }
    ready(pr)?;
    let head = &pr.head_ref_oid;
    if head.len() != 40 || !head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("unexpected head commit {head:?}"));
    }
    let skipped = (checks.iter())
        .filter(|c| matches!(c.conclusion.as_str(), "SKIPPED" | "NEUTRAL"))
        .count();
    let passed = checks.len() - skipped;
    Ok(MergeFacts {
        repo: w.repo.clone(),
        number: w.number,
        title: first_line(&pr.title, 200),
        url: w.url(),
        author: author.clone(),
        base: pr.base_ref_name.clone(),
        head: head.clone(),
        files: pr.changed_files,
        additions: pr.additions,
        deletions: pr.deletions,
        checks: match skipped {
            0 => format!("{passed} passed"),
            s => format!("{passed} passed, {s} skipped"),
        },
        method: policy.method,
    })
}

/// Each check's newest run: a re-run counts only its newest, as the PR card has it; an
/// unfinished run always counts, as a queued re-run may not have started.
fn newest_checks(pr: &Pr) -> Vec<&Check> {
    let mut newest: BTreeMap<(&str, &str, &str), &Check> = BTreeMap::new();
    for c in &pr.status_check_rollup {
        let key = (c.kind.as_str(), c.workflow_name.as_str(), c.label());
        let replaces = |old: &&Check| {
            old.verdict().is_some() && (c.verdict().is_none() || old.started_at <= c.started_at)
        };
        if newest.get(&key).is_none_or(replaces) {
            newest.insert(key, c);
        }
    }
    newest.into_values().collect()
}

/// Err if any of `checks` failed or is still running, naming up to three.
fn checks_settled(checks: &[&Check]) -> Result<(), String> {
    let named = |v: Option<bool>| -> Vec<&str> {
        let them = checks.iter().filter(|c| c.verdict() == v);
        them.map(|c| c.label()).take(3).collect()
    };
    let failed = named(Some(false));
    if !failed.is_empty() {
        return Err(format!("checks failed: {}", failed.join(", ")));
    }
    let pending = named(None);
    if !pending.is_empty() {
        return Err(format!("checks still running: {}", pending.join(", ")));
    }
    Ok(())
}

/// Err unless GitHub says `pr` can merge cleanly now.
fn ready(pr: &Pr) -> Result<(), String> {
    if pr.mergeable != "MERGEABLE" || pr.merge_state_status != "CLEAN" {
        return Err(format!(
            "GitHub says it isn't ready to merge ({}, {})",
            pr.mergeable.to_lowercase(),
            pr.merge_state_status.to_lowercase()
        ));
    }
    Ok(())
}

/// What publishing an app draft does with its PR.
#[derive(Debug, PartialEq)]
pub enum DraftMerge {
    Merge,
    /// Already merged at the draft's head (an earlier publish whose activation failed).
    Merged,
}

/// Whether app draft PR `pr` (fetched as `w`) may merge now. The owner's tap is the review:
/// no policy, and a repo without CI is fine, but its head must still be `head` (what the
/// draft was built from), and it must be open and ready, no check failed or still running.
pub fn check_draft(w: &Want, pr: &Pr, head: &str) -> Result<DraftMerge, String> {
    if pr.number != w.number {
        return Err(format!("gh returned #{} for #{}", pr.number, w.number));
    }
    if !pr.head_ref_oid.eq_ignore_ascii_case(head) {
        return Err(format!(
            "it was pushed to after the draft was made (head {}, draft {})",
            short(&pr.head_ref_oid),
            short(head)
        ));
    }
    match pr.state.as_str() {
        "MERGED" => return Ok(DraftMerge::Merged),
        "OPEN" => {}
        s => return Err(format!("it is {}", s.to_lowercase())),
    }
    if pr.is_draft {
        return Err("it is a draft PR".into());
    }
    checks_settled(&newest_checks(pr))?;
    ready(pr)?;
    Ok(DraftMerge::Merge)
}

/// `owner/name` of a GitHub remote URL (`git@github.com:o/n.git`, `https://github.com/o/n`).
pub fn github_repo(remote: &str) -> Option<String> {
    let path = (remote.strip_prefix("git@github.com:"))
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (o, n) = path.split_once('/')?;
    (!o.is_empty() && !n.is_empty() && !n.contains('/')).then(|| format!("{o}/{n}"))
}

/// Merge app draft PR `w` at `head` if [`check_draft`] lets it, its repo being `apps_repo`'s
/// origin. Ok: what happened. Err: why nothing was merged.
pub async fn merge_draft(
    gh: &std::ffi::OsStr,
    apps_repo: &str,
    w: &Want,
    head: &str,
) -> Result<String, String> {
    let origin = crate::git::origin(apps_repo).await;
    let ours = origin.as_deref().and_then(github_repo);
    if !ours.is_some_and(|o| o.eq_ignore_ascii_case(&w.repo)) {
        return Err(format!(
            "{} isn't the apps repo ({apps_repo}'s origin is {})",
            w.repo,
            origin.as_deref().unwrap_or("unknown")
        ));
    }
    let json =
        (run_gh(gh, &view_args(w)).await).map_err(|e| format!("couldn't read the PR: {e}"))?;
    match check_draft(w, &parse(&json)?, head)? {
        DraftMerge::Merged => Ok(format!("{}#{} was merged already", w.repo, w.number)),
        DraftMerge::Merge => {
            merge_confirmed(gh, w, MergeMethod::Squash, head).await?;
            Ok(format!(
                "merged {}#{} (squash at {})",
                w.repo,
                w.number,
                short(head)
            ))
        }
    }
}

/// `gh` with `args`, its stdout; Err: why it failed.
async fn run_gh(gh: &std::ffi::OsStr, args: &[String]) -> Result<String, String> {
    (crate::pr::gh(gh, args, GH_TIMEOUT).await).map_err(crate::pr::GhError::why)
}

/// Merge `w` while its head is `head`, then read it back: Err unless it is merged now (gh
/// exits 0 having only queued it on a repo with a merge queue).
async fn merge_confirmed(
    gh: &std::ffi::OsStr,
    w: &Want,
    method: MergeMethod,
    head: &str,
) -> Result<(), String> {
    run_gh(gh, &merge_args(w, method, head)).await?;
    let state = (run_gh(gh, &view_args(w)).await.ok())
        .and_then(|json| parse(&json).ok())
        .map(|pr| pr.state);
    match state.as_deref() {
        Some("MERGED") => Ok(()),
        Some(s) => Err(format!(
            "gh merged it, but it is {} (a merge queue?)",
            s.to_lowercase()
        )),
        None => Err("gh merged it, but it couldn't be read back".into()),
    }
}

/// How one `merge_pr` call ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Merged(Box<MergeFacts>),
    /// A dry run, and it would.
    WouldMerge(Box<MergeFacts>),
    /// A rule failed.
    Refused(String),
    /// The user said no, or didn't answer.
    Denied(String),
    /// `gh` couldn't read or merge it.
    Failed(String),
}

/// `abc1234`.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// `squash at abc1234`.
fn how(f: &MergeFacts) -> String {
    format!("{} at {}", f.method.as_str(), short(&f.head))
}

/// The evidence row each attempt leaves on the task.
pub fn evidence(w: &Want, o: &Outcome) -> Link {
    let (ok, detail) = match o {
        Outcome::Merged(f) => (Some(true), format!("merged ({})", how(f))),
        Outcome::WouldMerge(f) => (None, format!("dry run: would merge ({})", how(f))),
        Outcome::Refused(why) => (Some(false), format!("refused: {why}")),
        Outcome::Denied(why) => (Some(false), format!("not merged: {why}")),
        Outcome::Failed(why) => (Some(false), format!("failed: {why}")),
    };
    Link {
        kind: LinkKind::Test,
        url: w.url(),
        label: mis_types::merge_label(&w.repo, w.number),
        ok,
        detail: Some(first_line(&detail, 200)),
        pr: None,
    }
}

/// What the agent hears: Ok if it merged or would, else Err with why not.
pub fn reply(w: &Want, o: &Outcome) -> Result<String, String> {
    let pr = format!("{}#{}", w.repo, w.number);
    match o {
        Outcome::Merged(f) => Ok(format!("merged {pr} ({})", how(f))),
        Outcome::WouldMerge(f) => Ok(format!(
            "dry run: would merge {pr} ({}); nothing was merged",
            how(f)
        )),
        Outcome::Refused(why) => Err(format!(
            "not merged: {pr} fails this routine's merge rules: {why}"
        )),
        Outcome::Denied(why) => Err(format!("not merged: {pr}: {why}")),
        Outcome::Failed(why) => Err(format!("not merged: {pr}: {why}")),
    }
}

/// The approval card asking the user to confirm merging `f`.
pub fn approval(f: &MergeFacts) -> Approval {
    let mut detail = format!(
        "{} #{}: {}\nby {} · {} files · +{} −{}\nchecks: {}\n{} ← {} · {}",
        f.repo,
        f.number,
        f.title,
        f.author,
        f.files,
        f.additions,
        f.deletions,
        f.checks,
        f.base,
        short(&f.head),
        f.method.as_str()
    );
    if detail.chars().count() > MAX_APPROVAL_DETAIL {
        detail = detail.chars().take(MAX_APPROVAL_DETAIL).collect();
    }
    Approval {
        id: uuid::Uuid::new_v4(),
        tool: mis_types::MERGE_TOOL.into(),
        summary: format!(
            "Merge {}#{}: {}",
            f.repo,
            f.number,
            first_line(&f.title, 100)
        ),
        detail,
        always: None,
        merge: Some(f.clone()),
    }
}

/// One task's `merge_pr`: its policy, whether the run is dry, and where it asks.
pub struct Merger {
    pub task: TaskId,
    pub policy: MergePolicy,
    pub dry: bool,
    pub approvals: Arc<Approvals>,
    /// `gh`, found on PATH unless a test says otherwise.
    pub gh: OsString,
}

impl Merger {
    /// Run one call: the tool's answer, and the evidence row if it was an attempt.
    pub async fn call(&self, args: &Value) -> (Result<String, String>, Link) {
        let w = match want(args, &self.policy) {
            Ok(w) => w,
            Err(why) => {
                let row = Link {
                    kind: LinkKind::Note,
                    url: String::new(),
                    label: first_line(&format!("merge_pr refused: {why}"), 200),
                    ok: None,
                    detail: None,
                    pr: None,
                };
                return (Err(format!("not merged: {why}")), row);
            }
        };
        let o = self.attempt(&w).await;
        tracing::info!(task = %self.task, "merge_pr {}#{}: {o:?}", w.repo, w.number);
        (reply(&w, &o), evidence(&w, &o))
    }

    async fn attempt(&self, w: &Want) -> Outcome {
        let facts = match self.eligible(w).await {
            Ok(f) => f,
            Err(o) => return o,
        };
        if self.dry {
            return Outcome::WouldMerge(Box::new(facts));
        }
        if self.policy.approve == MergeApprove::Phone {
            if let Err(why) = self.approvals.confirm(self.task, approval(&facts)).await {
                return Outcome::Denied(why.into());
            }
            // Read again: checks or labels may have changed while the user looked.
            match self.eligible(w).await {
                Ok(now) if (&now.head, &now.base) == (&facts.head, &facts.base) => {}
                Ok(_) => {
                    let why = "it was pushed to or retargeted after you were asked";
                    return Outcome::Refused(why.into());
                }
                Err(o) => return o,
            }
        }
        match merge_confirmed(&self.gh, w, self.policy.method, &facts.head).await {
            Ok(()) => Outcome::Merged(Box::new(facts)),
            Err(why) => Outcome::Failed(why),
        }
    }

    /// Read `w` and check it against the policy.
    async fn eligible(&self, w: &Want) -> Result<MergeFacts, Outcome> {
        let json = (self.gh(&view_args(w)).await)
            .map_err(|e| Outcome::Failed(format!("couldn't read the PR: {e}")))?;
        let pr = parse(&json).map_err(Outcome::Failed)?;
        check(&self.policy, w, &pr).map_err(Outcome::Refused)
    }

    async fn gh(&self, args: &[String]) -> Result<String, String> {
        run_gh(&self.gh, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> MergePolicy {
        MergePolicy {
            repos: vec!["tommy-xr/MakeItSo".into()],
            max_files: 10,
            max_lines: 300,
            authors: vec!["app/dependabot".into(), "tommy-xr".into()],
            forbid_paths: vec![".github/workflows/**".into(), "release/**".into()],
            require_labels: vec!["automerge".into()],
            method: MergeMethod::Squash,
            approve: MergeApprove::None,
        }
    }

    const HEAD: &str = "d68ea1f8a9b4b0890914ce16e5539e480e68c07b";

    /// `gh pr view --json FIELDS` for an eligible dependabot PR #7, then `edit`ed.
    fn pr(edit: impl FnOnce(&mut Value)) -> Pr {
        let mut v = json!({
            "number": 7, "title": "chore(deps): bump serde to 1.0.300",
            "url": "https://github.com/tommy-xr/MakeItSo/pull/7", "state": "OPEN",
            "author": {"id": "x", "is_bot": true, "login": "app/dependabot", "name": ""},
            "files": [
                {"path": "Cargo.lock", "additions": 4, "deletions": 4, "changeType": "MODIFIED"},
                {"path": "Cargo.toml", "additions": 1, "deletions": 1, "changeType": "MODIFIED"},
            ],
            "additions": 5, "deletions": 5, "changedFiles": 2, "isDraft": false,
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "labels": [{"id": "l", "name": "automerge", "description": "", "color": "fff"}],
            "headRefOid": HEAD,
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "check (ubuntu-latest)", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SKIPPED"},
                {"__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS"},
            ],
            "baseRefName": "main",
        });
        edit(&mut v);
        parse(&v.to_string()).unwrap()
    }

    fn want7() -> Want {
        Want {
            repo: "tommy-xr/MakeItSo".into(),
            number: 7,
        }
    }

    /// A change to the eligible PR's JSON.
    type Edit = fn(&mut Value);

    #[test]
    fn a_draft_pr_merges_open_ready_and_at_its_head_even_without_checks() {
        let w = want7();
        let ok = pr(|v| v["statusCheckRollup"] = json!([]));
        assert_eq!(check_draft(&w, &ok, HEAD), Ok(DraftMerge::Merge));
        let merged = pr(|v| v["state"] = json!("MERGED"));
        assert_eq!(check_draft(&w, &merged, HEAD), Ok(DraftMerge::Merged));
        let other = "a".repeat(40);
        assert!(check_draft(&w, &ok, &other)
            .unwrap_err()
            .contains("pushed to"));
        assert!(
            check_draft(&w, &merged, &other).is_err(),
            "merged at another head"
        );
        let refuse: [(Edit, &str); 6] = [
            (|v| v["state"] = json!("CLOSED"), "closed"),
            (|v| v["isDraft"] = json!(true), "draft PR"),
            (|v| v["number"] = json!(8), "#8"),
            (|v| v["mergeStateStatus"] = json!("BLOCKED"), "blocked"),
            (
                |v| v["statusCheckRollup"][0]["conclusion"] = json!("FAILURE"),
                "failed",
            ),
            (
                |v| v["statusCheckRollup"][0]["status"] = json!("IN_PROGRESS"),
                "running",
            ),
        ];
        for (edit, why) in refuse {
            let got = check_draft(&w, &pr(edit), HEAD).unwrap_err();
            assert!(got.contains(why), "{got}");
        }
    }

    #[test]
    fn github_remotes_name_their_repo() {
        for url in [
            "git@github.com:tommy-xr/mis-notes.git",
            "https://github.com/tommy-xr/mis-notes",
            "https://github.com/tommy-xr/mis-notes.git",
            "ssh://git@github.com/tommy-xr/mis-notes.git",
        ] {
            assert_eq!(
                github_repo(url).as_deref(),
                Some("tommy-xr/mis-notes"),
                "{url}"
            );
        }
        assert_eq!(github_repo("https://gitlab.com/o/r"), None);
        assert_eq!(github_repo("https://github.com/o/r/x"), None);
    }

    fn refused(edit: impl FnOnce(&mut Value)) -> String {
        check(&policy(), &want7(), &pr(edit)).unwrap_err()
    }

    #[test]
    fn an_eligible_pr_gives_the_facts_its_merge_pins() {
        let f = check(&policy(), &want7(), &pr(|_| {})).unwrap();
        assert_eq!(f.head, HEAD);
        assert_eq!((f.files, f.additions, f.deletions), (2, 5, 5));
        assert_eq!(f.checks, "2 passed, 1 skipped");
        assert_eq!(f.url, "https://github.com/tommy-xr/MakeItSo/pull/7");
        assert_eq!(
            merge_args(&want7(), f.method, &f.head),
            [
                "pr",
                "merge",
                "7",
                "-R",
                "tommy-xr/MakeItSo",
                "--squash",
                "--match-head-commit",
                HEAD
            ]
        );
        assert_eq!(
            view_args(&want7())[..5],
            ["pr", "view", "7", "-R", "tommy-xr/MakeItSo"]
        );
    }

    #[test]
    fn each_rule_refuses_with_its_reason() {
        let cases: Vec<(Edit, &str)> = vec![
            (|v| v["isDraft"] = json!(true), "it is a draft"),
            (|v| v["state"] = json!("MERGED"), "it is merged"),
            (|v| v["number"] = json!(8), "gh returned #8"),
            (
                (|v| v["author"]["login"] = json!("mallory")),
                "author mallory isn't in",
            ),
            (|v| v["changedFiles"] = json!(11), "11 files changed, over max_files 10"),
            (|v| v["changedFiles"] = json!(3), "gh listed 2 of its 3 files"),
            (|v| v["additions"] = json!(296), "301 lines changed, over max_lines 300"),
            (
                (|v| v["files"][1]["path"] = json!(".github/workflows/ci.yml")),
                "it changes .github/workflows/ci.yml, which forbid_paths .github/workflows/** covers",
            ),
            (
                (|v| v["files"][0]["changeType"] = json!("RENAMED")),
                "renames or copies Cargo.lock",
            ),
            (
                |v| {
                    v["files"][0].as_object_mut().unwrap().remove("changeType");
                },
                "renames or copies Cargo.lock (or gh didn't say)",
            ),
            (|v| v["labels"] = json!([]), "lacks the label automerge"),
            (|v| v["statusCheckRollup"] = json!([]), "no checks ran"),
            (
                |v| {
                    v["statusCheckRollup"] = json!([{"__typename": "CheckRun", "name": "build",
                        "status": "COMPLETED", "conclusion": "SKIPPED"}])
                },
                "no check passed",
            ),
            (
                (|v| v["statusCheckRollup"][0]["conclusion"] = json!("FAILURE")),
                "checks failed: check (ubuntu-latest)",
            ),
            (
                (|v| v["statusCheckRollup"][2]["state"] = json!("ERROR")),
                "checks failed: ci/legacy",
            ),
            (
                (|v| {
                    v["statusCheckRollup"][0]["status"] = json!("IN_PROGRESS");
                    v["statusCheckRollup"][0]["conclusion"] = json!("");
                }),
                "checks still running: check (ubuntu-latest)",
            ),
            (
                (|v| v["statusCheckRollup"][2]["state"] = json!("PENDING")),
                "checks still running: ci/legacy",
            ),
            (
                (|v| v["mergeable"] = json!("CONFLICTING")),
                "isn't ready to merge (conflicting, clean)",
            ),
            (
                (|v| v["mergeStateStatus"] = json!("BLOCKED")),
                "isn't ready to merge (mergeable, blocked)",
            ),
            (|v| v["headRefOid"] = json!("abc"), "unexpected head commit"),
        ];
        for (edit, why) in cases {
            let got = refused(edit);
            assert!(got.contains(why), "{got:?} lacks {why:?}");
        }
    }

    #[test]
    fn a_rerun_check_counts_only_its_newest_run() {
        let run = |at: &str, conclusion: &str| {
            json!({"__typename": "CheckRun", "name": "check", "workflowName": "CI",
                "status": "COMPLETED", "conclusion": conclusion, "startedAt": at})
        };
        let rerun = pr(|v| {
            v["statusCheckRollup"] = json!([
                run("2026-11-03T10:00:00Z", "FAILURE"),
                run("2026-11-03T11:00:00Z", "SUCCESS"),
            ])
        });
        let f = check(&policy(), &want7(), &rerun).unwrap();
        assert_eq!(f.checks, "1 passed");
        let queued_rerun = pr(|v| {
            let mut queued = run("", "");
            queued["status"] = json!("QUEUED");
            v["statusCheckRollup"] = json!([run("2026-11-03T11:00:00Z", "SUCCESS"), queued])
        });
        let err = check(&policy(), &want7(), &queued_rerun).unwrap_err();
        assert!(err.contains("checks still running: check"), "{err}");
        // A status named like a check run is a check of its own.
        let mixed = pr(|v| {
            v["statusCheckRollup"] = json!([
                run("2026-11-03T11:00:00Z", "SUCCESS"),
                {"__typename": "StatusContext", "context": "check", "state": "FAILURE"},
            ])
        });
        assert!(check(&policy(), &want7(), &mixed)
            .unwrap_err()
            .contains("checks failed: check"));
        let failed_again = pr(|v| {
            v["statusCheckRollup"] = json!([
                run("2026-11-03T11:00:00Z", "SUCCESS"),
                run("2026-11-03T12:00:00Z", "FAILURE"),
            ])
        });
        assert!(check(&policy(), &want7(), &failed_again).is_err());
    }

    #[test]
    fn an_empty_policy_list_requires_nothing_and_authors_ignore_case() {
        let mut p = policy();
        (p.require_labels, p.forbid_paths) = (vec![], vec![]);
        p.authors = vec!["APP/Dependabot".into()];
        let renamed = pr(|v| {
            v["labels"] = json!([]);
            v["files"][0]["changeType"] = json!("RENAMED");
        });
        assert!(check(&p, &want7(), &renamed).is_ok());
    }

    #[test]
    fn globs_match_within_and_across_segments() {
        for (g, path, hit) in [
            (".github/workflows/**", ".github/workflows/ci.yml", true),
            (".github/workflows/**", ".github/workflows/a/b.yml", true),
            (".github/workflows/**", ".github/dependabot.yml", false),
            ("release/**", "release/package.sh", true),
            ("release/**", "crates/release/x", false),
            ("**/Cargo.toml", "crates/a/Cargo.toml", true),
            ("**/Cargo.toml", "Cargo.toml", true),
            ("**/Cargo.toml", "xCargo.toml", false),
            ("*.md", "README.md", true),
            ("*.md", "docs/README.md", false),
            ("**.md", "docs/README.md", true),
            ("src/?.rs", "src/a.rs", true),
            ("src/?.rs", "src/ab.rs", false),
            ("Cargo.lock", "Cargo.lock", true),
            ("Cargo.lock", "Cargo.lockx", false),
        ] {
            assert_eq!(glob(g, path), hit, "{g} vs {path}");
        }
    }

    #[test]
    fn calls_name_a_listed_repo_and_a_number() {
        let p = policy();
        let w = want(&json!({"repo": "TOMMY-XR/makeitso", "number": 7}), &p).unwrap();
        assert_eq!(w, want7(), "spelled as the policy does");
        assert_eq!(
            want(&json!({"repo": "tommy-xr/MakeItSo", "number": "7"}), &p).unwrap(),
            want7()
        );
        for bad in [
            json!({"number": 7}),
            json!({"repo": "tommy-xr/MakeItSo"}),
            json!({"repo": "tommy-xr/MakeItSo", "number": 0}),
        ] {
            assert!(want(&bad, &p).is_err(), "{bad}");
        }
        let other = want(&json!({"repo": "evil/repo", "number": 7}), &p);
        assert_eq!(
            other,
            Err("repo evil/repo isn't in this routine's merge repos".into())
        );
        // Argv, never a shell: a repo can't smuggle flags past the policy.
        assert!(want(&json!({"repo": "--admin", "number": 7}), &p).is_err());
    }

    #[test]
    fn outcomes_report_as_evidence_and_replies() {
        let f = check(&policy(), &want7(), &pr(|_| {})).unwrap();
        let w = want7();
        let row = evidence(&w, &Outcome::Merged(Box::new(f.clone())));
        assert_eq!(
            (row.kind, row.label.as_str(), row.ok),
            (LinkKind::Test, "merge tommy-xr/MakeItSo#7", Some(true))
        );
        assert_eq!(row.detail.as_deref(), Some("merged (squash at d68ea1f)"));
        assert_eq!(row.url, "https://github.com/tommy-xr/MakeItSo/pull/7");
        let dry = evidence(&w, &Outcome::WouldMerge(Box::new(f.clone())));
        assert_eq!(
            (dry.ok, dry.detail.as_deref()),
            (None, Some("dry run: would merge (squash at d68ea1f)"))
        );
        let no = evidence(&w, &Outcome::Refused("it is a draft".into()));
        assert_eq!(
            (no.ok, no.detail.as_deref()),
            (Some(false), Some("refused: it is a draft"))
        );
        assert_eq!(
            reply(&w, &Outcome::Merged(Box::new(f.clone()))),
            Ok("merged tommy-xr/MakeItSo#7 (squash at d68ea1f)".into())
        );
        assert!(reply(&w, &Outcome::Denied("the user denied this".into()))
            .unwrap_err()
            .contains("denied"));
        let a = approval(&f);
        assert_eq!(
            a.summary,
            "Merge tommy-xr/MakeItSo#7: chore(deps): bump serde to 1.0.300"
        );
        assert_eq!((a.tool.as_str(), a.always.as_deref()), ("merge_pr", None));
        assert!(
            a.detail.contains("by app/dependabot · 2 files · +5 −5"),
            "{}",
            a.detail
        );
        assert_eq!(a.merge.as_ref(), Some(&f));
    }
}

//! A task's changes in its git repo, against the commit it started from: per-file
//! counts and bounded unified patches. Worker and orchestrator bound alike with
//! `TaskDiff::bounded`; the orchestrator because it never trusts a worker.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Files listed, of the task's and of earlier changes each; the rest of the task's are only counted (`TaskDiff::more`).
pub const MAX_FILES: usize = 300;
/// Largest patch kept for one file, and for all of a task's files together.
pub const MAX_FILE_PATCH: usize = 64 << 10;
pub const MAX_PATCH: usize = 256 << 10;
/// Paths with changes before the task that are told apart from its own.
pub const MAX_DIRTY: usize = 1000;
const MAX_PATH_CHARS: usize = 1024;
const MAX_NAME_CHARS: usize = 256;
const MAX_NOTE_CHARS: usize = 80;

/// Where a task started in its git repo. A reply in its thread starts from the
/// same base, so the thread's changes add up.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffBase {
    /// The repo's top level, as git prints it.
    pub root: String,
    /// HEAD's commit.
    pub head: String,
    /// Paths with uncommitted changes then (untracked included), each with a
    /// fingerprint of its content then: a file still matching it is no change of the task's.
    #[serde(default)]
    pub dirty: BTreeMap<String, String>,
}

/// The branch a task left its repo on, for offering a push or a PR.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RepoState {
    /// None when HEAD is detached.
    pub branch: Option<String>,
    /// Its upstream, e.g. `origin/fix-x`.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: u32,
    /// The origin's default branch, e.g. `main`, when git knows it.
    pub default_branch: Option<String>,
    /// Commits made since the task started.
    pub commits: u32,
    /// Some remote is on github.com.
    pub github: bool,
    /// The task's own changes are in the tree, not all committed.
    #[serde(default)]
    pub uncommitted: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileStatus {
    #[serde(rename = "A")]
    Added,
    #[serde(rename = "M")]
    Modified,
    #[serde(rename = "D")]
    Deleted,
    #[serde(rename = "R")]
    Renamed,
}

/// Whether a file already had uncommitted changes when the task started.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Before {
    #[default]
    Clean,
    /// Changed before the task, and untouched since: none of it is the task's.
    Only,
    /// Changed before the task, then by it: its patch holds both.
    Also,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileDiff {
    /// Relative to the repo's top level, `/`-separated.
    pub path: String,
    /// Renamed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub status: FileStatus,
    /// Lines; 0 for a binary file.
    pub added: u32,
    pub removed: u32,
    #[serde(default)]
    pub binary: bool,
    /// Not tracked by git: all of it counts as added.
    #[serde(default)]
    pub untracked: bool,
    #[serde(default)]
    pub before: Before,
    /// Its unified diff (git's header lines, then hunks); None when left out, `note` says why.
    pub patch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What a task changed in its repo since it (or its thread) started.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TaskDiff {
    pub base: DiffBase,
    pub repo: RepoState,
    /// By path.
    pub files: Vec<FileDiff>,
    /// The task's changed files past `MAX_FILES`, not listed.
    #[serde(default)]
    pub more: u32,
}

/// `TaskSummary::diff`: counts for lists, and the repo state for push/PR actions.
/// The files and patches are at `/api/tasks/<id>/diff`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffStat {
    /// Files the task changed (past `MAX_FILES` included), and their lines.
    pub files: u32,
    pub added: u32,
    pub removed: u32,
    pub repo: RepoState,
}

/// `s` cut to `max` chars.
fn cut(s: &mut String, max: usize) {
    if let Some((i, _)) = s.char_indices().nth(max) {
        s.truncate(i);
    }
}

impl TaskDiff {
    /// Within the limits: at most `MAX_FILES` listed, patches past `MAX_FILE_PATCH`
    /// each or `MAX_PATCH` in all dropped with a note, long strings cut.
    pub fn bounded(mut self) -> Self {
        // The task's files and earlier ones each up to the limit: the earlier are
        // named in a PR request, to be left out.
        let (mut mine, mut earlier): (Vec<_>, Vec<_>) =
            (self.files.drain(..)).partition(|f| f.before != Before::Only);
        if mine.len() > MAX_FILES {
            self.more += (mine.len() - MAX_FILES) as u32;
            mine.truncate(MAX_FILES);
        }
        earlier.truncate(MAX_FILES);
        self.files = mine.into_iter().chain(earlier).collect();
        let mut total = 0;
        for f in &mut self.files {
            cut(&mut f.path, MAX_PATH_CHARS);
            if let Some(from) = &mut f.from {
                cut(from, MAX_PATH_CHARS);
            }
            let size = f.patch.as_ref().map_or(0, String::len);
            if size > MAX_FILE_PATCH {
                f.patch = None;
                f.note = Some(format!("patch over {} KB", MAX_FILE_PATCH >> 10));
            } else if total + size > MAX_PATCH {
                f.patch = None;
                f.note = Some(format!("past the {} KB shown per task", MAX_PATCH >> 10));
            }
            total += f.patch.as_ref().map_or(0, String::len);
            if let Some(note) = &mut f.note {
                cut(note, MAX_NOTE_CHARS);
            }
        }
        let b = &mut self.base;
        cut(&mut b.root, MAX_PATH_CHARS);
        cut(&mut b.head, 64);
        if b.dirty.len() > MAX_DIRTY {
            b.dirty = std::mem::take(&mut b.dirty)
                .into_iter()
                .take(MAX_DIRTY)
                .collect();
        }
        b.dirty
            .retain(|p, fp| p.chars().count() <= MAX_PATH_CHARS && fp.len() <= 64);
        let r = &mut self.repo;
        for name in [&mut r.branch, &mut r.upstream, &mut r.default_branch]
            .into_iter()
            .flatten()
        {
            cut(name, MAX_NAME_CHARS);
        }
        self
    }

    pub fn stat(&self) -> DiffStat {
        let mine: Vec<_> = (self.files.iter())
            .filter(|f| f.before != Before::Only)
            .collect();
        DiffStat {
            files: mine.len() as u32 + self.more,
            added: mine.iter().map(|f| f.added).sum(),
            removed: mine.iter().map(|f| f.removed).sum(),
            repo: self.repo.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, patch: usize) -> FileDiff {
        FileDiff {
            path: path.into(),
            from: None,
            status: FileStatus::Modified,
            added: 2,
            removed: 1,
            binary: false,
            untracked: false,
            before: Before::Clean,
            patch: Some("x".repeat(patch)),
            note: None,
        }
    }

    #[test]
    fn bounded_drops_patches_past_the_limits_and_counts_unlisted_files() {
        let mut files: Vec<_> = (0..MAX_FILES + 5)
            .map(|i| file(&i.to_string(), 10))
            .collect();
        files[0].patch = Some("x".repeat(MAX_FILE_PATCH + 1));
        for f in &mut files[1..6] {
            f.patch = Some("x".repeat(MAX_FILE_PATCH));
        }
        files[2].path = "é".repeat(2000);
        // Of the five past the limit, the earlier changes aren't the task's.
        for f in &mut files[MAX_FILES..MAX_FILES + 2] {
            f.before = Before::Only;
        }
        let d = TaskDiff {
            files,
            ..Default::default()
        }
        .bounded();
        // Earlier ones are listed apart, up to the limit too.
        assert_eq!((d.files.len(), d.more), (MAX_FILES + 2, 3));
        assert_eq!(d.files[0].note.as_deref(), Some("patch over 64 KB"));
        // Four 64 KB patches fill the 256 KB; the fifth, and the small ones after, go.
        let kept: Vec<bool> = d.files[..8].iter().map(|f| f.patch.is_some()).collect();
        assert_eq!(kept, [false, true, true, true, true, false, false, false]);
        assert_eq!(
            d.files[5].note.as_deref(),
            Some("past the 256 KB shown per task")
        );
        assert_eq!(d.files[2].path.chars().count(), MAX_PATH_CHARS);
        let total: usize = d
            .files
            .iter()
            .filter_map(|f| f.patch.as_ref())
            .map(String::len)
            .sum();
        assert!(total <= MAX_PATCH);
    }

    #[test]
    fn stat_leaves_out_files_changed_only_before_the_task() {
        let mut only = file("a", 1);
        only.before = Before::Only;
        let mut also = file("b", 1);
        also.before = Before::Also;
        let d = TaskDiff {
            files: vec![only, also, file("c", 1)],
            more: 3,
            ..Default::default()
        };
        let s = d.stat();
        assert_eq!((s.files, s.added, s.removed), (5, 4, 2));
    }
}

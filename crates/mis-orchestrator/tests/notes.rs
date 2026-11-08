//! Publishing captures to a notes repo: each scenario runs the publisher against a temp
//! bare remote, with a human's clone beside it. Synthetic captures only.

use std::path::{Path, PathBuf};
use std::process::Command;

use mis_orchestrator::capture::{Capture, Inbox, Saved};
use mis_orchestrator::notes::{Notes, Published};
use mis_types::SyncState::{Conflict, Idle, Offline};

const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0 photo";

/// `git args` in `dir`; its stdout, trimmed. Panics if it fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "git {args:?}: {err}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A bare remote holding a README and `inbox/`, a human's clone of it, and a state dir.
struct World {
    dir: PathBuf,
    remote: PathBuf,
    human: PathBuf,
    state: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mis-notes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Self {
            remote: dir.join("remote.git"),
            human: dir.join("human"),
            state: dir.join("state"),
            dir,
        };
        git(
            &w.dir,
            &["init", "--quiet", "--bare", "-b", "main", "remote.git"],
        );
        git(&w.dir, &["clone", "--quiet", "remote.git", "human"]);
        for (k, v) in [
            ("user.name", "Human"),
            ("user.email", "human@example.com"),
            ("commit.gpgsign", "false"),
        ] {
            git(&w.human, &["config", k, v]);
        }
        std::fs::create_dir_all(w.human.join("inbox")).unwrap();
        std::fs::write(w.human.join("inbox/.gitkeep"), "").unwrap();
        std::fs::write(w.human.join("README.md"), "# notes\n").unwrap();
        w.human_commit("init");
        w
    }

    fn url(&self) -> String {
        self.remote.to_string_lossy().into_owned()
    }

    fn notes(&self) -> Notes {
        Notes::new(self.url(), &self.state)
    }

    /// Keep a capture as `POST /api/capture` would.
    fn capture(&self, text: &str, images: &[&[u8]]) -> Saved {
        let inbox = Inbox::open(self.state.join("captures/inbox")).unwrap();
        let images = images.iter().map(|b| b.to_vec()).collect();
        let c = Capture::new(None, Some(text.into()), None, images, None).unwrap();
        inbox.store(&c).unwrap()
    }

    fn receipt(&self, file: &str) -> Vec<u8> {
        std::fs::read(self.state.join("captures/inbox").join(file)).unwrap()
    }

    fn remote_git(&self, args: &[&str]) -> String {
        let mut all = vec!["--git-dir", self.remote.to_str().unwrap()];
        all.extend(args);
        git(&self.dir, &all)
    }

    /// Subjects on the remote's main, newest first.
    fn log(&self) -> Vec<String> {
        let log = self.remote_git(&["log", "--format=%s", "main"]);
        log.lines().map(Into::into).collect()
    }

    fn head(&self) -> String {
        self.remote_git(&["rev-parse", "main"])
    }

    fn files(&self) -> Vec<String> {
        let ls = self.remote_git(&["ls-tree", "-r", "--name-only", "main"]);
        ls.lines().map(Into::into).collect()
    }

    fn show(&self, path: &str) -> Vec<u8> {
        let out = Command::new("git")
            .args(["--git-dir", self.remote.to_str().unwrap(), "show"])
            .arg(format!("main:{path}"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{path}");
        out.stdout
    }

    fn map(&self) -> std::collections::BTreeMap<uuid::Uuid, Published> {
        let json = std::fs::read(self.state.join("captures/published.json")).unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    /// Commit everything in the human's clone and push it, after catching up.
    fn human_commit(&self, msg: &str) {
        git(&self.human, &["add", "-A"]);
        git(&self.human, &["commit", "--quiet", "-m", msg]);
        git(&self.human, &["push", "--quiet", "origin", "HEAD:main"]);
    }

    fn human_pull(&self) {
        git(
            &self.human,
            &["pull", "--quiet", "--ff-only", "origin", "main"],
        );
    }

    /// Run `sh` in the human's clone (caught up first) just before the publisher's
    /// next push leaves: someone pushing between its fetch and its push.
    fn on_next_push(&self, sh: &str) {
        let clone = self.state.join("notes");
        git(&clone, &["config", "core.hooksPath", ".git/hooks"]);
        let hook = clone.join(".git/hooks/pre-push");
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nrm -f \"$0\"\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL \
             GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL\n\
             cd '{}' && git pull --quiet --ff-only origin main && {sh}\n",
            self.human.display()
        );
        std::fs::write(&hook, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn image_of(file: &str) -> String {
    file.replace(".md", "-1.jpg")
}

#[test]
fn two_captures_make_one_commit_and_a_second_run_does_nothing() {
    let w = World::new();
    let a = w.capture("liminal inspiration for amf #amf", &[JPEG]);
    let b = w.capture("second thought", &[]);
    let notes = w.notes();
    let s = notes.run();
    assert_eq!((s.state, s.pending, &s.last_error), (Idle, 0, &None));
    assert!(s.last_push.is_some());
    assert_eq!(w.log(), ["capture: 2 items", "init"]);
    let (a_md, a_jpg, b_md) = (
        format!("inbox/{}", a.file),
        format!("inbox/{}", image_of(&a.file)),
        format!("inbox/{}", b.file),
    );
    let mut want = vec![
        "README.md".to_string(),
        "inbox/.gitkeep".into(),
        a_jpg.clone(),
        a_md.clone(),
        b_md.clone(),
    ];
    want.sort();
    assert_eq!(w.files(), want);
    // Records are the receipts byte for byte; receipts stay where they were.
    assert_eq!(w.show(&a_md), w.receipt(&a.file));
    assert_eq!(w.show(&a_jpg), JPEG);
    assert_eq!(w.show(&b_md), w.receipt(&b.file));
    let who = w.remote_git(&[
        "log",
        "-1",
        "--format=%an <%ae>|%cn <%ce>|%(trailers:key=MIS-Writer,valueonly,separator=)",
        "main",
    ]);
    assert_eq!(
        who,
        "Make It So <mis@localhost>|Make It So <mis@localhost>|capture"
    );
    let map = w.map();
    let head = w.head();
    assert_eq!(map.len(), 2);
    assert_eq!(
        map[&a.id],
        Published {
            path: a_md,
            commit: head.clone()
        }
    );
    assert_eq!(map[&b.id].path, b_md);
    let again = notes.run();
    assert_eq!((again.state, again.pending), (Idle, 0));
    assert_eq!(w.head(), head);
    // The single-item subject.
    w.capture("third", &[]);
    notes.run();
    assert_eq!(w.log()[0], "capture: 1 item");
}

#[test]
fn a_resend_after_restart_is_not_published_twice() {
    let w = World::new();
    let first = w.capture("resend me", &[JPEG]);
    assert_eq!(w.notes().run().state, Idle);
    let head = w.head();
    // Restarted: a new inbox and publisher over the same state dir.
    let again = w.capture("resend me", &[JPEG]);
    assert_eq!(
        (again.id, &again.file, again.new),
        (first.id, &first.file, false)
    );
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Idle, 0));
    assert_eq!(w.head(), head);
    assert_eq!(w.log(), ["capture: 1 item", "init"]);
}

#[test]
fn a_record_the_human_deleted_is_not_published_again() {
    let w = World::new();
    let a = w.capture("delete me later", &[]);
    w.notes().run();
    w.human_pull();
    std::fs::remove_file(w.human.join("inbox").join(&a.file)).unwrap();
    w.human_commit("drop a capture");
    let b = w.capture("a new one", &[]);
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Idle, 0));
    assert_eq!(
        w.log(),
        [
            "capture: 1 item",
            "drop a capture",
            "capture: 1 item",
            "init"
        ]
    );
    let files = w.files();
    assert!(!files.contains(&format!("inbox/{}", a.file)), "{files:?}");
    assert!(files.contains(&format!("inbox/{}", b.file)), "{files:?}");
}

#[test]
fn a_record_the_human_edited_is_left_alone() {
    let w = World::new();
    let a = w.capture("edit my frontmatter", &[]);
    w.notes().run();
    w.human_pull();
    let path = w.human.join("inbox").join(&a.file);
    let md = std::fs::read_to_string(&path).unwrap();
    let edited = md.replacen(
        "schema: 1\n",
        "schema: 1\ntitle: \"Renamed\"\npara: area\n",
        1,
    );
    assert_ne!(md, edited);
    std::fs::write(&path, &edited).unwrap();
    w.human_commit("title a capture");
    w.capture("another", &[]);
    assert_eq!(w.notes().run().state, Idle);
    assert_eq!(w.show(&format!("inbox/{}", a.file)), edited.as_bytes());
    assert_eq!(w.log()[..2], ["capture: 1 item", "title a capture"]);
}

#[test]
fn a_human_push_racing_the_publisher_is_rebased_onto() {
    let w = World::new();
    w.capture("first", &[]);
    w.notes().run();
    let b = w.capture("second", &[]);
    w.on_next_push(
        "mkdir -p projects && echo plan > projects/p.md && git add projects \
         && git commit --quiet -m 'human: project note' && git push --quiet origin HEAD:main",
    );
    let s = w.notes().run();
    assert_eq!((s.state, s.pending, &s.last_error), (Idle, 0, &None));
    assert!(
        !w.state.join("notes/.git/hooks/pre-push").exists(),
        "the race ran"
    );
    assert_eq!(
        w.log(),
        [
            "capture: 1 item",
            "human: project note",
            "capture: 1 item",
            "init"
        ]
    );
    // Rebased, not merged: one parent, the human's commit.
    let parents = w.remote_git(&["log", "-1", "--format=%P", "main"]);
    assert_eq!(parents, w.remote_git(&["rev-parse", "main~1"]));
    let files = w.files();
    assert!(files.contains(&"projects/p.md".to_string()));
    assert!(files.contains(&format!("inbox/{}", b.file)));
    assert_eq!(w.map()[&b.id].commit, w.head());
}

#[test]
fn a_human_push_to_the_same_path_is_a_conflict_and_the_remote_is_untouched() {
    let w = World::new();
    w.capture("first", &[]);
    w.notes().run();
    let a = w.capture("contested", &[]);
    w.on_next_push(&format!(
        "echo 'the human got here first' > inbox/{} && git add inbox \
         && git commit --quiet -m 'human: same name' && git push --quiet origin HEAD:main",
        a.file
    ));
    let notes = w.notes();
    let s = notes.run();
    assert_eq!((s.state, s.pending), (Conflict, 1));
    let clone = w.state.join("notes");
    let why = s.last_error.unwrap();
    assert_eq!(
        why,
        format!("notes repo conflict, resolve in {}", clone.display())
    );
    let human = w.head();
    assert_eq!(w.log()[0], "human: same name");
    assert_eq!(
        w.show(&format!("inbox/{}", a.file)),
        b"the human got here first\n"
    );
    assert!(!w.map().contains_key(&a.id), "still pending");
    assert!(!clone.join(".git/rebase-merge").exists(), "rebase aborted");
    // Stays a conflict, remote untouched, until a human resolves it.
    assert_eq!(notes.run().state, Conflict);
    assert_eq!(w.head(), human);
    // Resolved by dropping the publisher's commit: it publishes under the next free name.
    git(&clone, &["reset", "--quiet", "--hard", "origin/main"]);
    let s = notes.run();
    assert_eq!((s.state, s.pending, &s.last_error), (Idle, 0, &None));
    let renamed = format!("inbox/{}", a.file.replace(".md", "-2.md"));
    assert_eq!(w.show(&renamed), w.receipt(&a.file));
    assert_eq!(w.map()[&a.id].path, renamed);
}

#[test]
fn a_crash_after_pushing_adopts_instead_of_duplicating() {
    let w = World::new();
    let a = w.capture("pushed then crashed", &[JPEG]);
    let b = w.capture("me too", &[]);
    w.notes().run();
    let head = w.head();
    let map = w.map();
    // As if it stopped between the push and saving the map.
    std::fs::remove_file(w.state.join("captures/published.json")).unwrap();
    let s = w.notes().run();
    assert_eq!((s.state, s.pending, s.last_push), (Idle, 0, None));
    assert_eq!(w.head(), head, "nothing pushed");
    assert_eq!(w.map(), map);
    let files = w.files();
    assert_eq!(
        files.iter().filter(|f| f.ends_with(".md")).count(),
        3,
        "{files:?}"
    );
    assert!(
        files.contains(&format!("inbox/{}", a.file))
            && files.contains(&format!("inbox/{}", b.file))
    );
}

#[test]
fn an_unreachable_remote_is_offline_and_publishing_resumes_once_it_is_back() {
    let w = World::new();
    let a = w.capture("while offline", &[]);
    // No clone yet, remote URL wrong.
    let bad = Notes::new(w.dir.join("nope.git").to_string_lossy().into(), &w.state);
    let s = bad.run();
    assert_eq!((s.state, s.pending), (Offline, 1));
    assert!(s.last_error.unwrap().starts_with("clone:"));
    assert!(!w.state.join("captures/published.json").exists());
    assert_eq!(w.notes().run().state, Idle, "fixed URL");
    assert!(w.map().contains_key(&a.id));
    // Cloned, then the remote goes away and comes back.
    let b = w.capture("remote moved", &[]);
    let away = w.dir.join("away.git");
    std::fs::rename(&w.remote, &away).unwrap();
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Offline, 1));
    assert!(s.last_error.unwrap().starts_with("fetch:"));
    assert!(!w.map().contains_key(&b.id));
    std::fs::rename(&away, &w.remote).unwrap();
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Idle, 0));
    assert!(w.map().contains_key(&b.id));
    assert_eq!(w.log(), ["capture: 1 item", "capture: 1 item", "init"]);
}

#[test]
fn a_name_held_by_another_record_gets_a_number() {
    let w = World::new();
    let a = w.capture("same name", &[JPEG]);
    // Another record already has that name (say, from before a state dir was restored).
    let other = "---\nschema: 1\nid: 00000000-0000-0000-0000-000000000009\n\
                 captured: 2026-11-01T00:00:00Z\nvia: \"shortcut\"\nassets: []\n---\nolder\n";
    std::fs::write(w.human.join("inbox").join(&a.file), other).unwrap();
    w.human_commit("an older record");
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Idle, 0));
    let stem = a.file.strip_suffix(".md").unwrap();
    let (md, jpg) = (
        format!("inbox/{stem}-2.md"),
        format!("inbox/{stem}-2-1.jpg"),
    );
    assert_eq!(
        w.show(&format!("inbox/{}", a.file)),
        other.as_bytes(),
        "untouched"
    );
    assert_eq!(w.show(&jpg), JPEG);
    let receipt = String::from_utf8(w.receipt(&a.file)).unwrap();
    let want = receipt.replace(
        &format!("  - path: {stem}-1.jpg\n"),
        &format!("  - path: {stem}-2-1.jpg\n"),
    );
    assert_ne!(want, receipt);
    assert_eq!(String::from_utf8(w.show(&md)).unwrap(), want);
    assert_eq!(w.map()[&a.id].path, md);
}

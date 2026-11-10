//! Publishing captures to a notes repo: each scenario runs the publisher against a temp
//! bare remote, with a human's clone beside it. Synthetic captures only. Unix: the race
//! is injected with a shell script.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use mis_orchestrator::capture::{Capture, Inbox, Saved};
use mis_orchestrator::notes::{Filed, Notes, Published, Refused};
use mis_types::NoteMove;
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

    /// Run `sh` in the human's clone (caught up first) when the publisher's next push
    /// connects, before the remote lists its refs: someone pushing after its fetch.
    /// (Its hooks are off, so this wraps the remote's receive-pack instead.)
    fn on_next_push(&self, sh: &str) {
        let wrapper = self.dir.join("receive-pack");
        let once = self.dir.join("raced");
        let script = format!(
            "#!/bin/sh\nif [ ! -e '{once}' ]; then touch '{once}'; (\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_CONFIG_PARAMETERS GIT_CONFIG_COUNT \
             GIT_PROTOCOL GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL\n\
             cd '{}' && git pull --quiet --ff-only origin main && {sh}\n) >&2; fi\n\
             exec git-receive-pack \"$@\"\n",
            self.human.display(),
            once = once.display(),
        );
        std::fs::write(&wrapper, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let clone = self.state.join("notes");
        let wrapper = wrapper.to_str().unwrap();
        git(&clone, &["config", "remote.origin.receivepack", wrapper]);
    }

    /// Queue filing `path` in `dest` and run: how it went.
    fn file(&self, notes: &Notes, path: &str, dest: &str, hash: Option<&str>) -> Filed {
        let m = NoteMove {
            path: path.into(),
            dest: dest.into(),
            summary: Some("Pure cores".into()),
            hash: hash.map(Into::into),
        };
        let filed = notes.file(m);
        notes.run();
        filed.blocking_recv().unwrap()
    }

    /// Folders a note may be filed in, pushed by the human.
    fn folders(&self, files: &[&str]) {
        self.human_pull();
        for f in files {
            let at = self.human.join(f);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, "# here\n").unwrap();
        }
        self.human_commit("folders");
    }

    fn raced(&self) -> bool {
        self.dir.join("raced").exists()
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
fn a_run_with_nothing_to_publish_still_catches_the_clone_up() {
    let w = World::new();
    let notes = w.notes();
    let s = notes.run();
    assert_eq!((s.state, s.pending, &s.last_error), (Idle, 0, &None));
    assert!(w.state.join("notes/README.md").exists(), "cloned");
    std::fs::write(w.human.join("inbox/by-hand.md"), "# mine\n").unwrap();
    w.human_commit("by hand");
    let s = notes.run();
    assert_eq!((s.state, s.last_push), (Idle, None), "nothing pushed");
    let got = std::fs::read_to_string(w.state.join("notes/inbox/by-hand.md")).unwrap();
    assert_eq!(got, "# mine\n");
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
    assert!(w.raced());
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
    assert!(w.raced());
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
    // A human resolving it mid-rebase: the publisher leaves their rebase alone.
    let rebase = Command::new("git")
        .current_dir(&clone)
        .args([
            "-c",
            "user.name=Human",
            "-c",
            "user.email=human@example.com",
        ])
        .args(["rebase", "--quiet", "origin/main"])
        .output()
        .unwrap();
    assert!(!rebase.status.success(), "conflicts");
    assert_eq!(notes.run().state, Conflict);
    assert!(clone.join(".git/rebase-merge").exists(), "still theirs");
    git(&clone, &["rebase", "--abort"]);
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
fn a_lost_map_never_brings_back_what_the_human_deleted_or_filed() {
    let w = World::new();
    let a = w.capture("deleted while the map was lost", &[]);
    let b = w.capture("filed while the map was lost", &[]);
    w.notes().run();
    let map = w.map();
    // The map is lost (a crash after pushing, or an older state dir restored)...
    std::fs::remove_file(w.state.join("captures/published.json")).unwrap();
    // ...and meanwhile the human deleted one record and filed the other out of inbox/.
    w.human_pull();
    std::fs::remove_file(w.human.join("inbox").join(&a.file)).unwrap();
    std::fs::create_dir_all(w.human.join("projects")).unwrap();
    let filed = w.human.join("projects").join(&b.file);
    std::fs::rename(w.human.join("inbox").join(&b.file), filed).unwrap();
    w.human_commit("tidy the inbox");
    let c = w.capture("a new one", &[]);
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Idle, 0));
    assert_eq!(
        w.log(),
        [
            "capture: 1 item",
            "tidy the inbox",
            "capture: 2 items",
            "init"
        ]
    );
    let inbox: Vec<String> = (w.files().into_iter())
        .filter(|f| f.starts_with("inbox/") && f.ends_with(".md"))
        .collect();
    assert_eq!(inbox, [format!("inbox/{}", c.file)]);
    // Rebuilt from the repo's history: where each was published, by which commit.
    let rebuilt = w.map();
    assert_eq!(
        (rebuilt[&a.id].clone(), rebuilt[&b.id].clone()),
        (map[&a.id].clone(), map[&b.id].clone())
    );
}

#[test]
fn files_from_an_interrupted_run_are_cleared_and_a_humans_are_kept() {
    let w = World::new();
    w.capture("first", &[]);
    w.notes().run();
    let a = w.capture("cut short", &[JPEG]);
    // As if a run stopped after writing (and staging) a's files, before committing.
    let clone = w.state.join("notes");
    let (md, jpg) = (
        format!("inbox/{}", a.file),
        format!("inbox/{}", image_of(&a.file)),
    );
    std::fs::write(clone.join(&md), "half written").unwrap();
    std::fs::write(clone.join(&jpg), "half").unwrap();
    git(&clone, &["add", "--", &md, &jpg]);
    std::fs::write(clone.join(".git/mis-writing"), format!("{md}\n{jpg}")).unwrap();
    // A draft a human left in the clone, and a commit to rebase onto.
    std::fs::write(clone.join("inbox/draft.md"), "mine").unwrap();
    w.human_pull();
    std::fs::write(w.human.join("README.md"), "# notes, edited\n").unwrap();
    w.human_commit("human: readme");
    let s = w.notes().run();
    assert_eq!((s.state, s.pending, &s.last_error), (Idle, 0, &None));
    assert_eq!(w.show(&md), w.receipt(&a.file));
    assert_eq!(w.show(&jpg), JPEG);
    assert_eq!(
        std::fs::read_to_string(clone.join("inbox/draft.md")).unwrap(),
        "mine"
    );
    assert!(
        !w.files().contains(&"inbox/draft.md".to_string()),
        "not published"
    );
    assert!(!clone.join(".git/mis-writing").exists());
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

#[test]
fn a_humans_uncommitted_file_at_a_new_records_name_is_never_overwritten() {
    let w = World::new();
    w.capture("first", &[]);
    w.notes().run();
    let a = w.capture("same name as a draft", &[]);
    let draft = w.state.join("notes/inbox").join(&a.file);
    std::fs::write(&draft, "a draft, not committed").unwrap();
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Idle, 0));
    assert_eq!(
        std::fs::read_to_string(&draft).unwrap(),
        "a draft, not committed"
    );
    let renamed = format!("inbox/{}", a.file.replace(".md", "-2.md"));
    assert_eq!(w.show(&renamed), w.receipt(&a.file));
    assert!(
        !w.files().contains(&format!("inbox/{}", a.file)),
        "draft not published"
    );
}

#[test]
fn a_crash_after_committing_keeps_and_pushes_the_commit() {
    let w = World::new();
    w.capture("first", &[]);
    w.notes().run();
    let a = w.capture("committed, then crashed", &[]);
    // A run that committed, then stopped before removing its marker and pushing.
    let clone = w.state.join("notes");
    let md = format!("inbox/{}", a.file);
    let receipt = w.state.join("captures/inbox").join(&a.file);
    std::fs::copy(receipt, clone.join(&md)).unwrap();
    git(&clone, &["add", "--", &md]);
    let msg = format!(
        "capture: 1 item\n\nMIS-Writer: capture\nMIS-Capture: {} {md}\n",
        a.id
    );
    let who = [
        "-c",
        "user.name=Make It So",
        "-c",
        "user.email=mis@localhost",
    ];
    git(
        &clone,
        &[&who[..], &["commit", "--quiet", "-m", &msg]].concat(),
    );
    std::fs::write(clone.join(".git/mis-writing"), &md).unwrap();
    let s = w.notes().run();
    assert_eq!((s.state, s.pending), (Idle, 0));
    assert_eq!(w.show(&md), w.receipt(&a.file));
    assert_eq!(w.log()[..2], ["capture: 1 item", "capture: 1 item"]);
    assert_eq!(w.map()[&a.id].commit, w.head());
    assert!(!clone.join(".git/mis-writing").exists());
}

#[test]
fn a_run_with_nothing_to_publish_pushes_a_commit_left_unpushed() {
    let w = World::new();
    w.capture("first", &[]);
    w.notes().run();
    // A commit made while the remote was away (a filing), with no capture pending.
    let clone = w.state.join("notes");
    std::fs::write(clone.join("inbox/later.md"), "later\n").unwrap();
    git(&clone, &["add", "--", "inbox/later.md"]);
    let who = [
        "-c",
        "user.name=Make It So",
        "-c",
        "user.email=mis@localhost",
    ];
    git(
        &clone,
        &[&who[..], &["commit", "--quiet", "-m", "offline"]].concat(),
    );
    let s = w.notes().run();
    assert_eq!((s.state, s.pending, &s.last_error), (Idle, 0, &None));
    assert!(s.last_push.is_some());
    assert_eq!(w.log()[0], "offline");
}

/// sha256 hex of `bytes`, as `POST /api/notes/move` takes it.
fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn filing_moves_a_note_and_its_image_in_one_pushed_commit() {
    let w = World::new();
    let a = w.capture("liminal inspiration for amf #mis", &[JPEG]);
    let notes = w.notes();
    notes.run();
    w.folders(&["projects/amf/README.md"]);
    let (md, jpg) = (
        format!("inbox/{}", a.file),
        format!("inbox/{}", image_of(&a.file)),
    );
    let hash = sha256(&w.show(&md));
    let got = w.file(&notes, &md, "projects/amf", Some(&hash)).unwrap();
    let to = format!("projects/amf/{}", a.file);
    assert_eq!(got.path, to);
    let files = w.files();
    assert!(!files.contains(&md) && !files.contains(&jpg), "{files:?}");
    assert_eq!(w.show(&format!("projects/amf/{}", image_of(&a.file))), JPEG);
    // The frontmatter gains a summary and the project; the rest is as captured.
    let receipt = String::from_utf8(w.receipt(&a.file)).unwrap();
    let want = receipt.replace("project: \"mis\"\n", "").replacen(
        "\n---\n",
        "\nsummary: \"Pure cores\"\nproject: \"amf\"\n---\n",
        1,
    );
    assert_eq!(String::from_utf8(w.show(&to)).unwrap(), want);
    assert_eq!(
        w.log()[..2],
        [format!("file: {md} -> projects/amf/"), "folders".into()]
    );
    let trailers = w.remote_git(&["log", "-1", "--format=%(trailers)", "main"]);
    assert_eq!(
        trailers,
        format!("MIS-Writer: human\nMIS-Filed: {} {md} {to}", a.id)
    );
    let s = notes.status();
    assert_eq!((s.state, s.pending, &s.last_error), (Idle, 0, &None));
    // Published once: a later run doesn't bring it back to the inbox.
    let head = w.head();
    notes.run();
    assert_eq!(w.head(), head);
}

#[test]
fn a_name_taken_where_a_note_is_filed_gets_a_number() {
    let w = World::new();
    let a = w.capture("same name", &[JPEG]);
    let notes = w.notes();
    notes.run();
    let stem = a.file.strip_suffix(".md").unwrap();
    w.folders(&[&format!("archive/{}", a.file)]);
    let got = w
        .file(&notes, &format!("inbox/{}", a.file), "archive", None)
        .unwrap();
    assert_eq!(got.path, format!("archive/{stem}-2.md"));
    assert_eq!(
        w.show(&format!("archive/{}", a.file)),
        b"# here\n",
        "untouched"
    );
    assert_eq!(w.show(&format!("archive/{stem}-2-1.jpg")), JPEG);
    let filed = String::from_utf8(w.show(&got.path)).unwrap();
    assert!(
        filed.contains(&format!("  - path: {stem}-2-1.jpg\n")),
        "{filed}"
    );
}

#[test]
fn a_note_changed_gone_or_edited_in_the_clone_is_not_filed() {
    let w = World::new();
    let a = w.capture("changing", &[]);
    let b = w.capture("going", &[]);
    let notes = w.notes();
    notes.run();
    w.folders(&["areas/home/boiler.md"]);
    let (a_md, b_md) = (format!("inbox/{}", a.file), format!("inbox/{}", b.file));
    let read = sha256(&w.show(&a_md));
    // The human edits one and deletes the other after the view read them.
    w.human_pull();
    std::fs::write(w.human.join(&a_md), "edited\n").unwrap();
    std::fs::remove_file(w.human.join(&b_md)).unwrap();
    w.human_commit("edit and delete");
    let head = w.head();
    let stale = |f: Filed| matches!(f, Err(Refused::Stale(_)));
    assert!(stale(w.file(&notes, &a_md, "areas/home", Some(&read))));
    assert!(stale(w.file(&notes, &b_md, "areas/home", None)));
    // A folder HEAD doesn't have.
    let got = w.file(&notes, &a_md, "areas/nope", None);
    assert!(stale(got));
    // An edit not committed in the clone (the rebase stops first).
    let clone = w.state.join("notes");
    std::fs::write(clone.join(&a_md), "uncommitted\n").unwrap();
    assert!(w.file(&notes, &a_md, "areas/home", None).is_err());
    assert_eq!(
        std::fs::read_to_string(clone.join(&a_md)).unwrap(),
        "uncommitted\n"
    );
    assert_eq!(w.head(), head, "nothing filed");
    git(&clone, &["checkout", "--", &a_md]);
    let current = sha256(b"edited\n");
    let got = w.file(&notes, &a_md, "areas/home", Some(&current)).unwrap();
    assert_eq!(got.path, format!("areas/home/{}", a.file));
}

#[test]
fn filing_offline_is_refused_and_nothing_is_committed() {
    let w = World::new();
    let a = w.capture("filed on a plane", &[]);
    let notes = w.notes();
    notes.run();
    w.folders(&["resources/rust/tips.md"]);
    notes.run();
    let clone = w.state.join("notes");
    let head = git(&clone, &["rev-parse", "HEAD"]);
    std::fs::rename(&w.remote, w.dir.join("away.git")).unwrap();
    let got = w.file(&notes, &format!("inbox/{}", a.file), "resources/rust", None);
    let Err(Refused::Failed(why)) = got else {
        panic!("{got:?}")
    };
    assert!(
        why.starts_with("offline; try again when connected"),
        "{why}"
    );
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
}

/// Commit `link` as a symlink to `to` in the human's clone.
#[cfg(unix)]
fn symlink(w: &World, link: &str, to: &str) {
    w.human_pull();
    let at = w.human.join(link);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(to, at).unwrap();
    w.human_commit("a symlink");
}

#[test]
fn filing_never_goes_through_a_symlink() {
    let w = World::new();
    let a = w.capture("not out of the clone", &[JPEG]);
    let notes = w.notes();
    notes.run();
    let md = format!("inbox/{}", a.file);
    // `archive` committed as a link out of the clone; then one only in the clone.
    let outside = w.state.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&w, "archive", "../outside");
    let got = w.file(&notes, &md, "archive", None);
    assert!(got.is_err(), "{got:?}");
    let head = w.head();
    git(&w.human, &["rm", "--quiet", "archive"]);
    w.human_commit("no archive");
    let clone = w.state.join("notes");
    notes.run();
    std::os::unix::fs::symlink(&outside, clone.join("archive")).unwrap();
    let got = w.file(&notes, &md, "archive", None);
    assert!(got.is_err(), "{got:?}");
    // A folder HEAD has, swapped for a link in the clone only.
    std::fs::remove_file(clone.join("archive")).unwrap();
    w.folders(&["areas/home/boiler.md"]);
    notes.run();
    std::fs::remove_dir_all(clone.join("areas/home")).unwrap();
    std::os::unix::fs::symlink(&outside, clone.join("areas/home")).unwrap();
    let got = w.file(&notes, &md, "areas/home", None);
    assert!(got.is_err(), "{got:?}");
    std::fs::remove_file(clone.join("areas/home")).unwrap();
    git(&clone, &["checkout", "--", "areas/home"]);
    // A project folder that is a link.
    symlink(&w, "projects/amf", "../../outside");
    let got = w.file(&notes, &md, "projects/amf", None);
    assert!(got.is_err(), "{got:?}");
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing outside"
    );
    assert!(w.files().contains(&md));
    assert_ne!(w.head(), head);
}

#[test]
fn only_the_notes_own_pictures_go_with_it() {
    let w = World::new();
    let a = w.capture("with a picture", &[JPEG]);
    let notes = w.notes();
    notes.run();
    w.folders(&["areas/home/boiler.md"]);
    let (md, jpg) = (
        format!("inbox/{}", a.file),
        format!("inbox/{}", image_of(&a.file)),
    );
    // Another note lists the same picture: not moved out from under it.
    let other = format!(
        "---\nid: 00000000-0000-0000-0000-000000000009\ncaptured: 2026-11-01T00:00:00Z\n\
         assets:\n  - path: {}\n---\nmine too\n",
        image_of(&a.file)
    );
    w.human_pull();
    std::fs::write(w.human.join("inbox/other.md"), &other).unwrap();
    w.human_commit("share a picture");
    let got = w.file(&notes, &md, "areas/home", None);
    assert!(matches!(got, Err(Refused::Stale(_))), "{got:?}");
    // It lists a note and a link as its pictures: neither goes.
    w.human_pull();
    std::fs::remove_file(w.human.join("inbox/other.md")).unwrap();
    std::fs::write(w.human.join("inbox/other.md"), "# other\n").unwrap();
    let path = w.human.join(&md);
    let listed = std::fs::read_to_string(&path).unwrap().replace(
        "assets:\n",
        "assets:\n  - path: other.md\n    sha256: x\n  - path: link.jpg\n    sha256: x\n",
    );
    std::fs::write(&path, listed).unwrap();
    std::os::unix::fs::symlink("../README.md", w.human.join("inbox/link.jpg")).unwrap();
    w.human_commit("odd assets");
    let got = w.file(&notes, &md, "areas/home", None).unwrap();
    assert_eq!(got.path, format!("areas/home/{}", a.file));
    let files = w.files();
    for stays in ["inbox/other.md", "inbox/link.jpg"] {
        assert!(files.contains(&stays.to_string()), "{stays}: {files:?}");
    }
    assert!(files.contains(&format!("areas/home/{}", image_of(&a.file))));
    assert!(!files.contains(&jpg));
}

#[test]
fn a_name_held_by_an_uncommitted_file_with_dots_gets_a_number() {
    let w = World::new();
    let notes = w.notes();
    w.folders(&["inbox/v1.2-notes.md", "archive/2025/retro.md"]);
    notes.run();
    let clone = w.state.join("notes");
    std::fs::write(clone.join("archive/v1.2-notes.md"), "mine").unwrap();
    let got = w
        .file(&notes, "inbox/v1.2-notes.md", "archive", None)
        .unwrap();
    assert_eq!(got.path, "archive/v1.2-notes-2.md");
    let mine = std::fs::read_to_string(clone.join("archive/v1.2-notes.md")).unwrap();
    assert_eq!(mine, "mine");
}

#[test]
fn frontmatter_keeps_its_line_endings_and_loses_no_lines() {
    let w = World::new();
    let notes = w.notes();
    w.folders(&["projects/amf/README.md"]);
    let crlf =
        "---\r\ntitle: x\r\nsummary: |\r\n  old\r\n\r\n  text\r\nproject:\r\n  - a\r\n- b\r\n\
                tags: [t]\r\n---\r\nbody\r\nproject: kept\r\n";
    w.human_pull();
    std::fs::write(w.human.join("inbox/crlf.md"), crlf).unwrap();
    std::fs::write(
        w.human.join("inbox/open.md"),
        "---\ntitle: never closed\nbody\n",
    )
    .unwrap();
    std::fs::write(w.human.join("inbox/empty.md"), "---\n---\nbody\n").unwrap();
    w.human_commit("notes");
    notes.run();
    let got = w
        .file(&notes, "inbox/crlf.md", "projects/amf", None)
        .unwrap();
    let want =
        "---\r\ntitle: x\r\ntags: [t]\r\nsummary: \"Pure cores\"\r\nproject: \"amf\"\r\n---\r\n\
                body\r\nproject: kept\r\n";
    assert_eq!(String::from_utf8(w.show(&got.path)).unwrap(), want);
    let got = w
        .file(&notes, "inbox/empty.md", "projects/amf", None)
        .unwrap();
    let want = "---\nsummary: \"Pure cores\"\nproject: \"amf\"\n---\nbody\n";
    assert_eq!(String::from_utf8(w.show(&got.path)).unwrap(), want);
    let got = w.file(&notes, "inbox/open.md", "projects/amf", None);
    assert!(matches!(got, Err(Refused::Stale(_))), "{got:?}");
    assert!(w.files().contains(&"inbox/open.md".to_string()));
}

#[test]
fn a_picture_any_other_note_lists_stays() {
    let w = World::new();
    let a = w.capture("shared picture", &[JPEG]);
    let notes = w.notes();
    notes.run();
    w.folders(&["areas/home/boiler.md"]);
    // A human's note, no capture id, named `.MD`.
    w.human_pull();
    let other = format!(
        "---\ntitle: mine\nassets:\n  - path: {}\n---\n",
        image_of(&a.file)
    );
    std::fs::write(w.human.join("inbox/Other.MD"), other).unwrap();
    w.human_commit("another note shows it");
    let got = w.file(&notes, &format!("inbox/{}", a.file), "areas/home", None);
    assert!(matches!(got, Err(Refused::Stale(_))), "{got:?}");
}

#[test]
fn a_note_checked_out_with_crlf_is_hashed_as_the_view_reads_it() {
    let w = World::new();
    w.human_pull();
    std::fs::write(w.human.join(".gitattributes"), "*.md text eol=crlf\n").unwrap();
    std::fs::write(w.human.join("inbox/n.md"), "---\ntitle: x\n---\nhi\n").unwrap();
    std::fs::create_dir_all(w.human.join("archive")).unwrap();
    std::fs::write(w.human.join("archive/.keep"), "").unwrap();
    w.human_commit("crlf on checkout");
    let notes = w.notes();
    notes.run();
    let clone = w.state.join("notes");
    let shown = std::fs::read(clone.join("inbox/n.md")).unwrap();
    assert_eq!(shown, b"---\r\ntitle: x\r\n---\r\nhi\r\n");
    let got = w
        .file(&notes, "inbox/n.md", "archive", Some(&sha256(&shown)))
        .unwrap();
    let want = "---\ntitle: x\nsummary: \"Pure cores\"\n---\nhi\n";
    assert_eq!(String::from_utf8(w.show(&got.path)).unwrap(), want);
}

#[test]
fn a_file_the_clone_doesnt_track_in_the_way_stops_the_filing() {
    let w = World::new();
    let a = w.capture("in the way", &[]);
    let notes = w.notes();
    notes.run();
    // `archive` isn't in HEAD; a human's file of that name is in the clone.
    let clone = w.state.join("notes");
    std::fs::write(clone.join("archive"), "mine").unwrap();
    let head = git(&clone, &["rev-parse", "HEAD"]);
    let got = w.file(&notes, &format!("inbox/{}", a.file), "archive", None);
    let Err(Refused::Stale(why)) = got else {
        panic!("{got:?}")
    };
    assert!(why.starts_with("merge:"), "{why}");
    assert_eq!(
        std::fs::read_to_string(clone.join("archive")).unwrap(),
        "mine"
    );
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        head,
        "nothing committed"
    );
    assert!(clone.join(format!("inbox/{}", a.file)).exists());
    assert_eq!(git(&clone, &["status", "--porcelain"]), "?? archive");
}

#[test]
fn a_filing_cut_short_before_it_lands_changes_nothing() {
    let w = World::new();
    let a = w.capture("cut short", &[JPEG]);
    let notes = w.notes();
    notes.run();
    w.folders(&["projects/amf/README.md"]);
    notes.run();
    // What a run stopped between `commit-tree` and the merge leaves: its index, its note,
    // an unreferenced commit.
    let clone = w.state.join("notes");
    std::fs::write(clone.join(".git/mis-filing-index"), "garbage").unwrap();
    std::fs::write(clone.join(".git/mis-filed"), "half").unwrap();
    let tree = git(&clone, &["rev-parse", "HEAD^{tree}"]);
    let who = [
        "-c",
        "user.name=Make It So",
        "-c",
        "user.email=mis@localhost",
    ];
    git(
        &clone,
        &[&who[..], &["commit-tree", &tree, "-p", "HEAD", "-m", "x"]].concat(),
    );
    let head = w.head();
    let s = notes.run();
    assert_eq!((s.state, &s.last_error), (Idle, &None));
    assert_eq!(w.head(), head, "nothing pushed");
    assert_eq!(git(&clone, &["status", "--porcelain"]), "");
    // And the next filing is whole.
    let md = format!("inbox/{}", a.file);
    let got = w.file(&notes, &md, "projects/amf", None).unwrap();
    let files = w.files();
    assert!(files.contains(&got.path) && !files.contains(&md));
    assert!(files.contains(&format!("projects/amf/{}", image_of(&a.file))));
    assert!(
        !clone.join(".git/mis-filing-index").exists() && !clone.join(".git/mis-filed").exists()
    );
}

#[test]
fn a_note_too_large_to_hash_as_shown_is_refused_not_cut() {
    let w = World::new();
    let notes = w.notes();
    w.human_pull();
    let big = format!("---\ntitle: big\n---\n{}", "x".repeat(20 << 20));
    std::fs::write(w.human.join("inbox/big.md"), &big).unwrap();
    std::fs::create_dir_all(w.human.join("archive")).unwrap();
    std::fs::write(w.human.join("archive/.keep"), "").unwrap();
    w.human_commit("a big note");
    notes.run();
    let head = w.head();
    let got = w.file(&notes, "inbox/big.md", "archive", None);
    let Err(Refused::Stale(why)) = got else {
        panic!("{got:?}")
    };
    assert!(why.ends_with("is over 20 MiB"), "{why}");
    assert_eq!(w.head(), head);
    assert_eq!(w.show("inbox/big.md").len(), big.len());
}

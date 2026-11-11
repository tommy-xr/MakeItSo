//! The core's builds as `mis-up --watch` reports them, and the owner's choice among them: what
//! the page shows, and which build the watcher starts next. A new build starts only once the
//! owner chooses it (or the watcher runs with `--auto-update`), and only while no task runs.

use mis_types::*;

use super::{Effect, Ledger, MAX_SUBJECT};

/// Commits kept per list in a report.
const MAX_COMMITS: usize = 50;
const MAX_ORIGIN: usize = 300;

/// Err unless `s` is 7-40 hex digits.
pub(super) fn sha(s: &str) -> Result<(), String> {
    match (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        true => Ok(()),
        false => Err(format!("sha {s:?}: want 7-40 hex digits")),
    }
}

/// `(#N)` at the end of a squash-merge subject: `("feat: x", Some(N))`.
fn split_pr(subject: &str) -> (&str, Option<u64>) {
    let pr = (subject.strip_suffix(')'))
        .and_then(|s| s.rsplit_once(" (#"))
        .and_then(|(rest, n)| Some((rest, n.parse().ok()?)));
    pr.map_or((subject, None), |(rest, n)| (rest, Some(n)))
}

/// Report `r` with its shas checked, its lists bounded and each subject one line, its
/// `(#N)` moved to `pr`.
pub fn checked(mut r: Builds) -> Result<Builds, String> {
    sha(&r.running)?;
    for s in r.ready.iter().chain(&r.previous) {
        sha(s)?;
    }
    for list in [&mut r.incoming, &mut r.outgoing] {
        list.truncate(MAX_COMMITS);
        for c in list.iter_mut() {
            sha(&c.sha)?;
            let line = first_line(&c.subject, MAX_SUBJECT);
            let (subject, pr) = split_pr(&line);
            (c.subject, c.pr) = (subject.to_string(), pr);
        }
    }
    r.origin = first_line(&r.origin, MAX_ORIGIN);
    Ok(r)
}

/// What the page shows of report `r`, the owner having chosen `chosen`. Update offers the ready
/// build; Roll back the previous one, unless this build is a roll back (it would go forward).
pub fn view(r: &Builds, chosen: Option<&str>) -> CoreUpdate {
    let ready = r.ready.as_ref().filter(|s| **s != r.running);
    let back = r
        .previous
        .as_ref()
        .filter(|s| **s != r.running && Some(*s) != ready && r.started != Started::RollBack);
    let change = |sha: &String, commits: &Vec<Commit>| Change {
        sha: sha.clone(),
        commits: commits.clone(),
    };
    let ready = ready.map(|s| change(s, &r.incoming));
    let back = back.map(|s| change(s, &r.outgoing));
    let offered = |c: &str| [&ready, &back].into_iter().flatten().any(|o| o.sha == c);
    CoreUpdate {
        running: r.running.clone(),
        started: r.started,
        repo: github_repo(&r.origin),
        asked: chosen.filter(|c| offered(c)).map(String::from),
        kept: chosen == Some(r.running.as_str()),
        ready,
        back,
        auto: r.auto,
    }
}

/// The build to start now, once no task runs (`busy` is how many do): the one the owner chose;
/// else, with `auto`, the ready one, unless the owner rolled back from it.
pub fn next(r: &Builds, chosen: Option<&str>, busy: usize) -> Option<String> {
    let v = view(r, chosen);
    let auto = (r.auto && r.ready != r.previous).then(|| v.ready.map(|c| c.sha));
    (busy == 0).then(|| v.asked.or(auto.flatten())).flatten()
}

impl Ledger {
    /// The watcher reports its builds; Ok: the build it should start now, if any.
    pub fn report_builds(&mut self, r: Builds) -> Result<(Option<String>, Vec<Effect>), String> {
        let r = checked(r)?;
        // A choice no longer offered is dropped, so it can't come back with a later report.
        let v = view(&r, self.chosen.as_deref());
        if v.asked.is_none() && !v.kept {
            self.chosen = None;
        }
        let start = next(&r, self.chosen.as_deref(), self.running());
        if self.builds.as_ref() == Some(&r) {
            return Ok((start, vec![]));
        }
        self.builds = Some(r);
        Ok((start, vec![Effect::Broadcast(self.status())]))
    }

    /// The owner chooses build `sha`: the ready one (Update), the previous one (Roll back), or
    /// the running one (keep it).
    pub fn choose_build(&mut self, sha: &str) -> Result<Vec<Effect>, String> {
        let r = self.builds.as_ref().ok_or("no builds reported")?;
        let v = view(r, Some(sha));
        if v.asked.is_none() && !v.kept {
            return Err(format!("{sha} is not a build to choose"));
        }
        self.chosen = Some(sha.to_string());
        Ok(vec![Effect::Broadcast(self.status())])
    }

    pub fn core_update(&self) -> Option<CoreUpdate> {
        let r = self.builds.as_ref()?;
        Some(view(r, self.chosen.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn commit(sha: &str, subject: &str) -> Commit {
        Commit {
            sha: sha.into(),
            subject: subject.into(),
            pr: None,
        }
    }

    /// Running A, B ready.
    fn ready() -> Builds {
        Builds {
            running: A.into(),
            ready: Some(B.into()),
            incoming: vec![commit(B, "feat: core updates wait (#7)")],
            origin: "git@github.com:demo/mis.git".into(),
            ..Builds::default()
        }
    }

    /// Updated onto B from A.
    fn updated() -> Builds {
        Builds {
            running: B.into(),
            started: Started::Update,
            previous: Some(A.into()),
            outgoing: vec![commit(B, "feat: core updates wait (#7)")],
            ..Builds::default()
        }
    }

    fn ledger(r: Builds) -> Ledger {
        let mut l = Ledger::default();
        l.report_builds(r).unwrap();
        l
    }

    #[test]
    fn reports_are_checked() {
        let r = checked(ready()).unwrap();
        assert_eq!(r.incoming[0].subject, "feat: core updates wait");
        assert_eq!(r.incoming[0].pr, Some(7));
        assert_eq!(view(&r, None).repo.as_deref(), Some("demo/mis"));
        for s in ["", "(#x)", "a (#7) b", "fix (#)"] {
            assert_eq!(split_pr(s), (s, None), "{s:?}");
        }
        let mut bad = ready();
        bad.ready = Some("main".into());
        assert!(checked(bad).is_err());
        let mut bad = ready();
        bad.incoming[0].sha = "zz".into();
        assert!(checked(bad).is_err());
        let mut long = ready();
        long.incoming = vec![commit(B, "x"); 80];
        assert_eq!(checked(long).unwrap().incoming.len(), MAX_COMMITS);
    }

    #[test]
    fn a_ready_build_waits_for_the_owner_then_for_no_task() {
        let mut l = ledger(ready());
        let v = l.core_update().unwrap();
        assert_eq!(v.ready.as_ref().unwrap().sha, B);
        assert_eq!((v.back, v.asked), (None, None));
        assert_eq!(
            l.report_builds(ready()).unwrap(),
            (None, vec![]),
            "unchanged: no broadcast"
        );

        assert!(l.choose_build("cccccccc").is_err(), "not offered");
        assert_eq!(l.choose_build(B).unwrap().len(), 1);
        assert_eq!(l.core_update().unwrap().asked.as_deref(), Some(B));
        assert_eq!(
            next(l.builds.as_ref().unwrap(), Some(B), 1),
            None,
            "a task runs"
        );
        assert_eq!(l.report_builds(ready()).unwrap().0.as_deref(), Some(B));
    }

    #[test]
    fn a_newer_ready_build_needs_choosing_again() {
        let mut l = ledger(ready());
        l.choose_build(B).unwrap();
        let mut newer = ready();
        newer.ready = Some("cccccccccccccccc".into());
        let (start, _) = l.report_builds(newer).unwrap();
        assert_eq!(start, None);
        assert_eq!(l.core_update().unwrap().asked, None);
        assert_eq!(
            l.report_builds(ready()).unwrap().0,
            None,
            "B again: not chosen"
        );
    }

    #[test]
    fn roll_back_and_keep() {
        let mut l = ledger(updated());
        let v = l.core_update().unwrap();
        assert_eq!((v.started, v.kept), (Started::Update, false));
        assert_eq!(v.back.unwrap().commits[0].pr, Some(7));

        l.choose_build(B).unwrap();
        assert!(l.core_update().unwrap().kept, "keeping the running build");
        assert_eq!(next(&updated(), Some(B), 0), None);

        l.choose_build(A).unwrap();
        assert_eq!(l.report_builds(updated()).unwrap().0.as_deref(), Some(A));
    }

    #[test]
    fn after_a_roll_back_update_brings_it_back() {
        // Rolled back to A from B, which is still main's build.
        let r = Builds {
            running: A.into(),
            started: Started::RollBack,
            ready: Some(B.into()),
            previous: Some(B.into()),
            ..ready()
        };
        let v = view(&r, None);
        assert_eq!(v.ready.unwrap().sha, B);
        assert_eq!(v.back, None, "Roll back would be Update");
        // Main moved on to C meanwhile: B is no build to roll "back" to.
        let r = Builds {
            ready: Some("cccccccc".into()),
            ..r
        };
        assert_eq!(view(&r, None).back, None);
    }

    #[test]
    fn auto_starts_a_ready_build_once_no_task_runs() {
        let r = Builds {
            auto: true,
            ..ready()
        };
        assert_eq!(next(&r, None, 2), None);
        assert_eq!(next(&r, None, 0).as_deref(), Some(B));
        let r = Builds {
            auto: true,
            ..updated()
        };
        assert_eq!(next(&r, None, 0), None, "auto never rolls back");
        // Rolled back from B: auto leaves it to the owner.
        let r = Builds {
            started: Started::RollBack,
            previous: Some(B.into()),
            auto: true,
            ..ready()
        };
        assert_eq!(next(&r, None, 0), None);
        assert_eq!(next(&r, Some(B), 0).as_deref(), Some(B), "the owner may");
    }
}

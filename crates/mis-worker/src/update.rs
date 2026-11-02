//! When the worker switches to its orchestrator's build, or on edge, while that isn't published
//! yet, to the newest edge build if it is newer. Pure: main.rs fetches, installs and restarts as
//! told.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use mis_types::{Channel, WorkerUpdate};
use mis_update::{Asset, EdgeHead};

/// A version that failed is tried again after this.
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// A version not published yet (CI may still be building it) is looked for again after this.
pub const RETRY_UNPUBLISHED: Duration = Duration::from_secs(600);

/// What main.rs does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Download and check this build; its result goes to `fetched` (after installing).
    Fetch(Asset),
    /// Read edge.json; its result goes to `head`.
    ReadEdge,
}

/// Why getting a build failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// It isn't published (404).
    Unpublished,
    Failed(String),
}

impl Fail {
    pub fn of(e: &anyhow::Error) -> Self {
        if mis_update::unpublished(e) {
            Self::Unpublished
        } else {
            Self::Failed(format!("{e:#}"))
        }
    }
}

/// A step's result, back from main.rs's task.
pub enum Done {
    Fetched(String, Result<Vec<u8>, Fail>),
    Head(Result<EdgeHead, String>),
}

pub struct Updater {
    mine: String,
    /// The CI run that built this binary (`mis_update::build_run`).
    run: Option<u64>,
    /// Why self-update is off (`mis_update::blocked`); None: on.
    blocked: Option<&'static str>,
    base: String,
    target: String,
    /// The orchestrator's version and channel, from the latest Welcome.
    orch: Option<(String, Option<Channel>)>,
    pub state: WorkerUpdate,
    /// A step is in flight.
    fetching: bool,
    /// Versions that failed, and when each may be tried again.
    failed: HashMap<String, Instant>,
    /// The orchestrator's edge build isn't published: read edge.json next.
    read_edge: bool,
    /// The newest edge build, taken meanwhile because it is newer than this one.
    fallback: Option<String>,
    /// The new build is in place (announced Pending).
    installed: bool,
    /// This connection's orchestrator acknowledged Pending (`OrchMsg::UpdateHeld`): every task
    /// it will send here has arrived.
    held: bool,
}

impl Updater {
    pub fn new(
        mine: &str,
        run: Option<u64>,
        blocked: Option<&'static str>,
        base: &str,
        target: &str,
    ) -> Self {
        Self {
            mine: mine.into(),
            run,
            blocked,
            base: base.into(),
            target: target.into(),
            orch: None,
            state: WorkerUpdate::None,
            fetching: false,
            failed: HashMap::new(),
            read_edge: false,
            fallback: None,
            installed: false,
            held: false,
        }
    }

    pub fn welcome(&mut self, version: Option<String>, channel: Option<Channel>) {
        let orch = version.map(|v| (v, channel));
        if orch != self.orch {
            self.read_edge = false;
            self.fallback = None;
        }
        self.orch = orch;
    }

    /// What to do now, if anything: the orchestrator runs another release build on a channel,
    /// self-update is on, nothing is in flight or installed, and the build isn't backing off.
    /// The fallback build, when there is one, goes before the orchestrator's.
    pub fn due(&mut self, now: Instant) -> Option<Step> {
        let (theirs, channel) = self.orch.as_ref()?;
        if self.blocked.is_some() || self.fetching || self.installed || *theirs == self.mine {
            return None;
        }
        let channel = (*channel)?;
        let step = if self.read_edge {
            Step::ReadEdge
        } else {
            let fallback = self.fallback.as_ref();
            let v = fallback
                .filter(|v| !self.backing_off(v, now))
                .unwrap_or(theirs);
            if self.backing_off(v, now) {
                return None;
            }
            Step::Fetch(mis_update::asset(&self.base, channel, v, &self.target)?)
        };
        self.fetching = true;
        Some(step)
    }

    fn backing_off(&self, version: &str, now: Instant) -> bool {
        self.failed.get(version).is_some_and(|until| now < *until)
    }

    /// A download of `version` finished: install it only if it is still the build to take.
    pub fn wants(&self, version: &str) -> bool {
        self.blocked.is_none()
            && (self.orch.as_ref().is_some_and(|(v, _)| v == version)
                || self.fallback.as_deref() == Some(version))
    }

    /// A download was dropped unused.
    pub fn abandon(&mut self) {
        self.fetching = false;
    }

    /// A fetch and install of `version` ended; returns the state to report.
    pub fn fetched(
        &mut self,
        version: &str,
        result: Result<(), Fail>,
        now: Instant,
    ) -> WorkerUpdate {
        self.fetching = false;
        self.state = match result {
            Ok(()) => {
                self.installed = true;
                WorkerUpdate::Pending {
                    version: version.into(),
                }
            }
            Err(fail) => {
                let (wait, why) = match fail {
                    Fail::Unpublished => {
                        // The newest published edge build may do meanwhile.
                        let orch = self.orch.as_ref();
                        self.read_edge =
                            orch.is_some_and(|(v, c)| v == version && *c == Some(Channel::Edge));
                        (RETRY_UNPUBLISHED, "not published yet".into())
                    }
                    Fail::Failed(why) => (RETRY_AFTER, why),
                };
                self.failed.retain(|_, until| now < *until);
                self.failed.insert(version.into(), now + wait);
                WorkerUpdate::Failed {
                    version: version.into(),
                    why,
                }
            }
        };
        self.state.clone()
    }

    /// edge.json was read: take its build meanwhile if it is newer than this one. Newer means a
    /// later CI run, so a build edge.json hasn't caught up with is never swapped for an older
    /// one. Returns the decision to log; None if the orchestrator moved on meanwhile.
    pub fn head(&mut self, head: Result<EdgeHead, String>, now: Instant) -> Option<String> {
        self.fetching = false;
        if !std::mem::take(&mut self.read_edge) {
            return None;
        }
        let (theirs, _) = self.orch.as_ref()?;
        let sha = |v: &str| mis_update::Release::parse(v).map_or(v.to_string(), |r| r.sha);
        let missing = format!("orchestrator build {} not on edge yet", sha(theirs));
        let retry = format!("retry in {}m", RETRY_UNPUBLISHED.as_secs() / 60);
        let head = match head {
            Ok(head) => head,
            Err(e) => return Some(format!("{missing}; reading edge.json failed: {e}; {retry}")),
        };
        let edge = sha(&head.version);
        let not = match (self.run, head.run) {
            (Some(mine), Some(run)) => {
                if run <= mine || !mis_update::newer(Channel::Edge, &self.mine, &head.version) {
                    "is not newer than this build"
                } else if head.version == *theirs {
                    "is the orchestrator's, still uploading"
                } else if self.backing_off(&head.version, now) {
                    "failed recently"
                } else {
                    self.fallback = Some(head.version);
                    return Some(format!("{missing}; updating to newest edge {edge}"));
                }
            }
            _ => "has no CI run to order it against this build by",
        };
        Some(format!("{missing}; newest edge {edge} {not}; {retry}"))
    }

    /// A new connection: its orchestrator hasn't acknowledged anything yet.
    pub fn connected(&mut self) {
        self.held = false;
    }

    pub fn held(&mut self) {
        self.held = true;
    }

    /// Restart now: a new build is installed, the orchestrator sends no more tasks, and none runs.
    pub fn restart_due(&self, idle: bool) -> bool {
        self.installed && self.held && idle
    }

    /// Logged once at startup.
    pub fn describe(&self) -> String {
        match (self.blocked, self.run) {
            (Some(why), _) => format!("self-update off: {why}"),
            (None, Some(run)) => format!(
                "self-update on ({}, {}, CI run {run})",
                self.base, self.target
            ),
            (None, None) => format!("self-update on ({}, {}, no CI run)", self.base, self.target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0.1.0+aaaaaaa";
    const B: &str = "0.2.0+bbbbbbb";
    const T: &str = "x86_64-unknown-linux-gnu";

    fn on() -> Updater {
        Updater::new(A, Some(10), None, "http://r", T)
    }

    /// The build `step` fetches.
    fn fetch(step: Option<Step>) -> Asset {
        match step {
            Some(Step::Fetch(a)) => a,
            other => panic!("expected a fetch, got {other:?}"),
        }
    }

    fn edge(version: &str, run: Option<u64>) -> Result<EdgeHead, String> {
        Ok(EdgeHead {
            version: version.into(),
            run,
        })
    }

    /// `u`, welcomed by an edge orchestrator on `theirs`, found it unpublished.
    fn unpublished(u: &mut Updater, theirs: &str, now: Instant) {
        u.welcome(Some(theirs.into()), Some(Channel::Edge));
        assert_eq!(fetch(u.due(now)).version, theirs);
        assert_eq!(
            u.fetched(theirs, Err(Fail::Unpublished), now),
            WorkerUpdate::Failed {
                version: theirs.into(),
                why: "not published yet".into()
            }
        );
        assert_eq!(u.due(now), Some(Step::ReadEdge));
    }

    #[test]
    fn follows_a_different_orchestrator_version_on_its_channel() {
        let now = Instant::now();
        let mut u = on();
        assert_eq!(u.due(now), None, "no Welcome yet");
        u.welcome(Some(A.into()), Some(Channel::Edge));
        assert_eq!(u.due(now), None, "same version");
        u.welcome(Some(B.into()), Some(Channel::Edge));
        let a = fetch(u.due(now));
        assert_eq!(
            a.url,
            format!("http://r/download/edge/mis-{T}-bbbbbbb.tar.gz")
        );
        assert_eq!(a.version, B);
        assert_eq!(u.due(now), None, "one fetch at a time");
        // Older orchestrators too: workers follow exactly, including down.
        let mut u = Updater::new(B, Some(10), None, "http://r", T);
        u.welcome(Some(A.into()), Some(Channel::Stable));
        assert_eq!(
            fetch(u.due(now)).url,
            format!("http://r/download/v0.1.0/mis-{T}.tar.gz")
        );
    }

    #[test]
    fn stays_put_when_off_unpublished_or_dev() {
        let now = Instant::now();
        let mut off = Updater::new(A, Some(10), Some("turned off"), "http://r", T);
        off.welcome(Some(B.into()), Some(Channel::Edge));
        assert_eq!(off.due(now), None);
        let mut u = on();
        u.welcome(Some(B.into()), None);
        assert_eq!(u.due(now), None, "orchestrator publishes nowhere");
        u.welcome(Some("0.2.0+bbbbbbb-dirty".into()), Some(Channel::Edge));
        assert_eq!(u.due(now), None, "dev orchestrator");
        u.welcome(None, None);
        assert_eq!(u.due(now), None, "old orchestrator");
    }

    #[test]
    fn a_failure_backs_off_that_version_only() {
        let now = Instant::now();
        let mut u = on();
        u.welcome(Some(B.into()), Some(Channel::Edge));
        u.due(now).unwrap();
        let st = u.fetched(B, Err(Fail::Failed("signature: bad".into())), now);
        assert_eq!(
            st,
            WorkerUpdate::Failed {
                version: B.into(),
                why: "signature: bad".into()
            }
        );
        assert_eq!(u.due(now + RETRY_UNPUBLISHED), None, "not the short retry");
        assert!(u.due(now + RETRY_AFTER).is_some(), "retried after an hour");
        u.fetched(B, Err(Fail::Failed("again".into())), now);
        u.welcome(Some("0.3.0+ccccccc".into()), Some(Channel::Edge));
        assert!(
            u.due(now).is_some(),
            "a newer orchestrator version is tried at once"
        );
    }

    #[test]
    fn an_unpublished_build_takes_a_newer_edge_build_meanwhile() {
        let now = Instant::now();
        let (c, e) = ("0.1.0+ccccccc", "0.1.0+eeeeeee");
        let mut u = on();
        unpublished(&mut u, c, now);
        assert_eq!(u.due(now), None, "one read at a time");
        assert_eq!(
            u.head(edge(e, Some(11)), now).as_deref(),
            Some("orchestrator build ccccccc not on edge yet; updating to newest edge eeeeeee")
        );
        let a = fetch(u.due(now));
        assert_eq!(
            a.url,
            format!("http://r/download/edge/mis-{T}-eeeeeee.tar.gz")
        );
        assert!(u.wants(e) && u.wants(c));
        assert_eq!(
            u.fetched(e, Ok(()), now),
            WorkerUpdate::Pending { version: e.into() }
        );
    }

    #[test]
    fn an_edge_build_not_newer_waits_ten_minutes() {
        let now = Instant::now();
        let c = "0.1.0+ccccccc";
        let retry = |u: &mut Updater| {
            assert_eq!(u.due(now + RETRY_UNPUBLISHED / 2), None);
            assert_eq!(fetch(u.due(now + RETRY_UNPUBLISHED)).version, c);
        };
        for (head, mine, why) in [
            (
                edge(A, Some(10)),
                Some(10),
                "newest edge aaaaaaa is not newer than this build",
            ),
            (edge("0.1.0+eeeeeee", Some(9)), Some(10), "is not newer"),
            (edge("0.0.9+eeeeeee", Some(11)), Some(10), "is not newer"),
            (edge("0.1.0+eeeeeee", None), Some(10), "has no CI run"),
            (edge("0.1.0+eeeeeee", Some(11)), None, "has no CI run"),
            (edge(c, Some(11)), Some(10), "still uploading"),
            (
                Err("timed out".into()),
                Some(10),
                "reading edge.json failed: timed out",
            ),
        ] {
            let mut u = Updater::new(A, mine, None, "http://r", T);
            unpublished(&mut u, c, now);
            let line = u.head(head, now).unwrap();
            assert!(
                line.contains(why) && line.ends_with("; retry in 10m"),
                "{line}"
            );
            assert!(!u.wants("0.1.0+eeeeeee"), "never downgrades: {line}");
            retry(&mut u);
        }
    }

    #[test]
    fn the_orchestrators_build_is_taken_once_published() {
        let now = Instant::now();
        let (c, e) = ("0.1.0+ccccccc", "0.1.0+eeeeeee");
        // Restarted on the fallback build, edge's newest.
        let mut u = Updater::new(e, Some(11), None, "http://r", T);
        unpublished(&mut u, c, now);
        let line = u.head(edge(e, Some(11)), now).unwrap();
        assert!(line.contains("not newer"), "{line}");
        let later = now + RETRY_UNPUBLISHED;
        assert_eq!(fetch(u.due(later)).version, c);
        assert_eq!(
            u.fetched(c, Ok(()), later),
            WorkerUpdate::Pending { version: c.into() }
        );
    }

    #[test]
    fn a_failed_fallback_backs_off_and_the_orchestrators_build_is_retried() {
        let now = Instant::now();
        let (c, e) = ("0.1.0+ccccccc", "0.1.0+eeeeeee");
        let mut u = on();
        unpublished(&mut u, c, now);
        u.head(edge(e, Some(11)), now).unwrap();
        assert_eq!(fetch(u.due(now)).version, e);
        u.fetched(e, Err(Fail::Failed("signature: bad".into())), now);
        assert_eq!(u.due(now), None, "both back off");
        let later = now + RETRY_UNPUBLISHED;
        assert_eq!(fetch(u.due(later)).version, c, "the orchestrator's first");
        u.fetched(c, Err(Fail::Unpublished), later);
        assert_eq!(u.due(later), Some(Step::ReadEdge));
        let line = u.head(edge(e, Some(11)), later).unwrap();
        assert!(line.contains("failed recently"), "{line}");
        assert_eq!(u.due(later), None);
    }

    #[test]
    fn a_new_orchestrator_version_drops_the_fallback() {
        let now = Instant::now();
        let (c, d, e) = ("0.1.0+ccccccc", "0.1.0+ddddddd", "0.1.0+eeeeeee");
        let mut u = on();
        unpublished(&mut u, c, now);
        u.welcome(Some(d.into()), Some(Channel::Edge));
        assert_eq!(
            u.head(edge(e, Some(11)), now),
            None,
            "read for the old version"
        );
        assert_eq!(fetch(u.due(now)).version, d);
        u.fetched(d, Err(Fail::Unpublished), now);
        u.due(now).unwrap();
        u.head(edge(e, Some(11)), now).unwrap();
        fetch(u.due(now));
        u.welcome(Some(B.into()), Some(Channel::Edge));
        assert!(!u.wants(e), "fallback for the old version");
        u.abandon();
        assert_eq!(fetch(u.due(now)).version, B);
    }

    #[test]
    fn stable_never_falls_back_but_retries_unpublished_soon() {
        let now = Instant::now();
        let mut u = on();
        u.welcome(Some(B.into()), Some(Channel::Stable));
        fetch(u.due(now));
        u.fetched(B, Err(Fail::Unpublished), now);
        assert_eq!(u.due(now), None);
        assert_eq!(fetch(u.due(now + RETRY_UNPUBLISHED)).version, B);
    }

    #[test]
    fn restarts_once_installed_held_and_idle() {
        let now = Instant::now();
        let mut u = on();
        u.held();
        assert!(!u.restart_due(true), "nothing installed");
        u.welcome(Some(B.into()), Some(Channel::Edge));
        u.due(now).unwrap();
        assert_eq!(
            u.fetched(B, Ok(()), now),
            WorkerUpdate::Pending { version: B.into() }
        );
        assert_eq!(
            u.due(now + RETRY_AFTER),
            None,
            "installed: nothing more to fetch"
        );
        u.connected();
        assert!(
            !u.restart_due(true),
            "new connection: tasks may still be on the way"
        );
        u.held();
        assert!(!u.restart_due(false), "a task runs");
        assert!(u.restart_due(true));
    }

    #[test]
    fn a_download_the_orchestrator_moved_on_from_is_dropped() {
        let now = Instant::now();
        let mut u = on();
        u.welcome(Some(B.into()), Some(Channel::Edge));
        u.due(now).unwrap();
        assert!(u.wants(B));
        u.welcome(Some("0.3.0+ccccccc".into()), Some(Channel::Edge));
        assert!(!u.wants(B));
        u.abandon();
        assert_eq!(fetch(u.due(now)).version, "0.3.0+ccccccc");
    }
}

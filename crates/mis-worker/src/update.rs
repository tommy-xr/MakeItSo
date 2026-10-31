//! When the worker switches to its orchestrator's build. Pure: main.rs fetches, installs and
//! restarts as told.

use std::time::{Duration, Instant};

use mis_types::{Channel, WorkerUpdate};
use mis_update::Asset;

/// A version that failed is tried again after this.
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);

pub struct Updater {
    mine: String,
    /// Why self-update is off (`mis_update::blocked`); None: on.
    blocked: Option<&'static str>,
    base: String,
    target: String,
    /// The orchestrator's version and channel, from the latest Welcome.
    orch: Option<(String, Option<Channel>)>,
    pub state: WorkerUpdate,
    /// A fetch is in flight.
    fetching: bool,
    /// A version that failed, and when it may be tried again.
    failed: Option<(String, Instant)>,
    /// The new build is in place (announced Pending).
    installed: bool,
    /// This connection's orchestrator acknowledged Pending (`OrchMsg::UpdateHeld`): every task
    /// it will send here has arrived.
    held: bool,
}

impl Updater {
    pub fn new(mine: &str, blocked: Option<&'static str>, base: &str, target: &str) -> Self {
        Self {
            mine: mine.into(),
            blocked,
            base: base.into(),
            target: target.into(),
            orch: None,
            state: WorkerUpdate::None,
            fetching: false,
            failed: None,
            installed: false,
            held: false,
        }
    }

    pub fn welcome(&mut self, version: Option<String>, channel: Option<Channel>) {
        self.orch = version.map(|v| (v, channel));
    }

    /// What to fetch now, if anything: the orchestrator runs another release build on a channel,
    /// self-update is on, nothing is in flight or installed, and this version isn't backing off.
    pub fn due(&mut self, now: Instant) -> Option<Asset> {
        let (theirs, channel) = self.orch.as_ref()?;
        if self.blocked.is_some() || self.fetching || self.installed || *theirs == self.mine {
            return None;
        }
        if (self.failed.as_ref()).is_some_and(|(v, until)| v == theirs && now < *until) {
            return None;
        }
        let asset = mis_update::asset(&self.base, (*channel)?, theirs, &self.target)?;
        self.fetching = true;
        Some(asset)
    }

    /// A download of `version` finished: install it only if the orchestrator still runs it.
    pub fn wants(&self, version: &str) -> bool {
        self.blocked.is_none() && self.orch.as_ref().is_some_and(|(v, _)| v == version)
    }

    /// A download was dropped unused.
    pub fn abandon(&mut self) {
        self.fetching = false;
    }

    /// A fetch and install of `version` ended; returns the state to report.
    pub fn fetched(
        &mut self,
        version: &str,
        result: Result<(), String>,
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
            Err(why) => {
                self.failed = Some((version.into(), now + RETRY_AFTER));
                WorkerUpdate::Failed {
                    version: version.into(),
                    why,
                }
            }
        };
        self.state.clone()
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
        match self.blocked {
            Some(why) => format!("self-update off: {why}"),
            None => format!("self-update on ({}, {})", self.base, self.target),
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
        Updater::new(A, None, "http://r", T)
    }

    #[test]
    fn follows_a_different_orchestrator_version_on_its_channel() {
        let now = Instant::now();
        let mut u = on();
        assert_eq!(u.due(now), None, "no Welcome yet");
        u.welcome(Some(A.into()), Some(Channel::Edge));
        assert_eq!(u.due(now), None, "same version");
        u.welcome(Some(B.into()), Some(Channel::Edge));
        let a = u.due(now).unwrap();
        assert_eq!(
            a.url,
            format!("http://r/download/edge/mis-{T}-bbbbbbb.tar.gz")
        );
        assert_eq!(a.version, B);
        assert_eq!(u.due(now), None, "one fetch at a time");
        // Older orchestrators too: workers follow exactly, including down.
        let mut u = Updater::new(B, None, "http://r", T);
        u.welcome(Some(A.into()), Some(Channel::Stable));
        assert_eq!(
            u.due(now).unwrap().url,
            format!("http://r/download/v0.1.0/mis-{T}.tar.gz")
        );
    }

    #[test]
    fn stays_put_when_off_unpublished_or_dev() {
        let now = Instant::now();
        let mut off = Updater::new(A, Some("turned off"), "http://r", T);
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
        let st = u.fetched(B, Err("signature: bad".into()), now);
        assert_eq!(
            st,
            WorkerUpdate::Failed {
                version: B.into(),
                why: "signature: bad".into()
            }
        );
        assert_eq!(u.due(now + Duration::from_secs(60)), None);
        assert!(u.due(now + RETRY_AFTER).is_some(), "retried after an hour");
        u.fetched(B, Err("again".into()), now);
        u.welcome(Some("0.3.0+ccccccc".into()), Some(Channel::Edge));
        assert!(
            u.due(now).is_some(),
            "a newer orchestrator version is tried at once"
        );
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
        assert_eq!(u.due(now).unwrap().version, "0.3.0+ccccccc");
    }
}

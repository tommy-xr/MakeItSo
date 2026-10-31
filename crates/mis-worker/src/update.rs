//! When the worker switches to its orchestrator's build. Pure: main.rs fetches, installs and
//! restarts as told.

use std::time::{Duration, Instant};

use mis_types::{Channel, WorkerUpdate};
use mis_update::Asset;

/// A version that failed is tried again after this.
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// Restart no sooner than this after announcing Pending, so an assignment already sent lands
/// first (the orchestrator stops assigning once it hears Pending).
pub const SETTLE: Duration = Duration::from_secs(2);

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
    /// When the new build was installed (announced Pending); restart once idle after SETTLE.
    installed: Option<Instant>,
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
            installed: None,
        }
    }

    pub fn welcome(&mut self, version: Option<String>, channel: Option<Channel>) {
        self.orch = version.map(|v| (v, channel));
    }

    /// What to fetch now, if anything: the orchestrator runs another release build on a channel,
    /// self-update is on, nothing is in flight or installed, and this version isn't backing off.
    pub fn due(&mut self, now: Instant) -> Option<Asset> {
        let (theirs, channel) = self.orch.as_ref()?;
        if self.blocked.is_some()
            || self.fetching
            || self.installed.is_some()
            || *theirs == self.mine
        {
            return None;
        }
        if (self.failed.as_ref()).is_some_and(|(v, until)| v == theirs && now < *until) {
            return None;
        }
        let asset = mis_update::asset(&self.base, (*channel)?, theirs, &self.target)?;
        self.fetching = true;
        Some(asset)
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
                self.installed = Some(now);
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

    /// When to look again for a restart, if one is coming.
    pub fn restart_at(&self) -> Option<Instant> {
        self.installed.map(|t| t + SETTLE)
    }

    /// Restart now: a new build is installed, settled, and no task runs here.
    pub fn restart_due(&self, idle: bool, now: Instant) -> bool {
        idle && self.restart_at().is_some_and(|t| now >= t)
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
    fn restarts_once_installed_settled_and_idle() {
        let now = Instant::now();
        let mut u = on();
        assert!(!u.restart_due(true, now));
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
        assert!(!u.restart_due(true, now), "settling");
        assert!(!u.restart_due(false, now + SETTLE), "a task runs");
        assert!(u.restart_due(true, now + SETTLE));
    }
}

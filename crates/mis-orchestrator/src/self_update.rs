//! The orchestrator replacing itself with the newest release on its channel. `due` decides
//! (pure); `run` polls, downloads, self-tests the new build, waits until no task runs, and swaps
//! it in, after which the caller shuts down and restarts into it. No rollback: a restarted
//! process can't watch itself crash, so the new build must pass `--self-test` first.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use mis_types::{Channel, Update};
use mis_update::{Release, Source, Staged};

use crate::{App, VERSION};

/// A version that failed is tried again after this.
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);
const BIN: &str = "mis-orchestrator";

/// Fetch `latest`: newer than `mine` on `channel`, and not a version that failed within the hour.
pub fn due(
    channel: Channel,
    mine: &str,
    latest: &str,
    failed: Option<&(String, Instant)>,
    now: Instant,
) -> bool {
    mis_update::newer(channel, mine, latest)
        && !failed.is_some_and(|(v, until)| v == latest && now < *until)
}

/// The page's update chip for `version`: pending, or failed with why.
fn status(version: &str, why: Option<String>) -> Update {
    let sha = Release::parse(version).map_or_else(String::new, |r| r.sha);
    match why {
        None => Update::Pending {
            sha,
            subject: format!("release {version}"),
        },
        Some(why) => Update::Failed {
            sha,
            subject: format!("release {version}: {why}"),
        },
    }
}

pub struct Updater {
    pub channel: Channel,
    pub source: Source,
    /// This executable, resolved at startup.
    pub exe: PathBuf,
    /// Passed to the new build's `--self-test`.
    pub state_dir: Option<PathBuf>,
    pub every: Duration,
}

impl Updater {
    /// Poll until a new build is in place and no task runs; then the caller restarts into it.
    pub async fn run(self, app: App) {
        let mut failed: Option<(String, Instant)> = None;
        let mut tick = tokio::time::interval(self.every);
        loop {
            tick.tick().await;
            let latest =
                match mis_update::latest(&self.source, self.channel, mis_update::TARGET).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!("checking for a release: {e:#}");
                        continue;
                    }
                };
            if !due(
                self.channel,
                VERSION,
                &latest,
                failed.as_ref(),
                Instant::now(),
            ) {
                continue;
            }
            tracing::info!("updating to {latest}");
            app.report_update(status(&latest, None));
            let result = match self.prepare(&latest).await {
                Ok(staged) => {
                    // A task assigned after this check carries on: its worker reports it after the restart.
                    while app.running() > 0 {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    staged.install()
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => {
                    tracing::info!("installed {latest}; restarting");
                    return;
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::warn!("update to {latest} failed: {why}");
                    app.report_update(status(&latest, Some(why)));
                    failed = Some((latest, Instant::now() + RETRY_AFTER));
                }
            }
        }
    }

    /// Download and check `version`, stage it beside this executable, and self-test it.
    async fn prepare(&self, version: &str) -> anyhow::Result<Staged> {
        let asset = mis_update::asset(&self.source.base, self.channel, version, mis_update::TARGET)
            .context("not a release version")?;
        let bytes = mis_update::fetch(&self.source, &asset, BIN).await?;
        let (exe, dir, version) = (
            self.exe.clone(),
            self.state_dir.clone(),
            version.to_string(),
        );
        tokio::task::spawn_blocking(move || {
            let staged = mis_update::stage(&exe, &bytes, BIN, &version)?;
            let mut args = vec![OsStr::new("--self-test")];
            if let Some(dir) = &dir {
                args.extend([OsStr::new("--state-dir"), dir.as_os_str()]);
            }
            staged.run_ok(&args)?;
            Ok(staged)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_when_newer_and_not_backing_off() {
        let now = Instant::now();
        let (a, b) = ("0.1.0+aaaaaaa", "0.2.0+bbbbbbb");
        assert!(due(Channel::Stable, a, b, None, now));
        assert!(
            !due(Channel::Stable, b, a, None, now),
            "stable never goes back"
        );
        assert!(!due(Channel::Edge, a, a, None, now));
        let failed = (b.to_string(), now + RETRY_AFTER);
        assert!(!due(Channel::Edge, a, b, Some(&failed), now));
        assert!(due(Channel::Edge, a, b, Some(&failed), now + RETRY_AFTER));
        assert!(due(Channel::Edge, a, "0.3.0+ccccccc", Some(&failed), now));
    }

    #[test]
    fn the_chip_names_the_release() {
        let p = status("0.2.0+bbbbbbb", None);
        assert_eq!(
            p,
            Update::Pending {
                sha: "bbbbbbb".into(),
                subject: "release 0.2.0+bbbbbbb".into()
            }
        );
        let Update::Failed { subject, .. } = status("0.2.0+bbbbbbb", Some("self-test".into()))
        else {
            panic!()
        };
        assert_eq!(subject, "release 0.2.0+bbbbbbb: self-test");
    }
}

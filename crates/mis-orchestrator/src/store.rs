//! The ledger's files in the state dir: `ledger.json` and
//! `transcripts/<task>.json`, readable only by this user and each replaced
//! atomically. What goes in them is `ledger::persist`'s; this reads and writes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use mis_types::TaskId;

use crate::ledger::persist::Snapshot;
use crate::ledger::{Effect, Ledger};
use crate::push::write_private;

pub const LEDGER_FILE: &str = "ledger.json";
const LOCK_FILE: &str = "ledger.lock";
const TRANSCRIPTS: &str = "transcripts";
/// How long `open` waits for another orchestrator to let go of the dir.
const LOCK_TRIES: u32 = 50;
const LOCK_WAIT: Duration = Duration::from_millis(100);

/// Another orchestrator holds the state dir.
#[derive(Debug)]
pub struct Locked(pub PathBuf);

impl std::fmt::Display for Locked {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "another orchestrator is using {}", self.0.display())
    }
}

impl std::error::Error for Locked {}

pub struct Store {
    dir: PathBuf,
    /// Locked while open: one orchestrator per state dir.
    _lock: File,
    /// Finished tasks whose transcript is saved for good.
    written: HashSet<TaskId>,
    /// Hash of what each file last got, so unchanged ones aren't rewritten.
    /// None: the ledger file.
    last: HashMap<Option<TaskId>, u64>,
    /// The last save failed; logged once until one succeeds.
    failing: bool,
}

impl Store {
    /// Lock `dir` and restore the ledger saved there. None saved yet: a fresh
    /// start. One that can't be read is moved aside (`ledger.json.bad-<unix
    /// secs>`) for a fresh start. Err: the dir can't be used, or is `Locked`.
    pub fn open(
        dir: &Path,
        now: Duration,
        now_ms: u64,
    ) -> anyhow::Result<(Store, Ledger, Vec<Effect>)> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(dir.join(TRANSCRIPTS))
            .with_context(|| format!("creating {}", dir.display()))?;
        let lock_path = dir.join(LOCK_FILE);
        let lock = (std::fs::OpenOptions::new().create(true).truncate(false))
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        // A restart may find the old process still saving: give it a moment.
        let mut tries = 0;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if tries < LOCK_TRIES => {
                    tries += 1;
                    std::thread::sleep(LOCK_WAIT);
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(Locked(dir.to_path_buf()).into())
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        let path = dir.join(LEDGER_FILE);
        let transcript = |id| std::fs::read_to_string(transcript_path(dir, id)).ok();
        let restored = match std::fs::read_to_string(&path) {
            Ok(json) => Ledger::restore(&json, transcript, now, now_ms).map_err(|e| anyhow!(e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(e) => Err(e.into()),
        };
        let restored = match restored {
            Ok(r) => r,
            Err(e) => {
                let aside = dir.join(format!("{LEDGER_FILE}.bad-{}", now_ms / 1000));
                tracing::warn!(
                    "can't read {}: {e:#}; moving it to {} and starting empty",
                    path.display(),
                    aside.display()
                );
                std::fs::rename(&path, &aside)
                    .with_context(|| format!("moving {} aside", path.display()))?;
                Default::default()
            }
        };
        let store = Store {
            dir: dir.to_path_buf(),
            _lock: lock,
            written: restored.written,
            last: HashMap::new(),
            failing: false,
        };
        Ok((store, restored.ledger, restored.effects))
    }

    /// For `Ledger::snapshot`.
    pub fn written(&self) -> &HashSet<TaskId> {
        &self.written
    }

    /// Write the files in `snap` that changed. Transcripts go first, so a
    /// finished task in the ledger file always has its final transcript.
    pub fn save(&mut self, snap: Snapshot) {
        let result = (snap.transcripts.into_iter())
            .try_for_each(|t| {
                self.put(Some(t.task), &t.json)?;
                if t.done {
                    self.written.insert(t.task);
                    self.last.remove(&Some(t.task));
                }
                Ok(())
            })
            .and_then(|()| self.put(None, &snap.ledger));
        match (result, self.failing) {
            (Err(e), false) => tracing::warn!("saving task history: {e}"),
            (Ok(()), true) => tracing::info!("saving task history again"),
            _ => return,
        }
        self.failing = !self.failing;
    }

    /// Delete the transcripts of tasks the ledger forgot.
    pub fn forget(&mut self, tasks: &[TaskId]) {
        for id in tasks {
            let _ = std::fs::remove_file(transcript_path(&self.dir, *id));
            self.written.remove(id);
        }
    }

    /// Write `json` to `key`'s file unless that's what it already holds.
    fn put(&mut self, key: Option<TaskId>, json: &str) -> std::io::Result<()> {
        let h = hash(json);
        if self.last.get(&key) == Some(&h) {
            return Ok(());
        }
        let path = match key {
            Some(id) => transcript_path(&self.dir, id),
            None => self.dir.join(LEDGER_FILE),
        };
        write_private(&path, json.as_bytes())?;
        self.last.insert(key, h);
        Ok(())
    }
}

fn transcript_path(dir: &Path, task: TaskId) -> PathBuf {
    dir.join(TRANSCRIPTS).join(format!("{task}.json"))
}

fn hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mis_types::TaskStatus;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("mis-store-test-{}", uuid::Uuid::new_v4()))
    }

    const NOW: Duration = Duration::ZERO;

    #[test]
    fn saved_tasks_come_back_once_the_dir_is_free() {
        let dir = temp_dir();
        let (mut store, mut ledger, _) = Store::open(&dir, NOW, 0).unwrap();
        let (id, _) = ledger.submit("hi".into(), None, Some("claude".into()), None, vec![]);
        store.save(ledger.snapshot(store.written()));
        // The next orchestrator waits for this one to let go.
        let held = std::thread::spawn(move || {
            std::thread::sleep(LOCK_WAIT * 3);
            drop(store);
        });
        let (_, ledger, _) = Store::open(&dir, NOW, 0).unwrap();
        held.join().unwrap();
        let (task, _, truncated) = ledger.show(id).unwrap();
        assert_eq!((task.status, truncated), (TaskStatus::Queued, false));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(dir.join(LEDGER_FILE)), 0o600);
            assert_eq!(mode(transcript_path(&dir, id)), 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_ledger_is_moved_aside_for_a_fresh_start() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LEDGER_FILE), "{ truncated").unwrap();
        let (_, ledger, effects) = Store::open(&dir, NOW, 5_000).unwrap();
        let mis_types::ServerMsg::Status { tasks, .. } = ledger.status() else {
            panic!()
        };
        assert!(tasks.is_empty() && effects.is_empty());
        assert!(!dir.join(LEDGER_FILE).exists());
        let aside = dir.join(format!("{LEDGER_FILE}.bad-5"));
        assert_eq!(std::fs::read_to_string(aside).unwrap(), "{ truncated");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

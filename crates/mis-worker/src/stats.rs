//! Machine load sampling for `WorkerMsg::Stats`.

use std::path::{Path, PathBuf};

use mis_types::WorkerStats;
use sysinfo::{Disks, System};

/// Keeps sysinfo state between samples: CPU usage is the delta since the last one.
pub struct Sampler {
    sys: System,
    disks: Disks,
    /// Disk stats come from the filesystem holding this path.
    path: PathBuf,
}

impl Sampler {
    /// Blocks for sysinfo's minimum CPU interval so the first sample is meaningful.
    pub fn new(path: PathBuf) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        Self {
            sys,
            disks: Disks::new_with_refreshed_list(),
            path,
        }
    }

    pub fn sample(&mut self) -> WorkerStats {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.disks.refresh(true);
        let mounts: Vec<&Path> = self.disks.iter().map(|d| d.mount_point()).collect();
        let (free, total) = disk_for(&mounts, &self.path)
            .map(|i| &self.disks.list()[i])
            .map_or((0, 0), |d| (d.available_space(), d.total_space()));
        WorkerStats {
            disk_free_gb: free as f32 / 1e9,
            disk_used_pct: pct(total.saturating_sub(free), total),
            cpu_pct: self.sys.global_cpu_usage().round().clamp(0.0, 100.0) as u8,
            mem_pct: pct(self.sys.used_memory(), self.sys.total_memory()),
            limits: Default::default(),
        }
    }
}

/// `used` as a rounded percentage of `total`, 0 when `total` is 0.
fn pct(used: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    let p = (used as u128 * 100 + total as u128 / 2) / total as u128;
    p.min(100) as u8
}

/// Index of the deepest mount containing `path`. Windows `\\?\` prefixes are
/// ignored so canonical roots match drive mounts like `C:\`.
fn disk_for(mounts: &[&Path], path: &Path) -> Option<usize> {
    let plain = |p: &Path| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\"));
    let path = plain(path);
    mounts
        .iter()
        .enumerate()
        .filter(|(_, m)| path.starts_with(plain(m)))
        .max_by_key(|(_, m)| m.as_os_str().len())
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_rounds_and_clamps() {
        assert_eq!(pct(0, 0), 0);
        assert_eq!(pct(1, 3), 33);
        assert_eq!(pct(2, 3), 67);
        assert_eq!(pct(5, 4), 100);
        assert_eq!(pct(u64::MAX, u64::MAX), 100);
    }

    #[test]
    fn deepest_mount_wins() {
        let mounts = [Path::new("/"), Path::new("/data"), Path::new("/data2")];
        assert_eq!(disk_for(&mounts, Path::new("/data/code")), Some(1));
        assert_eq!(disk_for(&mounts, Path::new("/home/x")), Some(0));
        assert_eq!(disk_for(&mounts[1..], Path::new("/home")), None);
    }
}

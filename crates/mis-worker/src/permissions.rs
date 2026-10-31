//! worker.toml `[permissions]` -> what each harness runs with.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mis_types::{Inventory, PermissionMode, Permissions};

/// What the agents run with: `--skip-permissions` means full, else the file's
/// `[permissions]`; None when neither (the harnesses then deny tools headless).
pub fn effective(file: Option<Permissions>, skip_permissions: bool) -> Option<Permissions> {
    match skip_permissions {
        true => Some(Permissions {
            mode: PermissionMode::Full,
            ..Default::default()
        }),
        false => file,
    }
}

/// `~/x` against `home`; other paths as given.
pub fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Folders expanded and canonical (symlinks and `..` resolved, so the
/// harnesses are handed the real directory), then `mis_agent::check`ed.
/// Full mode keeps no lists: nothing is limited.
pub fn resolve(p: Permissions, home: Option<&Path>) -> Result<Permissions> {
    if p.mode == PermissionMode::Full {
        return Ok(Permissions {
            mode: PermissionMode::Full,
            ..Default::default()
        });
    }
    let mut folders = vec![];
    for f in &p.folders {
        let path = expand(f, home);
        let real = path
            .canonicalize()
            .with_context(|| format!("permissions folder {f}"))?;
        if !real.is_dir() {
            bail!("permissions folder {f} is not a directory");
        }
        let real = real.display().to_string();
        if !folders.contains(&real) {
            folders.push(real);
        }
    }
    let p = Permissions { folders, ..p };
    mis_agent::check(&p).map_err(|e| anyhow::anyhow!("worker.toml [permissions]: {e}"))?;
    Ok(p)
}

/// `p` for one harness. Codex fails to load its config when an override names
/// a server it doesn't have, so it gets only the `mcp` servers its own config defines.
pub fn for_harness(p: &Permissions, harness: &str, inv: &Inventory) -> Permissions {
    let mut p = p.clone();
    if harness == "codex" {
        let has = inv.mcp.get("codex").cloned().unwrap_or_default();
        p.mcp.retain(|s| has.contains(s));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders(f: &[&str]) -> Permissions {
        Permissions {
            folders: f.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn skip_permissions_means_full_and_unset_stays_unset() {
        assert_eq!(effective(None, false), None);
        let full = effective(Some(folders(&["/x"])), true).unwrap();
        assert_eq!(full.mode, PermissionMode::Full);
        assert_eq!(
            effective(Some(folders(&["/x"])), false),
            Some(folders(&["/x"]))
        );
    }

    #[test]
    fn folders_expand_home_and_resolve_symlinks_and_dots() {
        let dir = std::env::temp_dir().join(format!("mis-perm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("real/sub")).unwrap();
        let dir = dir.canonicalize().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let real = dir.join("real").display().to_string();
        let got = resolve(folders(&["~/real/sub/.."]), Some(&dir)).unwrap();
        assert_eq!(got.folders, std::slice::from_ref(&real));
        #[cfg(unix)]
        {
            let link = dir.join("link").display().to_string();
            let got = resolve(folders(&[&link, &real]), None).unwrap();
            assert_eq!(got.folders, [real], "symlink resolved, deduped");
        }
        assert!(resolve(folders(&["~/missing"]), Some(&dir)).is_err());
        assert!(resolve(folders(&["/"]), None).is_err());
        let bad_cmd = Permissions {
            commands: vec!["git *".into()],
            ..folders(&[])
        };
        assert!(resolve(bad_cmd, None).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn codex_gets_only_servers_its_config_defines() {
        let p = Permissions {
            mcp: vec!["blender".into(), "playwright".into()],
            ..Default::default()
        };
        let inv = Inventory {
            mcp: [
                ("claude".into(), vec!["blender".into(), "playwright".into()]),
                ("codex".into(), vec!["blender".into()]),
            ]
            .into(),
            ..Default::default()
        };
        assert_eq!(for_harness(&p, "codex", &inv).mcp, ["blender"]);
        assert_eq!(for_harness(&p, "claude", &inv).mcp, p.mcp);
        assert!(for_harness(&p, "codex", &Inventory::default())
            .mcp
            .is_empty());
    }
}

//! Roots: the directories this worker may run tasks in.

use std::path::{Path, PathBuf};

/// `MIS_ROOTS` / `--root` value -> paths. Splits on `,` and the platform path
/// separator (`:` on unix), e.g. "/a,/b:/c" -> [/a, /b, /c].
pub fn split_roots(s: &str) -> Vec<PathBuf> {
    s.split(',')
        .flat_map(std::env::split_paths)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Canonicalise each root so `resolve_cwd`'s prefix check sees real paths.
pub fn canonical_roots(roots: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    roots
        .iter()
        .map(|r| {
            r.canonicalize()
                .map_err(|e| anyhow::anyhow!("root {}: {e}", r.display()))
        })
        .collect()
}

/// Task cwd -> directory to run in. `roots` must be canonical and non-empty.
/// None -> first root; relative -> joined to the first root; either way the
/// canonical result must sit under some root (so `../..` escapes are refused).
pub fn resolve_cwd(roots: &[PathBuf], requested: Option<&str>) -> Result<PathBuf, String> {
    let Some(first) = roots.first() else {
        return Err("worker has no roots".into());
    };
    let Some(req) = requested else {
        return Ok(first.clone());
    };
    let path = Path::new(req);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        first.join(path)
    };
    let refuse = |why: &str| {
        let list: Vec<_> = roots.iter().map(|r| r.display().to_string()).collect();
        Err(format!(
            "cwd {req} {why}; worker roots: {}",
            list.join(", ")
        ))
    };
    match joined.canonicalize() {
        Ok(c) if roots.iter().any(|r| c.starts_with(r)) => Ok(c),
        Ok(_) => refuse("is outside the worker's roots"),
        Err(e) => refuse(&format!("is not usable ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh canonical dir with a `sub` child; removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let d = std::env::temp_dir().join(format!("mis-roots-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(d.join("sub")).unwrap();
            Tmp(d.canonicalize().unwrap())
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn splits_on_comma_and_path_separator() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(
            split_roots(&format!("a,b{sep}c,,")),
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")]
        );
    }

    #[test]
    fn none_is_first_root() {
        let t = Tmp::new();
        assert_eq!(resolve_cwd(std::slice::from_ref(&t.0), None).unwrap(), t.0);
    }

    #[test]
    fn relative_joins_first_root() {
        let t = Tmp::new();
        assert_eq!(
            resolve_cwd(std::slice::from_ref(&t.0), Some("sub")).unwrap(),
            t.0.join("sub")
        );
    }

    #[test]
    fn absolute_inside_any_root() {
        let (a, b) = (Tmp::new(), Tmp::new());
        let sub = b.0.join("sub");
        let got = resolve_cwd(&[a.0.clone(), b.0.clone()], Some(sub.to_str().unwrap()));
        assert_eq!(got.unwrap(), sub);
    }

    #[test]
    fn refuses_outside_and_escape() {
        let t = Tmp::new();
        let roots = [t.0.join("sub")];
        let e = resolve_cwd(&roots, Some(t.0.to_str().unwrap())).unwrap_err();
        assert!(e.contains("outside") && e.contains("sub"), "{e}");
        let e = resolve_cwd(&roots, Some("..")).unwrap_err();
        assert!(e.contains("outside"), "{e}");
        let e = resolve_cwd(&roots, Some("../sub/../..")).unwrap_err();
        assert!(e.contains("outside"), "{e}");
    }

    #[test]
    fn refuses_missing() {
        let t = Tmp::new();
        let e = resolve_cwd(std::slice::from_ref(&t.0), Some("nope")).unwrap_err();
        assert!(e.contains("not usable"), "{e}");
    }

    #[test]
    fn no_roots_is_error() {
        assert!(resolve_cwd(&[], None).is_err());
    }
}

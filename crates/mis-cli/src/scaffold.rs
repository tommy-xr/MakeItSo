//! `mis apps sdk` and `mis apps new`: an app's dev copy of the SDK, and a new app from a
//! template folder. Local files only; no orchestrator.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The SDK this CLI was built with, as the apps listener serves it under `/a/_/`.
pub const SDK: [(&str, &str); 3] = [
    (
        "mis.js",
        include_str!("../../mis-orchestrator/static/mis.js"),
    ),
    (
        "mis-mock.js",
        include_str!("../../mis-orchestrator/static/mis-mock.mjs"),
    ),
    (
        "mis.d.ts",
        include_str!("../../mis-orchestrator/static/mis.d.ts"),
    ),
];

/// Where `mis apps new` puts an app's SDK copy, for its dev server and type-checks.
pub const SDK_DIR: &str = "sdk";

/// Template folders that are build output, not template.
const SKIP: [&str; 3] = ["node_modules", "dist", ".git"];

/// Writes [`SDK`] into `dir` (made if missing), replacing older copies.
pub fn write_sdk(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, body) in SDK {
        let path = dir.join(name);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// `budget-tracker` -> `Budget tracker`.
pub fn title_of(slug: &str) -> String {
    let words = slug.replace('-', " ");
    let mut c = words.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Err: why `title` can't go into the template's JSON and HTML as is.
pub fn check_title(title: &str) -> Result<()> {
    let bad = |c: char| c.is_control() || "\"\\<>&".contains(c);
    if title.trim().is_empty() || title.chars().count() > 80 || title.contains(bad) {
        bail!("title {title:?}: 1-80 characters, none of \" \\ < > &");
    }
    Ok(())
}

/// `text` with `{{slug}}` and `{{title}}` filled in.
pub fn fill(text: &str, slug: &str, title: &str) -> String {
    text.replace("{{slug}}", slug).replace("{{title}}", title)
}

/// Copies `template` to `dest` (which must not exist), filling placeholders in UTF-8 files and
/// leaving others as they are, then writes the SDK into `dest/sdk`. Built beside `dest` and
/// renamed into place, so a failure leaves nothing behind.
pub fn new_app(template: &Path, dest: &Path, slug: &str, title: &str) -> Result<()> {
    if !mis_types::valid_slug(slug) {
        bail!(
            "{slug:?}: an app's name is a-z, 0-9 and -, up to 32, starting with a letter or digit"
        );
    }
    check_title(title)?;
    if !template.is_dir() {
        bail!("no template folder at {}", template.display());
    }
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    let name = dest.file_name().context("no app folder name")?;
    let parent = dest.parent().context("no apps folder")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    // Else the copy would walk into its own output.
    if parent.canonicalize()?.starts_with(template.canonicalize()?) {
        bail!(
            "{} is inside the template {}",
            dest.display(),
            template.display()
        );
    }
    let tmp = parent.join(format!(
        ".{}.new-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    // Not create_dir_all: a leftover from a crashed run must not be merged in.
    std::fs::create_dir(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let built = copy(template, &tmp, slug, title).and_then(|()| write_sdk(&tmp.join(SDK_DIR)));
    let done = built.and_then(|()| {
        // Checked again: another `new` may have made it meanwhile (rename would replace an empty one).
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        std::fs::rename(&tmp, dest).with_context(|| format!("creating {}", dest.display()))
    });
    if done.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    done
}

/// `from`'s contents into the folder `to`.
fn copy(from: &Path, to: &Path, slug: &str, title: &str) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
        .collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let src: PathBuf = e.path();
        let dst = to.join(e.file_name());
        let kind = e.file_type()?;
        if kind.is_symlink() {
            bail!("{} is a symlink: templates hold only files", src.display());
        } else if kind.is_dir() {
            if !SKIP.iter().any(|s| e.file_name() == *s) {
                std::fs::create_dir(&dst).with_context(|| format!("creating {}", dst.display()))?;
                copy(&src, &dst, slug, title)?;
            }
        } else if !kind.is_file() {
            bail!("{} is not a regular file", src.display());
        } else {
            let bytes =
                std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
            let out = match String::from_utf8(bytes) {
                Ok(text) => fill(&text, slug, title).into_bytes(),
                Err(binary) => binary.into_bytes(),
            };
            std::fs::write(&dst, out).with_context(|| format!("writing {}", dst.display()))?;
            // Keeps a script's execute bit.
            std::fs::set_permissions(&dst, e.metadata()?.permissions())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_placeholders() {
        assert_eq!(title_of("budget-tracker"), "Budget tracker");
        assert_eq!(title_of("pool"), "Pool");
        assert_eq!(title_of("2fa"), "2fa");
        assert_eq!(
            fill(
                r#"{"title": "{{title}}", "base": "/a/{{slug}}/{{slug}}"}"#,
                "kv",
                "K V"
            ),
            r#"{"title": "K V", "base": "/a/kv/kv"}"#
        );
        assert!(check_title("Pool & spa").is_err());
        assert!(check_title("Say \"hi\"").is_err());
        assert!(check_title(" ").is_err());
        assert!(check_title(&"x".repeat(81)).is_err());
        assert!(check_title("Chores, week 2").is_ok());
    }

    #[test]
    fn new_copies_a_template_fills_it_and_adds_the_sdk() {
        let root = std::env::temp_dir().join(format!("mis-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let t = root.join("apps/_template");
        for d in ["src", "node_modules/x", "dist"] {
            std::fs::create_dir_all(t.join(d)).unwrap();
        }
        std::fs::write(t.join("app.json"), r#"{"title": "{{title}}"}"#).unwrap();
        std::fs::write(t.join(".gitignore"), "node_modules\n").unwrap();
        std::fs::write(t.join("src/main.ts"), "const base = '/a/{{slug}}/';").unwrap();
        let png = [0x89, b'P', b'N', b'G', 0xff, b'{', b'{'];
        std::fs::write(t.join("icon.png"), png).unwrap();
        std::fs::write(t.join("node_modules/x/a.js"), "x").unwrap();
        std::fs::write(t.join("dist/index.html"), "x").unwrap();

        let dest = root.join("apps/demo");
        new_app(&t, &dest, "demo", "Demo").unwrap();
        let read = |p: &str| std::fs::read_to_string(dest.join(p)).unwrap();
        assert_eq!(read("app.json"), r#"{"title": "Demo"}"#);
        assert_eq!(read("src/main.ts"), "const base = '/a/demo/';");
        assert_eq!(read(".gitignore"), "node_modules\n");
        assert_eq!(std::fs::read(dest.join("icon.png")).unwrap(), png);
        assert!(!dest.join("node_modules").exists() && !dest.join("dist").exists());
        for (name, body) in SDK {
            assert_eq!(read(&format!("sdk/{name}")), body);
        }
        // Never over an existing app; nothing left behind by a refusal.
        std::fs::write(dest.join("app.json"), "mine").unwrap();
        assert!(new_app(&t, &dest, "demo", "Demo").is_err());
        assert_eq!(read("app.json"), "mine");
        assert!(new_app(&t, &root.join("apps/x"), "Bad_Slug", "X").is_err());
        assert!(new_app(&root.join("nope"), &root.join("apps/y"), "y", "Y").is_err());
        // A template holding the apps folder would copy into itself.
        assert!(new_app(&root, &root.join("apps/z"), "z", "Z").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = t.join("run.sh");
            std::fs::write(&script, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            new_app(&t, &root.join("apps/exec"), "exec", "Exec").unwrap();
            let mode = std::fs::metadata(root.join("apps/exec/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
            std::fs::remove_dir_all(root.join("apps/exec")).unwrap();
        }
        let left: Vec<_> = std::fs::read_dir(root.join("apps")).unwrap().collect();
        assert_eq!(left.len(), 2, "{left:?}"); // _template and demo
        std::fs::remove_dir_all(root).unwrap();
    }
}

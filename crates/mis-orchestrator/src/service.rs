//! `mis-orchestrator service`: run this orchestrator as a macOS LaunchAgent (the GUI session, so
//! Keychain sign-ins work), restarted whenever it exits. [`launchd_plist`] and friends are pure;
//! `install`/`uninstall`/`status` drive `launchctl` (macOS only).

use std::path::{Path, PathBuf};

pub const LABEL: &str = "io.github.tommy-xr.mis-orchestrator";

/// What the LaunchAgent runs, every path resolved at install.
#[derive(Debug, Clone)]
pub struct Spec {
    pub label: String,
    pub exe: String,
    pub state_dir: String,
    pub routines_dir: String,
    pub env_file: Option<String>,
    /// The unit's whole `PATH`: see [`unit_path`].
    pub path: String,
    /// stdout and stderr, appended (not rotated).
    pub log: String,
}

/// The LaunchAgent plist: runs at load, restarted on any exit, files private (umask 077).
pub fn launchd_plist(s: &Spec) -> String {
    let string = |v: &str| format!("\t\t<string>{}</string>\n", xml_escape(v));
    let mut argv = vec![s.exe.as_str(), "--state-dir", &s.state_dir];
    argv.extend(["--routines-dir", &s.routines_dir]);
    if let Some(e) = &s.env_file {
        argv.extend(["--env-file", e]);
    }
    let argv: String = argv.into_iter().map(string).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{argv}	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>{path}</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>Umask</key>
	<integer>63</integer>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(&s.label),
        path = xml_escape(&s.path),
        log = xml_escape(&s.log),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The unit's `PATH`: the dirs of `claude` and `git` as found at install, the exe's dir, then
/// the system dirs, each once and only if it `exists`. launchd's default is the system dirs only.
pub fn unit_path(
    claude: Option<&Path>,
    git: Option<&Path>,
    exe: &Path,
    exists: impl Fn(&Path) -> bool,
) -> String {
    let found = [claude, git, Some(exe)].into_iter().flatten();
    let system = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(Path::new);
    let mut dirs: Vec<&Path> = Vec::new();
    for d in found.filter_map(Path::parent).chain(system) {
        if !dirs.contains(&d) && exists(d) {
            dirs.push(d);
        }
    }
    let dirs: Vec<_> = dirs.iter().map(|d| d.to_string_lossy()).collect();
    dirs.join(":")
}

/// The top-level lines of `launchctl print` that `status` shows: `state = running` etc.
/// Nested blocks (indented further) repeat keys like `state`, so they're skipped.
pub fn print_summary(out: &str) -> Vec<(&str, &str)> {
    const KEYS: [&str; 4] = ["state", "pid", "runs", "last exit code"];
    (out.lines())
        .filter_map(|l| l.strip_prefix('\t'))
        .filter(|l| !l.starts_with('\t'))
        .filter_map(|l| l.split_once(" = "))
        .filter(|(k, _)| KEYS.contains(k))
        .collect()
}

/// `bin`'s path on `PATH`, as a shell would find it (not following symlinks: a versioned
/// install's link stays valid across its updates).
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// The paths in use now, which `install` pins.
pub struct Dirs {
    pub state_dir: PathBuf,
    pub routines_dir: PathBuf,
    pub env_file: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
pub use mac::{install, status, uninstall};

#[cfg(not(target_os = "macos"))]
pub use unsupported::{install, status, uninstall};

#[cfg(not(target_os = "macos"))]
mod unsupported {
    const WHY: &str = "service: not supported yet on this OS (macOS only)";
    pub fn install(_: &str, _: &std::path::Path, _: super::Dirs) -> anyhow::Result<()> {
        anyhow::bail!(WHY)
    }
    pub fn uninstall(_: &str) -> anyhow::Result<()> {
        anyhow::bail!(WHY)
    }
    pub fn status(_: &str) -> anyhow::Result<()> {
        anyhow::bail!(WHY)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{bail, Context};

    use super::{launchd_plist, print_summary, unit_path, which, Dirs, Spec};

    /// launchd sets these for an agent; pre-flight passes the same, plus the unit's PATH.
    const LAUNCHD_ENV: [&str; 5] = ["HOME", "USER", "LOGNAME", "SHELL", "TMPDIR"];

    fn home() -> anyhow::Result<PathBuf> {
        (std::env::var_os("HOME").filter(|h| !h.is_empty()))
            .map(PathBuf::from)
            .context("no HOME")
    }

    fn plist_path(label: &str) -> anyhow::Result<PathBuf> {
        Ok(home()?.join(format!("Library/LaunchAgents/{label}.plist")))
    }

    fn log_path() -> anyhow::Result<PathBuf> {
        Ok(home()?.join("Library/Logs/mis/orchestrator.log"))
    }

    /// This user's GUI session, where the agent runs.
    fn domain() -> String {
        // SAFETY: getuid has no preconditions.
        format!("gui/{}", unsafe { libc::getuid() })
    }

    fn target(label: &str) -> String {
        format!("{}/{label}", domain())
    }

    fn utf8(p: &Path) -> anyhow::Result<String> {
        (p.to_str().map(String::from)).with_context(|| format!("{} is not UTF-8", p.display()))
    }

    fn launchctl(args: &[&str]) -> anyhow::Result<std::process::Output> {
        Command::new("launchctl")
            .args(args)
            .output()
            .context("running launchctl")
    }

    /// Check the unit would start (doctor with its env, state dir free), then (re)load it.
    pub fn install(label: &str, exe: &Path, dirs: Dirs) -> anyhow::Result<()> {
        let abs = |p: &Path| std::path::absolute(p).with_context(|| p.display().to_string());
        let state_dir = abs(&dirs.state_dir)?;
        let routines_dir = abs(&dirs.routines_dir)?;
        let env_file = (dirs.env_file.as_deref())
            .map(|e| e.canonicalize().with_context(|| e.display().to_string()))
            .transpose()?;
        let (claude, git) = (which("claude"), which("git"));
        let path = unit_path(claude.as_deref(), git.as_deref(), exe, Path::is_dir);
        // Held: KeepAlive would restart a service that can't take the state dir, forever.
        if state_dir.is_dir() {
            if let Err(e) = crate::store::lock(&state_dir) {
                bail!("{e:#}: stop it first (`mis-orchestrator service uninstall` if it's this service)");
            }
        }
        // Doctor reads its config from env only; flags would win over the env file the same way.
        let mut doctor = Command::new(exe);
        doctor.arg("doctor").env_clear().env("PATH", &path);
        for k in LAUNCHD_ENV {
            if let Some(v) = std::env::var_os(k) {
                doctor.env(k, v);
            }
        }
        doctor.env("MIS_STATE_DIR", &state_dir);
        doctor.env("MIS_ROUTINES_DIR", &routines_dir);
        if let Some(e) = &env_file {
            doctor.env("MIS_ENV_FILE", e);
        }
        println!("doctor, as the service will run:");
        if !doctor.status().context("running doctor")?.success() {
            bail!("doctor found a failure: fix it, then install again");
        }
        let log = log_path()?;
        let spec = Spec {
            label: label.into(),
            exe: utf8(exe)?,
            state_dir: utf8(&state_dir)?,
            routines_dir: utf8(&routines_dir)?,
            env_file: env_file.as_deref().map(utf8).transpose()?,
            path,
            log: utf8(&log)?,
        };
        private_dir(log.parent().unwrap())?;
        let plist = plist_path(label)?;
        std::fs::create_dir_all(plist.parent().unwrap())?;
        std::fs::write(&plist, launchd_plist(&spec))
            .with_context(|| format!("writing {}", plist.display()))?;
        // Reinstall: unload the old one first (not running: the lock was free).
        let _ = launchctl(&["bootout", &target(label)]);
        let out = launchctl(&["bootstrap", &domain(), &utf8(&plist)?])?;
        if !out.status.success() {
            bail!(
                "launchctl bootstrap: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!("\ninstalled {label}: starts now and at every login, restarted if it exits");
        println!("  plist: {}", plist.display());
        println!("  log:   {}", log.display());
        println!("check it with `mis-orchestrator service status`");
        Ok(())
    }

    fn private_dir(dir: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        (std::fs::DirBuilder::new().recursive(true).mode(0o700))
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))
    }

    /// Stop and unload the service and delete its plist; the log stays.
    pub fn uninstall(label: &str) -> anyhow::Result<()> {
        let out = launchctl(&["bootout", &target(label)])?;
        match out.status.success() {
            true => println!("stopped {label}"),
            false => println!("{label} wasn't loaded"),
        }
        let plist = plist_path(label)?;
        match std::fs::remove_file(&plist) {
            Ok(()) => println!("removed {}", plist.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", plist.display())),
        }
        println!("log kept: {}", log_path()?.display());
        Ok(())
    }

    /// Loaded or not, running or not, the last exit, and the log's tail.
    pub fn status(label: &str) -> anyhow::Result<()> {
        let plist = plist_path(label)?;
        if !plist.exists() {
            println!("not installed ({} missing)", plist.display());
        }
        let out = launchctl(&["print", &target(label)])?;
        match out.status.success() {
            false => println!("{label}: not loaded"),
            true => {
                println!("{label}: loaded");
                for (k, v) in print_summary(&String::from_utf8_lossy(&out.stdout)) {
                    println!("  {k}: {v}");
                }
            }
        }
        let log = log_path()?;
        match tail(&log, 20) {
            Ok(t) => print!("\n{} (last lines):\n{t}", log.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => println!("reading {}: {e}", log.display()),
        }
        Ok(())
    }

    /// The last `n` lines of `path`, read from its last 64 KiB (the log isn't rotated).
    fn tail(path: &Path, n: usize) -> std::io::Result<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(64 << 10)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<_> = text.lines().collect();
        let start = lines.len().saturating_sub(n);
        Ok(lines[start..].iter().map(|l| format!("{l}\n")).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(env_file: Option<&str>) -> Spec {
        Spec {
            label: LABEL.into(),
            exe: "/Users/u/.local/bin/mis-orchestrator".into(),
            state_dir: "/Users/u/.local/state/mis".into(),
            routines_dir: "/Users/u/.config/mis/routines".into(),
            env_file: env_file.map(Into::into),
            path: "/Users/u/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
            log: "/Users/u/Library/Logs/mis/orchestrator.log".into(),
        }
    }

    #[test]
    fn plist_snapshot() {
        let want = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>io.github.tommy-xr.mis-orchestrator</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Users/u/.local/bin/mis-orchestrator</string>
		<string>--state-dir</string>
		<string>/Users/u/.local/state/mis</string>
		<string>--routines-dir</string>
		<string>/Users/u/.config/mis/routines</string>
		<string>--env-file</string>
		<string>/Users/u/.config/mis/orchestrator.env</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/Users/u/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>Umask</key>
	<integer>63</integer>
	<key>StandardOutPath</key>
	<string>/Users/u/Library/Logs/mis/orchestrator.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/u/Library/Logs/mis/orchestrator.log</string>
</dict>
</plist>
"#;
        assert_eq!(
            launchd_plist(&spec(Some("/Users/u/.config/mis/orchestrator.env"))),
            want
        );
    }

    #[test]
    fn plist_without_env_file_and_escaped() {
        let p = launchd_plist(&Spec {
            state_dir: "/a&b/<s>".into(),
            ..spec(None)
        });
        assert!(!p.contains("--env-file"));
        assert!(p.contains("<string>/a&amp;b/&lt;s&gt;</string>"));
        assert!(p.contains("<string>/Users/u/.config/mis/routines</string>\n\t</array>"));
    }

    #[test]
    fn path_keeps_found_dirs_that_exist_once() {
        let exists = |d: &Path| d != Path::new("/sbin");
        let p = unit_path(
            Some(Path::new("/Users/u/.local/bin/claude")),
            Some(Path::new("/usr/bin/git")),
            Path::new("/Users/u/.local/bin/mis-orchestrator"),
            exists,
        );
        assert_eq!(p, "/Users/u/.local/bin:/usr/bin:/bin:/usr/sbin");
        let p = unit_path(None, None, Path::new("/opt/mis/mis-orchestrator"), |_| true);
        assert_eq!(p, "/opt/mis:/usr/bin:/bin:/usr/sbin:/sbin");
    }

    #[test]
    fn summary_skips_nested_blocks() {
        let out = "gui/501/x = {\n\tactive count = 1\n\tstate = running\n\n\tenvironment = {\n\t\tstate => x\n\t}\n\
            \tendpoints = {\n\t\tstate = active\n\t}\n\truns = 2\n\tpid = 496\n\tlast exit code = 9: Killed: 9\n}\n";
        assert_eq!(
            print_summary(out),
            [
                ("state", "running"),
                ("runs", "2"),
                ("pid", "496"),
                ("last exit code", "9: Killed: 9")
            ]
        );
    }
}

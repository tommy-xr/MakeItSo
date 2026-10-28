//! Bakes the git revision into the binary so `--version`, the startup banner
//! and the page can say which commit is running.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let rev = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let rev = if dirty { format!("{rev}-dirty") } else { rev };
    println!("cargo:rustc-env=MIS_GIT_REV={rev}");
    for p in [
        "../../.git/HEAD",
        "../../.git/refs/heads",
        "../../.git/index",
    ] {
        println!("cargo:rerun-if-changed={p}");
    }
}

//! Finds, checks and installs signed release builds of the Make It So binaries.
//! Core (pure): version parsing, which asset holds a version, signature check, extraction.
//! Shell: `fetch` (HTTP), `stage` + `Staged::install` (swap the running executable), `restart`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
pub use mis_types::Channel;

/// Public key every release asset is signed with.
pub const RELEASE_KEY: &str = include_str!("../../../release/minisign.pub");
/// Where releases are published; `MIS_RELEASE_URL` replaces it.
pub const RELEASES: &str = "https://github.com/tommy-xr/MakeItSo/releases";
/// This build's target triple, which names its release asset.
pub const TARGET: &str = env!("MIS_TARGET");
/// CI sets `MIS_RELEASE=1` when building binaries for a release.
pub const CI_BUILD: bool = option_env!("MIS_RELEASE").is_some();
/// Bigger downloads are refused; a release tarball is tens of MB.
const MAX_BYTES: usize = 256 << 20;

/// A release build's version, `0.1.0+1a2b3c4`: semver plus the first 7 hex chars of its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub semver: String,
    pub sha: String,
}

impl Release {
    /// None for builds with uncommitted changes (`-dirty`), without git (`unknown`), or other shapes.
    pub fn parse(version: &str) -> Option<Self> {
        let (semver, sha) = version.split_once('+')?;
        let parts: Vec<&str> = semver.split('.').collect();
        let num = |p: &&str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        (parts.len() == 3 && parts.iter().all(num) && sha.len() == 7 && sha.bytes().all(hex)).then(
            || Self {
                semver: semver.into(),
                sha: sha.into(),
            },
        )
    }
}

/// Why this binary must not replace itself, if so. The rule: it updates only when not turned
/// off (`off`: `--no-self-update` / `MIS_NO_SELF_UPDATE`, which `mis-up` passes), its version
/// names a clean commit, and it is a CI release build or a release server was named explicitly
/// (`custom_server`: `MIS_RELEASE_URL`, for mirrors and tests).
pub fn blocked(
    off: bool,
    version: &str,
    ci_build: bool,
    custom_server: bool,
) -> Option<&'static str> {
    if off {
        Some("turned off")
    } else if Release::parse(version).is_none() {
        Some("dev build (no clean commit in its version)")
    } else if !ci_build && !custom_server {
        Some("dev build (not built by CI)")
    } else {
        None
    }
}

/// One release tarball and the version it must hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub version: String,
    /// File name, e.g. `mis-x86_64-unknown-linux-gnu-1a2b3c4.tar.gz`.
    pub name: String,
    pub url: String,
}

impl Asset {
    pub fn sig_url(&self) -> String {
        format!("{}.minisig", self.url)
    }
}

/// Where `version`'s tarball for `target` lives under `base` (a `.../releases` URL). Stable:
/// tag `v<semver>`, `mis-<target>.tar.gz`. Edge: tag `edge`, `mis-<target>-<sha7>.tar.gz`.
pub fn asset(base: &str, channel: Channel, version: &str, target: &str) -> Option<Asset> {
    let r = Release::parse(version)?;
    let (tag, name) = match channel {
        Channel::Stable => (format!("v{}", r.semver), format!("mis-{target}.tar.gz")),
        Channel::Edge => ("edge".into(), format!("mis-{target}-{}.tar.gz", r.sha)),
    };
    let base = base.trim_end_matches('/');
    Some(Asset {
        version: version.into(),
        url: format!("{base}/download/{tag}/{name}"),
        name,
    })
}

/// `data` is `asset`, signed by `key` (a minisign public key file's text). The signed trusted
/// comment must be `mis <version> <name>`, so a validly signed tarball of another version or
/// target is refused too.
pub fn verify(key: &str, data: &[u8], sig: &str, asset: &Asset) -> Result<(), String> {
    let key = minisign_verify::PublicKey::decode(key).map_err(|e| format!("release key: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig).map_err(|e| format!("signature: {e}"))?;
    key.verify(data, &sig, false)
        .map_err(|e| format!("signature: {e}"))?;
    let want = format!("mis {} {}", asset.version, asset.name);
    if sig.trusted_comment() != want {
        return Err(format!(
            "signature is for '{}', not '{want}'",
            sig.trusted_comment()
        ));
    }
    Ok(())
}

/// The regular file named exactly `file` at the top of a .tar.gz. Read into memory: no path in
/// the archive is ever written to.
pub fn extract(tar_gz: &[u8], file: &str) -> Result<Vec<u8>, String> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz));
    for entry in ar.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        if path != Path::new(file) || !entry.header().entry_type().is_file() {
            continue;
        }
        let mut out = Vec::new();
        entry
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())?;
        if out.len() > MAX_BYTES {
            return Err(format!("{file} is over {MAX_BYTES} bytes"));
        }
        return Ok(out);
    }
    Err(format!("no {file} in the archive"))
}

/// Executable file name of `bin` on this OS (`mis-worker.exe` on Windows).
pub fn exe_name(bin: &str) -> String {
    format!("{bin}{}", std::env::consts::EXE_SUFFIX)
}

/// Where releases come from and the key they must be signed with.
#[derive(Debug, Clone)]
pub struct Source {
    pub base: String,
    pub key: String,
    /// `base` came from `MIS_RELEASE_URL`.
    pub custom: bool,
}

impl Source {
    /// GitHub and the embedded key. `MIS_RELEASE_URL` names another server; debug builds also
    /// take `MIS_RELEASE_KEY` (a public key file) so tests can sign with a throwaway key.
    pub fn from_env() -> anyhow::Result<Self> {
        let url = std::env::var("MIS_RELEASE_URL")
            .ok()
            .filter(|u| !u.is_empty());
        let mut key = RELEASE_KEY.to_string();
        if cfg!(debug_assertions) {
            if let Some(path) = std::env::var_os("MIS_RELEASE_KEY") {
                key = std::fs::read_to_string(&path)
                    .with_context(|| format!("MIS_RELEASE_KEY {}", path.to_string_lossy()))?;
            }
        }
        Ok(Self {
            custom: url.is_some(),
            base: url.unwrap_or_else(|| RELEASES.into()),
            key,
        })
    }
}

/// Download `asset` and its signature, check them, and return the executable `bin` inside.
pub async fn fetch(src: &Source, asset: &Asset, bin: &str) -> anyhow::Result<Vec<u8>> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let sig = get(&http, &asset.sig_url()).await?;
    let sig = String::from_utf8(sig).context("signature is not text")?;
    let data = get(&http, &asset.url).await?;
    verify(&src.key, &data, &sig, asset).map_err(anyhow::Error::msg)?;
    extract(&data, &exe_name(bin)).map_err(anyhow::Error::msg)
}

async fn get(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut resp = http.get(url).send().await?.error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            bail!("{url}: over {MAX_BYTES} bytes");
        }
    }
    Ok(body)
}

/// A new executable written beside the one it replaces, checked to run and report its version.
/// Dropped without `install`, it is deleted.
pub struct Staged {
    pub path: PathBuf,
    target: PathBuf,
}

/// Write `exe` (the bytes of `bin`) next to `target`, the running executable, and check that it
/// says `<bin> <version>` to `--version`.
pub fn stage(target: &Path, exe: &[u8], bin: &str, version: &str) -> anyhow::Result<Staged> {
    let dir = target.parent().context("executable has no folder")?;
    let staged = Staged {
        path: dir.join(format!(
            ".{bin}.update-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX
        )),
        target: target.into(),
    };
    let mut f = std::fs::OpenOptions::new();
    f.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut f, 0o755);
    let _ = std::fs::remove_file(&staged.path); // left by a crash of this pid's predecessor
    std::io::Write::write_all(
        &mut f
            .open(&staged.path)
            .with_context(|| format!("writing {}", staged.path.display()))?,
        exe,
    )?;
    let out = std::process::Command::new(&staged.path)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", staged.path.display()))?;
    let said = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || said.trim() != format!("{bin} {version}") {
        bail!("new {bin} says '{}', want '{bin} {version}'", said.trim());
    }
    Ok(staged)
}

impl Staged {
    /// Rename it over the running executable, which keeps running from the old file. Windows
    /// can't replace a running exe but can rename it: it moves to `<exe>.old` (deleted by
    /// `cleanup` at the next start) and comes back if the swap fails.
    pub fn install(self) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            let old = old_path(&self.target);
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&self.target, &old).context("moving the running exe aside")?;
            if let Err(e) = std::fs::rename(&self.path, &self.target) {
                let _ = std::fs::rename(&old, &self.target);
                return Err(anyhow::Error::from(e).context("swapping in the new exe"));
            }
        }
        #[cfg(not(windows))]
        std::fs::rename(&self.path, &self.target).context("swapping in the new executable")?;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}

/// Delete what a Windows swap left behind (`<exe>.old`); a no-op elsewhere.
pub fn cleanup(_exe: &Path) {
    #[cfg(windows)]
    let _ = std::fs::remove_file(old_path(_exe));
}

/// Run `exe` (resolved at startup: on Linux the path of a replaced executable reads
/// "(deleted)") with this process's arguments in its place: exec on Unix; on Windows spawn it
/// and exit. Returns only on failure.
pub fn restart(exe: &Path) -> anyhow::Error {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        anyhow::Error::from(cmd.exec()).context(format!("exec {}", exe.display()))
    }
    #[cfg(not(unix))]
    match cmd.spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => anyhow::Error::from(e).context(format!("starting {}", exe.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PUB: &str = include_str!("../tests/fixtures/test.pub");
    const TEST_KEY: &str = include_str!("../tests/fixtures/test.key");
    const OTHER_KEY: &str = include_str!("../tests/fixtures/other.key");

    fn sign(key: &str, data: &[u8], comment: &str) -> String {
        let sk = minisign::SecretKeyBox::from_string(key)
            .unwrap()
            .into_unencrypted_secret_key()
            .unwrap();
        minisign::sign(None, &sk, data, Some(comment), None)
            .unwrap()
            .into_string()
    }

    fn edge(v: &str) -> Asset {
        asset(RELEASES, Channel::Edge, v, "x86_64-unknown-linux-gnu").unwrap()
    }

    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn release_versions_name_a_clean_commit() {
        let r = Release::parse("0.1.0+1a2b3c4").unwrap();
        assert_eq!((r.semver.as_str(), r.sha.as_str()), ("0.1.0", "1a2b3c4"));
        for v in [
            "0.1.0+1a2b3c4-dirty",
            "0.1.0+unknown",
            "0.1.0",
            "0.1+1a2b3c4",
            "0.1.x+1a2b3c4",
            "0.1.0+1A2B3C4",
            "0.1.0+1a2b3c45",
            "",
        ] {
            assert_eq!(Release::parse(v), None, "{v}");
        }
    }

    #[test]
    fn self_update_needs_on_a_clean_ci_build_or_named_server() {
        let v = "0.1.0+1a2b3c4";
        assert_eq!(blocked(false, v, true, false), None);
        assert_eq!(blocked(false, v, false, true), None);
        assert!(blocked(true, v, true, true).is_some());
        assert!(blocked(false, v, false, false).is_some());
        assert!(blocked(false, "0.1.0+1a2b3c4-dirty", true, true).is_some());
    }

    #[test]
    fn assets_by_channel() {
        let t = "aarch64-apple-darwin";
        let s = asset("http://h/r/", Channel::Stable, "0.2.0+1a2b3c4", t).unwrap();
        assert_eq!(
            s.url,
            "http://h/r/download/v0.2.0/mis-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(s.sig_url(), format!("{}.minisig", s.url));
        let e = asset("http://h/r", Channel::Edge, "0.2.0+1a2b3c4", t).unwrap();
        assert_eq!(
            e.url,
            "http://h/r/download/edge/mis-aarch64-apple-darwin-1a2b3c4.tar.gz"
        );
        assert_eq!(e.name, "mis-aarch64-apple-darwin-1a2b3c4.tar.gz");
        assert_eq!(asset("http://h/r", Channel::Edge, "0.2.0+x-dirty", t), None);
    }

    #[test]
    fn good_signature_passes() {
        let a = edge("0.2.0+1a2b3c4");
        let sig = sign(
            TEST_KEY,
            b"tarball",
            &format!("mis 0.2.0+1a2b3c4 {}", a.name),
        );
        assert_eq!(verify(TEST_PUB, b"tarball", &sig, &a), Ok(()));
    }

    #[test]
    fn tampered_wrong_key_and_wrong_comment_fail() {
        let a = edge("0.2.0+1a2b3c4");
        let comment = format!("mis 0.2.0+1a2b3c4 {}", a.name);
        let sig = sign(TEST_KEY, b"tarball", &comment);
        assert!(verify(TEST_PUB, b"tarbalL", &sig, &a).is_err(), "tampered");
        let other = sign(OTHER_KEY, b"tarball", &comment);
        assert!(
            verify(TEST_PUB, b"tarball", &other, &a).is_err(),
            "wrong key"
        );
        // Validly signed, but for another version (a downgrade) or another target.
        let old = sign(
            TEST_KEY,
            b"tarball",
            &format!("mis 0.1.0+0000000 {}", a.name),
        );
        let err = verify(TEST_PUB, b"tarball", &old, &a).unwrap_err();
        assert!(err.contains("not 'mis 0.2.0+1a2b3c4"), "{err}");
        let mac = sign(
            TEST_KEY,
            b"tarball",
            "mis 0.2.0+1a2b3c4 mis-aarch64-apple-darwin-1a2b3c4.tar.gz",
        );
        assert!(
            verify(TEST_PUB, b"tarball", &mac, &a).is_err(),
            "other target"
        );
        assert!(verify(TEST_PUB, b"tarball", "garbage", &a).is_err());
    }

    #[test]
    fn the_real_release_key_parses() {
        assert!(minisign_verify::PublicKey::decode(RELEASE_KEY).is_ok());
    }

    #[test]
    fn extract_takes_only_the_named_top_level_file() {
        let gz = tar_gz(&[
            ("sub/mis-worker", b"nested"),
            ("mis", b"cli"),
            ("mis-worker", b"worker"),
        ]);
        assert_eq!(extract(&gz, "mis-worker").unwrap(), b"worker");
        assert!(extract(&gz, "mis-orchestrator").is_err());
        assert!(extract(b"not a tarball", "mis-worker").is_err());
    }

    #[test]
    fn extract_ignores_traversal_and_links() {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "mis-worker", "/etc/passwd").unwrap();
        // `append_data` refuses `..`, so write the name into the header directly.
        let mut h = tar::Header::new_gnu();
        h.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"../mis-worker");
        h.set_size(4);
        h.set_cksum();
        b.append(&h, &b"evil"[..]).unwrap();
        let gz = b.into_inner().unwrap().finish().unwrap();
        assert!(extract(&gz, "mis-worker").is_err());
    }
}

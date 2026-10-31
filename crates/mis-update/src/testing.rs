//! Fakes for integration tests: a binary that reports another version, signed release tarballs
//! (with the throwaway keys in tests/fixtures), and a local stand-in for GitHub Releases.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Throwaway keypairs: `test.pub`/`test.key` and `other.key`.
pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// The version `exe` reports to `--version` (as `<bin> <version>`), if it is a release build.
/// A dirty tree's build isn't, and never updates itself.
pub fn release_version(exe: &Path, bin: &str) -> Option<String> {
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()?;
    let v = String::from_utf8(out.stdout).ok()?;
    let v = v.trim().strip_prefix(bin)?.trim().to_string();
    crate::Release::parse(&v).map(|_| v)
}

/// Another release version of the same length as `a`: same semver, another commit.
pub fn other_version(a: &str) -> String {
    let (semver, sha) = a.split_once('+').unwrap();
    format!(
        "{semver}+{}",
        if sha == "bbbbbbb" {
            "ccccccc"
        } else {
            "bbbbbbb"
        }
    )
}

/// `exe` with every copy of version `a` swapped for `b` (same length), written to `out` and
/// runnable (re-signed ad hoc on macOS, which won't run it otherwise). Returns its bytes.
pub fn patched(exe: &[u8], a: &str, b: &str, out: &Path) -> Vec<u8> {
    assert_eq!(a.len(), b.len());
    let mut bytes = exe.to_vec();
    let (mut n, mut i) = (0, 0);
    while let Some(at) = bytes[i..].windows(a.len()).position(|w| w == a.as_bytes()) {
        bytes[i + at..i + at + a.len()].copy_from_slice(b.as_bytes());
        i += at + a.len();
        n += 1;
    }
    assert!(n > 0, "version string not found in the binary");
    write_exe(out, &bytes);
    if cfg!(target_os = "macos") {
        let ok = std::process::Command::new("codesign")
            .args(["-f", "-s", "-"])
            .arg(out)
            .status()
            .unwrap();
        assert!(ok.success());
        bytes = std::fs::read(out).unwrap();
    }
    bytes
}

pub fn write_exe(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

pub fn tar_gz(name: &str, data: &[u8]) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o755);
    h.set_cksum();
    b.append_data(&mut h, name, data).unwrap();
    b.into_inner().unwrap().finish().unwrap()
}

/// A minisign signature of `data` by fixture key `key` ("test.key", "other.key").
pub fn sign(key: &str, data: &[u8], comment: &str) -> String {
    let key = std::fs::read_to_string(format!("{FIXTURES}/{key}")).unwrap();
    let sk = minisign::SecretKeyBox::from_string(&key)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, data, Some(comment), None)
        .unwrap()
        .into_string()
}

/// Edge release files for `bin` at `version`: the tarball, its signature by `key`, edge.json.
pub fn edge_release(bin: &str, exe: &[u8], version: &str, key: &str) -> HashMap<String, Vec<u8>> {
    let asset = crate::asset("", crate::Channel::Edge, version, crate::TARGET).unwrap();
    let tarball = tar_gz(&crate::exe_name(bin), exe);
    let sig = sign(key, &tarball, &format!("mis {version} {}", asset.name));
    let json = format!(
        r#"{{"sha":"x","version":"{version}","assets":["{}"]}}"#,
        asset.name
    );
    HashMap::from([
        (
            format!("download/edge/{}.minisig", asset.name),
            sig.into_bytes(),
        ),
        (format!("download/edge/{}", asset.name), tarball),
        ("download/edge/edge.json".into(), json.into_bytes()),
    ])
}

/// Serves `files` by path; returns the base URL and the paths asked for.
pub async fn release_server(files: HashMap<String, Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::extract::{Path as UrlPath, State};
    use axum::http::StatusCode;
    type S = (Arc<HashMap<String, Vec<u8>>>, Arc<Mutex<Vec<String>>>);
    let hits = Arc::new(Mutex::new(vec![]));
    let app = axum::Router::new()
        .route(
            "/{*path}",
            axum::routing::get(
                |State((files, hits)): State<S>, UrlPath(path): UrlPath<String>| async move {
                    hits.lock().unwrap().push(path.clone());
                    files.get(&path).cloned().ok_or(StatusCode::NOT_FOUND)
                },
            ),
        )
        .with_state((Arc::new(files), hits.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, hits)
}

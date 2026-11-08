//! Apps: static bundles published as numbered releases, served on their own listener.
//! On disk under `<state>/apps`: `<slug>/releases/<n>/` (the bundle plus `.release.json`,
//! all kept), `<slug>/active` (the live n) and `<slug>/policy.json` (its members, see
//! [`crate::app_access`]). `unpack` and the path rules are pure; `Apps` is the filesystem
//! shell around them.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::TimeZone;
use mis_types::{AppBackup, AppMember, AppRelease, AppRole, AppView, Restored};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app_access::{self, Policy};
use crate::backups::{self, BackupError, Take};
use crate::records_db::Records;

/// Largest bundle, its files' bytes added up.
pub const MAX_BYTES: u64 = 20 << 20;
pub const MAX_FILES: usize = 2000;
/// Largest upload: a bundle that doesn't compress, plus tar headers.
pub const MAX_ARCHIVE: usize = MAX_BYTES as usize + (2 << 20);
/// Tar headers and padding allowed beyond `MAX_BYTES` when expanding: ~4 KiB a file.
const TAR_OVERHEAD: u64 = 8 << 20;
const MANIFEST: &str = "app.json";
/// Beside the bundle in its release folder; bundles can't hold it (dot files are refused).
const META: &str = ".release.json";
/// Beside `releases/`, so no bundle can ship or replace it.
const POLICY: &str = "policy.json";
const MAX_TITLE_CHARS: usize = 80;

/// `[a-z0-9][a-z0-9-]{0,31}`: `pool`, `chores-2`.
pub fn valid_slug(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=32).contains(&s.len()) && s.starts_with(ok) && s.chars().all(|c| ok(c) || c == '-')
}

/// `app.json` at the top of a bundle.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub title: String,
    /// A file in the bundle.
    #[serde(default)]
    pub icon: Option<String>,
    pub clients: Clients,
}

/// Each way the app can be run: an entry file in the bundle.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clients {
    /// The page a browser opens, e.g. `index.html`.
    pub web: String,
}

/// A checked bundle, ready to store.
#[derive(Debug)]
pub struct Bundle {
    pub manifest: Manifest,
    /// Path (`/`-separated, relative) to contents.
    pub files: BTreeMap<String, Vec<u8>>,
    pub sha256: String,
    pub size: u64,
}

/// `a/b.js` as a path inside a bundle or release: `/`-separated, relative, no empty, `.`,
/// `..` or hidden (dot) segments, no `\`, `:` (a Windows drive or stream) or NUL. None: not one.
pub fn clean_path(raw: &str) -> Option<&str> {
    let seg_ok = |s: &str| !s.is_empty() && !s.starts_with('.') && !s.contains(['\\', ':', '\0']);
    raw.split('/').all(seg_ok).then_some(raw)
}

/// A tar entry's path as a bundle path; `./` prefixes dropped. Ok(None): the top folder.
/// Parsed from the archive's bytes, not as an OS path, so `a\b` is refused on Windows too.
fn entry_path(raw: &[u8]) -> Result<Option<String>, String> {
    let shown = String::from_utf8_lossy(raw);
    let p = std::str::from_utf8(raw).map_err(|_| format!("{shown}: not UTF-8"))?;
    if p.starts_with('/') {
        return Err(format!("{shown}: absolute path"));
    }
    let mut segs = vec![];
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(format!("{shown}: `..` in path")),
            s => segs.push(s),
        }
    }
    if segs.is_empty() {
        return Ok(None);
    }
    let joined = segs.join("/");
    if reserved(&joined) {
        return Err(format!("{shown}: `api/` belongs to the server"));
    }
    match clean_path(&joined) {
        Some(_) => Ok(Some(joined)),
        None => Err(format!("{shown}: hidden or odd name")),
    }
}

/// A `.tar.gz` bundle, checked: only regular files and folders (no links or devices),
/// relative paths, at most `MAX_FILES` files of `MAX_BYTES` in all, and a valid `app.json`
/// whose entry and icon are in it. Expanded in memory; nothing is written.
pub fn unpack(tar_gz: &[u8]) -> Result<Bundle, String> {
    let bad = |e: std::io::Error| format!("bad archive: {e}");
    // Bounds what a small gzip can expand to, headers included.
    let stream = flate2::read::GzDecoder::new(tar_gz).take(MAX_BYTES + TAR_OVERHEAD);
    let mut ar = tar::Archive::new(stream);
    let mut files = BTreeMap::new();
    let mut size = 0u64;
    for entry in ar.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let path = entry_path(&entry.path_bytes())?;
        let kind = entry.header().entry_type();
        match kind {
            tar::EntryType::Directory | tar::EntryType::XGlobalHeader => continue,
            tar::EntryType::Regular | tar::EntryType::Continuous => {}
            other => return Err(format!("{}: {other:?} entries aren't allowed", show(&path))),
        }
        let path = path.ok_or("a file named like the top folder")?;
        let len = entry.size();
        size = size.saturating_add(len);
        if size > MAX_BYTES {
            return Err(format!("bundle is over {} MiB", MAX_BYTES >> 20));
        }
        if files.len() == MAX_FILES {
            return Err(format!("bundle has over {MAX_FILES} files"));
        }
        let mut bytes = Vec::with_capacity(len as usize);
        (&mut entry)
            .take(len)
            .read_to_end(&mut bytes)
            .map_err(bad)?;
        if bytes.len() as u64 != len {
            return Err("bad archive: a file is cut short".into());
        }
        if files.insert(path.clone(), bytes).is_some() {
            return Err(format!("{path}: listed twice"));
        }
    }
    if ar.into_inner().limit() == 0 {
        return Err("bad archive: expands too far".into());
    }
    // `a` and `a/b` can't both be files.
    for path in files.keys() {
        let mut prefixes = path.match_indices('/').map(|(i, _)| &path[..i]);
        if let Some(p) = prefixes.find(|p| files.contains_key(*p)) {
            return Err(format!("{p}: both a file and a folder"));
        }
    }
    let manifest = manifest(&files)?;
    Ok(Bundle {
        manifest,
        sha256: digest(&files),
        size,
        files,
    })
}

fn show(path: &Option<String>) -> &str {
    path.as_deref().unwrap_or(".")
}

/// `app.json`, checked against the bundle's `files`.
fn manifest(files: &BTreeMap<String, Vec<u8>>) -> Result<Manifest, String> {
    let raw = files.get(MANIFEST).ok_or("no app.json at the top")?;
    let mut m: Manifest = serde_json::from_slice(raw).map_err(|e| format!("app.json: {e}"))?;
    m.title = m.title.trim().to_string();
    if m.title.is_empty() || m.title.contains(['\n', '\r']) {
        return Err("app.json: title must be one non-empty line".into());
    }
    if m.title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!(
            "app.json: title is over {MAX_TITLE_CHARS} characters"
        ));
    }
    let named = |what: &str, p: &str| match clean_path(p) {
        Some(p) if files.contains_key(p) => Ok(()),
        _ => Err(format!("app.json: {what} {p:?} isn't a file in the bundle")),
    };
    named("clients.web", &m.clients.web)?;
    // Served at `/a/<slug>/`, so its relative URLs must resolve from the top.
    if m.clients.web.contains('/') {
        return Err("app.json: clients.web must be at the top of the bundle".into());
    }
    if let Some(icon) = &m.icon {
        named("icon", icon)?;
    }
    Ok(m)
}

/// SHA-256 over each file's path, length and bytes, in path order: the same files hash
/// the same however they were archived.
fn digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    for (path, bytes) in files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A release folder's name as its n: `1`, `12`; not `0`, `01` or `.tmp-…`.
pub fn release_n(name: &str) -> Option<u32> {
    let n: u32 = name.parse().ok()?;
    (n > 0 && n.to_string() == name).then_some(n)
}

/// The n after every existing one.
pub fn next_release(existing: impl IntoIterator<Item = u32>) -> u32 {
    existing.into_iter().max().unwrap_or(0) + 1
}

/// `api` and `api/…` inside an app: the server's routes, never bundle files.
pub fn reserved(path: &str) -> bool {
    path.split('/').next() == Some("api")
}

/// Where `/a/` is reached: `public` (e.g. `https://home.example.com`), else the apps
/// listener's address if loopback. None: not served, or no `public` for a non-loopback bind.
pub fn base_url(public: Option<&str>, bind: Option<std::net::SocketAddr>) -> Option<String> {
    let bind = bind?;
    match public {
        Some(p) => Some(p.trim_end_matches('/').to_string()),
        // The listener answers no other non-loopback Host, so such a link would only 421.
        None if !bind.ip().is_loopback() => None,
        None => Some(format!("http://{bind}")),
    }
}

/// What a request path inside an app (after `/a/<slug>/`) serves: a file, else the
/// entry for an extensionless path (an SPA's own route), else nothing.
pub fn wants_entry_fallback(path: &str) -> bool {
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    !last.contains('.')
}

/// Content-Type by extension; unknown ones download as bytes.
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

/// `.release.json`: what listing shows of a release.
#[derive(Serialize, Deserialize)]
struct Meta {
    at: u64,
    sha256: String,
    size: u64,
}

#[derive(Debug, PartialEq)]
pub enum ActivateError {
    NoSuchRelease,
    Io(String),
}

#[derive(Debug, PartialEq)]
pub enum MemberError {
    NoSuchApp,
    /// Not an email address.
    Invalid(String),
    Io(String),
}

/// The apps folder. Publishing, activating, sharing and backups take `lock`, so each publish
/// gets its own n, concurrent shares don't drop each other and no prune races a restore.
pub struct Apps {
    dir: PathBuf,
    /// See [`base_url`].
    base_url: Option<String>,
    lock: Mutex<()>,
    /// Each app's records, shared with the apps listener so backups take the same locks.
    records: Arc<Records>,
}

/// The live release of an app: its folder and manifest.
pub struct Live {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Apps {
    pub fn new(dir: PathBuf, base_url: Option<String>) -> Self {
        Self {
            records: Arc::new(Records::new(dir.clone())),
            dir,
            base_url,
            lock: Mutex::new(()),
        }
    }

    pub fn records(&self) -> Arc<Records> {
        self.records.clone()
    }

    /// The apps folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn releases_dir(&self, slug: &str) -> PathBuf {
        self.dir.join(slug).join("releases")
    }

    /// Store `bundle` as `slug`'s next release, published at Unix millis `at`; its n.
    /// Written into a temp folder, then renamed into place whole.
    pub fn publish(
        &self,
        slug: &str,
        bundle: &Bundle,
        at: u64,
        activate: bool,
    ) -> anyhow::Result<u32> {
        let releases = self.releases_dir(slug);
        crate::images::private_dir(&releases)?;
        let tmp = releases.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
        let written = write_release(&tmp, bundle, at);
        let n = written.and_then(|()| {
            let _held = self.lock.lock().unwrap();
            let n = next_release(self.release_ns(slug));
            if activate {
                self.pre_activation(slug, n, at)?;
            }
            std::fs::rename(&tmp, releases.join(n.to_string()))?;
            if activate {
                self.write_active(slug, n)?;
            }
            Ok(n)
        });
        if n.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        n
    }

    /// Make release `n` of `slug` the live one, at Unix millis `at`.
    pub fn activate(&self, slug: &str, n: u32, at: u64) -> Result<(), ActivateError> {
        let _held = self.lock.lock().unwrap();
        if !self.release_ns(slug).contains(&n) {
            return Err(ActivateError::NoSuchRelease);
        }
        self.pre_activation(slug, n, at)
            .map_err(|e| ActivateError::Io(format!("{e:#}")))?;
        self.write_active(slug, n)
            .map_err(|e| ActivateError::Io(e.to_string()))
    }

    /// Snapshot `slug`'s records before release `n` goes live; failing, it doesn't.
    fn pre_activation(&self, slug: &str, n: u32, at: u64) -> anyhow::Result<()> {
        let app = self.dir.join(slug);
        match backups::take(&app, &self.records, slug, Take::PreActivation(n), at) {
            Ok(_) => Ok(()),
            Err(e) => anyhow::bail!("backing up before activating: {e:?}"),
        }
    }

    /// `slug`'s snapshots, oldest first. None: no such app.
    pub fn backups(&self, slug: &str) -> Option<Vec<AppBackup>> {
        let _held = self.lock.lock().unwrap();
        let exists = !self.release_ns(slug).is_empty();
        exists.then(|| backups::list(&self.dir.join(slug)))
    }

    /// Snapshot `slug` now (Unix millis `at`). None: it has no records yet.
    pub fn backup(&self, slug: &str, at: u64) -> Result<Option<AppBackup>, BackupError> {
        let _held = self.lock.lock().unwrap();
        if self.release_ns(slug).is_empty() {
            return Err(BackupError::NoSuchApp);
        }
        backups::take(&self.dir.join(slug), &self.records, slug, Take::Manual, at)
    }

    /// See [`backups::restore`].
    pub fn restore(
        &self,
        slug: &str,
        name: &str,
        with_policy: bool,
        at: u64,
    ) -> Result<Restored, BackupError> {
        let _held = self.lock.lock().unwrap();
        if self.release_ns(slug).is_empty() {
            return Err(BackupError::NoSuchApp);
        }
        let app = self.dir.join(slug);
        backups::restore(&app, &self.records, slug, name, with_policy, at)
    }

    /// Each app's daily snapshot, if owed one at `now` (days in `tz`): the ones taken.
    pub fn daily_backups<Tz: TimeZone>(&self, now: u64, tz: &Tz) -> Vec<AppBackup> {
        let mut taken = vec![];
        for app in self.list() {
            let _held = self.lock.lock().unwrap();
            let dir = self.dir.join(&app.slug);
            match backups::daily(&dir, &self.records, &app.slug, now, tz) {
                Ok(Some(made)) => {
                    tracing::info!("apps/{}: backed up as {}", app.slug, made.name);
                    taken.push(made);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("daily backup of {}: {e:?}", app.slug),
            }
        }
        taken
    }

    fn write_active(&self, slug: &str, n: u32) -> std::io::Result<()> {
        crate::push::write_private(
            &self.dir.join(slug).join("active"),
            format!("{n}\n").as_bytes(),
        )
    }

    /// `slug`'s release numbers, ascending.
    fn release_ns(&self, slug: &str) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(self.releases_dir(slug)) else {
            return vec![];
        };
        let mut ns: Vec<u32> = (entries.flatten())
            .filter_map(|e| release_n(e.file_name().to_str()?))
            .collect();
        ns.sort_unstable();
        ns
    }

    fn active_n(&self, slug: &str) -> Option<u32> {
        let raw = std::fs::read_to_string(self.dir.join(slug).join("active")).ok()?;
        release_n(raw.trim())
    }

    fn release_manifest(&self, slug: &str, n: u32) -> Option<Manifest> {
        let path = self.releases_dir(slug).join(n.to_string()).join(MANIFEST);
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// `slug`'s live release, if it has one.
    pub fn live(&self, slug: &str) -> Option<Live> {
        let n = self.active_n(slug)?;
        Some(Live {
            dir: self.releases_dir(slug).join(n.to_string()),
            manifest: self.release_manifest(slug, n)?,
        })
    }

    /// `slug`'s members. Missing: shared with no one. Unreadable: also no one (fail closed).
    pub fn policy(&self, slug: &str) -> Policy {
        self.read_policy(slug).unwrap_or_else(|e| {
            tracing::warn!("apps/{slug}/policy.json: {e}; shared with no one");
            Policy::default()
        })
    }

    fn read_policy(&self, slug: &str) -> Result<Policy, String> {
        match std::fs::read(self.dir.join(slug).join(POLICY)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Give `email` `role` in `slug` (None: remove them); its members after.
    pub fn set_member(
        &self,
        slug: &str,
        email: &str,
        role: Option<AppRole>,
    ) -> Result<Vec<AppMember>, MemberError> {
        let _held = self.lock.lock().unwrap();
        if self.release_ns(slug).is_empty() {
            return Err(MemberError::NoSuchApp);
        }
        // A policy that doesn't parse is left for a person to fix, not overwritten.
        let policy = self.read_policy(slug).map_err(MemberError::Io)?;
        let policy = app_access::set_member(policy, email, role).map_err(MemberError::Invalid)?;
        let json = serde_json::to_vec_pretty(&policy).expect("policy serializes");
        crate::push::write_private(&self.dir.join(slug).join(POLICY), &json)
            .map_err(|e| MemberError::Io(e.to_string()))?;
        Ok(policy.list())
    }

    /// `/a/<slug>/` on this server, if it is served.
    pub fn url(&self, slug: &str) -> Option<String> {
        Some(format!("{}/a/{slug}/", self.base_url.as_ref()?))
    }

    /// Every app with at least one release, by slug.
    pub fn list(&self) -> Vec<AppView> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        let mut slugs: Vec<String> = (entries.flatten())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|s| valid_slug(s))
            .collect();
        slugs.sort();
        slugs.iter().filter_map(|s| self.view(s)).collect()
    }

    fn view(&self, slug: &str) -> Option<AppView> {
        let releases: Vec<AppRelease> = (self.release_ns(slug).into_iter())
            .filter_map(|n| {
                let path = self.releases_dir(slug).join(n.to_string()).join(META);
                let m: Meta = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
                Some(AppRelease {
                    n,
                    at: m.at,
                    sha256: m.sha256,
                    size: m.size,
                })
            })
            .collect();
        let active = self
            .active_n(slug)
            .filter(|n| releases.iter().any(|r| r.n == *n));
        let shown = active.or(releases.last().map(|r| r.n))?;
        Some(AppView {
            slug: slug.into(),
            title: self.release_manifest(slug, shown)?.title.trim().into(),
            url: self.url(slug),
            active,
            releases,
            members: self.policy(slug).list(),
        })
    }
}

/// `bundle`'s files and `.release.json` into new folder `dir`, readable only by this user.
fn write_release(dir: &Path, bundle: &Bundle, at: u64) -> anyhow::Result<()> {
    crate::images::private_dir(dir)?;
    let meta = Meta {
        at,
        sha256: bundle.sha256.clone(),
        size: bundle.size,
    };
    let meta = serde_json::to_vec(&meta).expect("meta serializes");
    let files = bundle.files.iter().map(|(p, b)| (p.as_str(), b.as_slice()));
    for (path, bytes) in files.chain([(META, meta.as_slice())]) {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            crate::images::private_dir(parent)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&path)?.write_all(bytes)?;
    }
    Ok(())
}

/// The bytes of `rel` (a [`clean_path`]) under `root`: a regular file reached without
/// following a symlink anywhere below `root`. None: not one.
pub fn read_file(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let path = root.canonicalize().ok()?.join(rel);
    if path.canonicalize().ok()? != path {
        return None;
    }
    // Opened without following a symlink swapped in since, or waiting on a FIFO.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut opts, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = opts.open(&path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = vec![];
    file.take(MAX_BYTES).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.tar.gz` of `(path, bytes)` files; paths written raw, so `../x` gets through.
    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, bytes) in files {
            let mut h = tar::Header::new_old();
            h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, *bytes).unwrap();
        }
        gz(&b.into_inner().unwrap())
    }

    fn gz(tar: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(tar).unwrap();
        e.finish().unwrap()
    }

    /// One entry of `kind` (a link, device…) named `path`.
    fn tar_gz_of(path: &str, kind: tar::EntryType) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_entry_type(kind);
        h.set_size(0);
        if kind.is_symlink() || kind.is_hard_link() {
            h.set_link_name("/etc/passwd").unwrap();
        }
        h.set_cksum();
        b.append(&h, &[][..]).unwrap();
        gz(&b.into_inner().unwrap())
    }

    const APP_JSON: &[u8] = br#"{"title":"Pool","clients":{"web":"index.html"}}"#;

    fn err(files: &[(&str, &[u8])]) -> String {
        unpack(&tar_gz(files)).unwrap_err()
    }

    #[test]
    fn slugs() {
        for ok in ["pool", "a", "chores-2", "0", &"a".repeat(32)] {
            assert!(valid_slug(ok), "{ok}");
        }
        for bad in ["", "-a", "Pool", "a_b", "a.b", "../a", &"a".repeat(33), "é"] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn unpacks_a_bundle() {
        let b = unpack(&tar_gz(&[
            ("./app.json", APP_JSON),
            ("index.html", b"<p>hi"),
            ("js/app.js", b"1"),
        ]))
        .unwrap();
        assert_eq!(b.manifest.title, "Pool");
        assert_eq!(b.manifest.clients.web, "index.html");
        let paths: Vec<_> = b.files.keys().map(String::as_str).collect();
        assert_eq!(paths, ["app.json", "index.html", "js/app.js"]);
        assert_eq!(b.size, APP_JSON.len() as u64 + 6);
        // The same files, archived in another order, hash the same; other bytes don't.
        let again = unpack(&tar_gz(&[
            ("js/app.js", b"1"),
            ("index.html", b"<p>hi"),
            ("app.json", APP_JSON),
        ]))
        .unwrap();
        assert_eq!(again.sha256, b.sha256);
        let other = (unpack(&tar_gz(&[("app.json", APP_JSON), ("index.html", b"<p>ho")]))).unwrap();
        assert_ne!(other.sha256, b.sha256);
        assert_eq!(b.sha256.len(), 64);
    }

    #[test]
    fn refuses_escaping_paths() {
        let page: &[u8] = b"x";
        let ok = ("app.json", APP_JSON);
        assert!(err(&[ok, ("index.html", page), ("../evil", page)]).contains("`..`"));
        assert!(err(&[ok, ("index.html", page), ("a/../../evil", page)]).contains("`..`"));
        assert!(err(&[ok, ("index.html", page), ("/etc/evil", page)]).contains("absolute"));
        assert!(err(&[ok, ("index.html", page), (".env", page)]).contains("hidden"));
        assert!(err(&[ok, ("index.html", page), ("a\\b", page)]).contains("odd"));
        assert!(err(&[ok, ("index.html", page), ("C:evil", page)]).contains("odd"));
    }

    #[test]
    fn refuses_links_and_devices() {
        use tar::EntryType::*;
        for kind in [Symlink, Link, Char, Block, Fifo] {
            let e = unpack(&tar_gz_of("x", kind)).unwrap_err();
            assert!(e.contains("entries aren't allowed"), "{kind:?}: {e}");
        }
    }

    #[test]
    fn refuses_duplicates_and_file_folder_clashes() {
        let ok = ("app.json", APP_JSON);
        assert!(err(&[ok, ("index.html", b"1"), ("index.html", b"2")]).contains("twice"));
        assert!(err(&[ok, ("index.html", b"1"), ("index.html/x", b"2")]).contains("both"));
    }

    #[test]
    fn caps_size_and_count() {
        let big = vec![0u8; MAX_BYTES as usize];
        let e = err(&[("app.json", APP_JSON), ("index.html", &big)]);
        assert!(e.contains("over 20 MiB"), "{e}");
        let names: Vec<String> = (0..=MAX_FILES).map(|i| format!("f{i}")).collect();
        let mut files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        files.push(("app.json", APP_JSON));
        assert!(err(&files).contains("over 2000 files"));
    }

    #[test]
    fn a_gzip_bomb_stops_at_the_cap() {
        // One file claiming more than fits, then zeros: refused without expanding it all.
        let mut h = tar::Header::new_gnu();
        h.set_path("x").unwrap();
        h.set_size(1 << 40);
        h.set_cksum();
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(h.as_bytes()).unwrap();
        for _ in 0..64 {
            e.write_all(&[0; 1 << 20]).unwrap();
        }
        let bomb = e.finish().unwrap();
        assert!(unpack(&bomb).unwrap_err().contains("over 20 MiB"));
        // Not a tar of headers either: endless empty folders run into the expansion cap.
        let mut b = tar::Builder::new(Vec::new());
        for i in 0..=(MAX_BYTES + TAR_OVERHEAD) / 512 {
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("d{i}")).unwrap();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_cksum();
            b.append(&h, &[][..]).unwrap();
        }
        let e = unpack(&gz(&b.into_inner().unwrap())).unwrap_err();
        assert!(e.contains("bad archive"), "{e}");
    }

    #[test]
    fn manifest_must_name_files_in_the_bundle() {
        let page = ("index.html", &b"x"[..]);
        let m = |json: &'static str| err(&[("app.json", json.as_bytes()), page]);
        assert!(err(&[page]).contains("no app.json"));
        assert!(m(r#"{"title":"P","clients":{}}"#).contains("missing field `web`"));
        assert!(
            m(r#"{"title":"P","clients":{"web":"index.html","xr":"a"}}"#)
                .contains("unknown field `xr`")
        );
        assert!(m(r#"{"title":"P","clients":{"web":"main.html"}}"#).contains("clients.web"));
        assert!(m(r#"{"title":"P","clients":{"web":"../index.html"}}"#).contains("clients.web"));
        let nested = br#"{"title":"P","clients":{"web":"web/index.html"}}"#;
        let e = err(&[("app.json", nested), ("web/index.html", b"x")]);
        assert!(e.contains("at the top"), "{e}");
        assert!(
            m(r#"{"title":"P","icon":"i.png","clients":{"web":"index.html"}}"#).contains("icon")
        );
        assert!(m(r#"{"title":" ","clients":{"web":"index.html"}}"#).contains("title"));
        assert!(m(r#"{"title":"a\nb","clients":{"web":"index.html"}}"#).contains("title"));
        assert!(
            m(r#"{"title":"P","clients":{"web":"index.html"},"x":1}"#).contains("unknown field")
        );
        let icon = br#"{"title":" P ","icon":"i.png","clients":{"web":"index.html"}}"#;
        let b = unpack(&tar_gz(&[("app.json", icon), page, ("i.png", b"")])).unwrap();
        assert_eq!(b.manifest.title, "P");
    }

    #[test]
    fn paths_and_release_numbers() {
        assert_eq!(clean_path("a/b.js"), Some("a/b.js"));
        for bad in [
            "",
            "a//b",
            "a/",
            "/a",
            "./a",
            "a/../b",
            ".release.json",
            "a\\b",
        ] {
            assert_eq!(clean_path(bad), None, "{bad}");
        }
        assert_eq!(release_n("12"), Some(12));
        for bad in ["0", "01", "+1", "-1", ".tmp-x", ""] {
            assert_eq!(release_n(bad), None, "{bad}");
        }
        assert_eq!(next_release([]), 1);
        assert_eq!(next_release([3, 1, 2]), 4);
    }

    #[test]
    fn urls_and_types() {
        let addr = "127.0.0.1:7475".parse().ok();
        assert_eq!(
            base_url(Some("https://home.example.com/"), addr).unwrap(),
            "https://home.example.com"
        );
        assert_eq!(base_url(None, addr).unwrap(), "http://127.0.0.1:7475");
        assert_eq!(base_url(None, None), None);
        assert_eq!(base_url(Some("https://h.example"), None), None);
        assert_eq!(base_url(None, "0.0.0.0:7475".parse().ok()), None);
        assert_eq!(base_url(None, "192.168.1.5:7475".parse().ok()), None);
        assert!(reserved("api") && reserved("api/v1/me") && reserved("api/"));
        assert!(!reserved("apis/x") && !reserved("js/api") && !reserved(""));
        let any = "[::]:7475".parse().ok();
        assert_eq!(
            base_url(Some("https://h.example"), any).unwrap(),
            "https://h.example"
        );
        let apps = Apps::new("x".into(), Some("https://h.example".into()));
        assert_eq!(apps.url("pool").unwrap(), "https://h.example/a/pool/");
        assert_eq!(Apps::new("x".into(), None).url("pool"), None);
        assert_eq!(content_type("a/B.JS"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("blob"), "application/octet-stream");
        assert!(wants_entry_fallback("scores/2026"));
        assert!(wants_entry_fallback("scores/"));
        assert!(!wants_entry_fallback("missing.png"));
    }

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mis-apps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn publishes_numbered_releases_and_activates() {
        let dir = temp();
        let apps = Apps::new(dir.clone(), None);
        let bundle =
            |page: &[u8]| unpack(&tar_gz(&[("app.json", APP_JSON), ("index.html", page)])).unwrap();
        assert_eq!(apps.publish("pool", &bundle(b"1"), 5, false).unwrap(), 1);
        assert_eq!(apps.list()[0].active, None);
        assert!(apps.live("pool").is_none());
        assert_eq!(apps.publish("pool", &bundle(b"2"), 6, true).unwrap(), 2);
        let live = apps.live("pool").unwrap();
        assert_eq!(read_file(&live.dir, "index.html").unwrap(), b"2");
        assert_eq!(
            apps.activate("pool", 3, 1),
            Err(ActivateError::NoSuchRelease)
        );
        apps.activate("pool", 1, 1).unwrap();
        let view = &apps.list()[0];
        assert_eq!(
            (view.slug.as_str(), view.title.as_str(), view.active),
            ("pool", "Pool", Some(1))
        );
        let ns: Vec<_> = view.releases.iter().map(|r| (r.n, r.at, r.size)).collect();
        assert_eq!(
            ns,
            [
                (1, 5, APP_JSON.len() as u64 + 1),
                (2, 6, APP_JSON.len() as u64 + 1)
            ]
        );
        // No temp folders left behind.
        let left = std::fs::read_dir(dir.join("pool/releases"))
            .unwrap()
            .count();
        assert_eq!(left, 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_publishes_get_distinct_numbers() {
        let dir = temp();
        let apps = std::sync::Arc::new(Apps::new(dir.clone(), None));
        let ns: Vec<u32> = (0..8)
            .map(|_| {
                let apps = apps.clone();
                std::thread::spawn(move || {
                    let b =
                        unpack(&tar_gz(&[("app.json", APP_JSON), ("index.html", b"")])).unwrap();
                    apps.publish("pool", &b, 0, false).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        let mut sorted = ns.clone();
        sorted.sort();
        assert_eq!(sorted, (1..=8).collect::<Vec<_>>());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bundles_cannot_ship_the_servers_api_paths() {
        let err = unpack(&tar_gz(&[
            ("app.json", APP_JSON),
            ("index.html", b""),
            ("api/v1/me", b"x"),
        ]));
        assert!(err.unwrap_err().contains("`api/`"));
        let ok = unpack(&tar_gz(&[
            ("app.json", APP_JSON),
            ("index.html", b""),
            ("apis/x", b"x"),
        ]));
        assert!(ok.is_ok());
    }

    #[test]
    fn members_persist_outside_the_bundle_and_bad_policies_fail_closed() {
        let dir = temp();
        let apps = Apps::new(dir.clone(), None);
        let editor = Some(AppRole::Editor);
        assert_eq!(
            apps.set_member("pool", "a@b.c", editor),
            Err(MemberError::NoSuchApp)
        );
        let b = unpack(&tar_gz(&[("app.json", APP_JSON), ("index.html", b"")])).unwrap();
        apps.publish("pool", &b, 0, true).unwrap();
        let shared = apps.set_member("pool", "Me@B.c", editor).unwrap();
        let me = AppMember {
            email: "me@b.c".into(),
            role: AppRole::Editor,
        };
        assert_eq!(shared, vec![me.clone()]);
        assert_eq!(apps.list()[0].members, [me]);
        let raw = std::fs::read_to_string(dir.join("pool/policy.json")).unwrap();
        assert!(raw.contains(r#""me@b.c": "editor""#), "{raw}");
        assert!(matches!(
            apps.set_member("pool", "nope", editor),
            Err(MemberError::Invalid(_))
        ));
        assert_eq!(apps.set_member("pool", "me@b.c", None).unwrap(), []);
        // Unparseable: nobody gets in, and sharing refuses to overwrite it.
        std::fs::write(dir.join("pool/policy.json"), "{oops").unwrap();
        assert_eq!(apps.policy("pool"), Policy::default());
        assert!(matches!(
            apps.set_member("pool", "a@b.c", editor),
            Err(MemberError::Io(_))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_file_follows_no_symlinks() {
        let dir = temp();
        std::fs::create_dir(dir.join("r")).unwrap();
        std::fs::write(dir.join("secret"), "s").unwrap();
        std::fs::write(dir.join("r/ok"), "ok").unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("r/link")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("r/up")).unwrap();
        let root = dir.join("r");
        assert_eq!(read_file(&root, "ok").unwrap(), b"ok");
        assert_eq!(read_file(&root, "link"), None);
        assert_eq!(read_file(&root, "up/secret"), None);
        assert_eq!(read_file(&root, "missing"), None);
        assert_eq!(read_file(&dir, "r"), None); // a folder
        std::fs::remove_dir_all(dir).unwrap();
    }
}

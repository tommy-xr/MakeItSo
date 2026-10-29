//! Web Push to the user's browsers when a task needs them: subscriptions and
//! the VAPID key persisted in a state dir, and the sender for `Effect::Notify`.
//! Payloads are end-to-end encrypted (RFC 8291) but still transit the browser
//! vendor's push service.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use axum::http::{header, HeaderValue, Uri};
use base64ct::{Base64UrlUnpadded, Encoding};
use mis_types::TaskId;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use serde::{Deserialize, Serialize};
use web_push_native::{p256, Auth, WebPushBuilder};

/// More than this and the oldest subscription goes.
pub const MAX_SUBSCRIPTIONS: usize = 20;
/// A notification nobody could deliver within this is stale.
pub const TTL: Duration = Duration::from_secs(3600);
const SUBS_FILE: &str = "push-subscriptions.json";
const KEY_FILE: &str = "vapid.pem";

/// One browser's `PushSubscription`; keys base64url.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// The decoded parts a push request needs, or why `sub` is unusable. Push
/// services are HTTPS; plain HTTP only to this machine (a local stand-in).
fn keys(sub: &Subscription) -> Result<(Uri, p256::PublicKey, Auth), String> {
    let uri: Uri = sub.endpoint.parse().map_err(|_| "bad endpoint")?;
    let loopback = |h: &str| {
        let h = h.trim_start_matches('[').trim_end_matches(']');
        h == "localhost"
            || h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    };
    match (uri.scheme_str(), uri.host()) {
        (Some("https"), Some(_)) => {}
        (Some("http"), Some(h)) if loopback(h) => {}
        _ => return Err("endpoint must be an https URL".into()),
    }
    let b64 = |s: &str| Base64UrlUnpadded::decode_vec(s.trim_end_matches('='));
    let key = b64(&sub.p256dh)
        .ok()
        .and_then(|k| p256::PublicKey::from_sec1_bytes(&k).ok())
        .ok_or("bad p256dh key")?;
    let auth = b64(&sub.auth).ok().filter(|a| a.len() == 16);
    let auth = auth.ok_or("bad auth secret")?;
    Ok((uri, key, Auth::clone_from_slice(&auth)))
}

/// Add `sub`, replacing one with the same endpoint (its keys may have changed);
/// past the cap the oldest goes. Newest last. False if it was already there as is
/// (pages re-send theirs on every connect).
pub fn add(subs: &mut Vec<Subscription>, sub: Subscription) -> bool {
    if subs.contains(&sub) {
        return false;
    }
    subs.retain(|s| s.endpoint != sub.endpoint);
    subs.push(sub);
    let over = subs.len().saturating_sub(MAX_SUBSCRIPTIONS);
    subs.drain(..over);
    true
}

/// The push service says this subscription is gone for good.
pub fn gone(status: u16) -> bool {
    matches!(status, 404 | 410)
}

/// What the service worker shows: `tag` makes a repeat replace, `url` opens the task.
pub fn payload(task: TaskId, title: &str, body: &str) -> String {
    serde_json::json!({
        "title": title,
        "body": body,
        "tag": task,
        "url": format!("/#task/{task}"),
    })
    .to_string()
}

/// Write `bytes` to `path` readable only by this user, atomically (temp + rename).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp); // so the mode below applies
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&tmp)?.write_all(bytes)?;
    std::fs::rename(&tmp, path)
}

/// The VAPID key in `dir` (PKCS#8 PEM), made on first run. Its public half is
/// what browsers subscribe with, so losing it orphans every subscription.
fn load_or_create_key(dir: &Path) -> anyhow::Result<p256::SecretKey> {
    let path = dir.join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(pem) => p256::SecretKey::from_pkcs8_pem(&pem)
            .ok()
            .with_context(|| format!("{} is not a P-256 key", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
            let pem = key.to_pkcs8_pem(LineEnding::LF)?;
            write_private(&path, pem.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// RFC 8292 `Authorization` for a push to `endpoint`: a JWT for its origin, valid
/// until `exp` (Unix seconds, at most a day out), and the key that checks it.
fn vapid_header(
    key: &jsonwebtoken::EncodingKey,
    public: &str,
    endpoint: &Uri,
    subject: &str,
    exp: u64,
) -> anyhow::Result<String> {
    let aud = match (endpoint.scheme_str(), endpoint.authority()) {
        (Some(scheme), Some(authority)) => format!("{scheme}://{authority}"),
        _ => anyhow::bail!("endpoint has no origin"),
    };
    let claims = serde_json::json!({ "aud": aud, "exp": exp, "sub": subject });
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    let jwt = jsonwebtoken::encode(&header, &claims, key)?;
    Ok(format!("vapid t={jwt}, k={public}"))
}

pub struct Push {
    dir: PathBuf,
    key: jsonwebtoken::EncodingKey,
    /// The key's public half, uncompressed P-256 point, base64url.
    public: String,
    /// VAPID `sub`: who push services contact about abuse.
    subject: String,
    subs: Mutex<Vec<Subscription>>,
    http: reqwest::Client,
}

impl Push {
    /// Load (or create) the key and subscriptions in `dir`. `subject` must be a
    /// `mailto:` or `https:` URL; Apple refuses anything else.
    pub fn open(dir: PathBuf, subject: String) -> anyhow::Result<Self> {
        if !(subject.starts_with("mailto:") || subject.starts_with("https://")) {
            anyhow::bail!("VAPID subject {subject:?} must start with mailto: or https://");
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let secret = load_or_create_key(&dir)?;
        let pem = secret.to_pkcs8_pem(LineEnding::LF)?;
        let public = secret.public_key().to_encoded_point(false);
        let path = dir.join(SUBS_FILE);
        // Unreadable: start empty; pages re-send their subscriptions when they connect.
        let subs = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!("ignoring {}: {e}", path.display());
                vec![]
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            dir,
            key: jsonwebtoken::EncodingKey::from_ec_pem(pem.as_bytes())?,
            public: Base64UrlUnpadded::encode_string(public.as_bytes()),
            subject,
            subs: Mutex::new(subs),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    /// The page's `applicationServerKey`.
    pub fn public_key(&self) -> String {
        self.public.clone()
    }

    pub fn subscribe(&self, sub: Subscription) -> Result<(), String> {
        keys(&sub)?;
        let host = host(&sub.endpoint);
        if self.update(|subs| add(subs, sub))? {
            tracing::info!(%host, "push subscription added");
        }
        Ok(())
    }

    pub fn unsubscribe(&self, endpoint: &str) {
        let removed = |subs: &mut Vec<Subscription>| {
            let n = subs.len();
            subs.retain(|s| s.endpoint != endpoint);
            subs.len() != n
        };
        if let Err(e) = self.update(removed) {
            tracing::warn!("{e}");
        }
    }

    /// Apply `f`; if it changed something, save, and keep the change only once
    /// saved. Under the lock, so saves keep order.
    fn update(&self, f: impl FnOnce(&mut Vec<Subscription>) -> bool) -> Result<bool, String> {
        let mut subs = self.subs.lock().unwrap();
        let mut next = subs.clone();
        if !f(&mut next) {
            return Ok(false);
        }
        let json = serde_json::to_vec_pretty(&next).expect("serializable");
        write_private(&self.dir.join(SUBS_FILE), &json)
            .map_err(|e| format!("saving push subscriptions: {e}"))?;
        *subs = next;
        Ok(true)
    }

    /// Push to every subscription; drop those the service says are gone.
    pub async fn send(&self, task: TaskId, title: &str, body: &str, urgent: bool) {
        let payload = payload(task, title, body);
        let subs = self.subs.lock().unwrap().clone();
        for sub in subs {
            let host = host(&sub.endpoint);
            match self.send_one(&sub, &payload, urgent).await {
                Ok(status) if gone(status) => {
                    tracing::info!(%host, status, "push subscription gone; dropped");
                    self.unsubscribe(&sub.endpoint);
                }
                Ok(status) if !(200..300).contains(&status) => {
                    tracing::warn!(%host, status, "push refused")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(%host, "push failed: {e:#}"),
            }
        }
    }

    async fn send_one(
        &self,
        sub: &Subscription,
        payload: &str,
        urgent: bool,
    ) -> anyhow::Result<u16> {
        let (uri, key, auth) = keys(sub).map_err(anyhow::Error::msg)?;
        let exp = crate::unix_secs() + TTL.as_secs();
        let vapid = vapid_header(&self.key, &self.public, &uri, &self.subject, exp)?;
        let req = WebPushBuilder::new(uri, key, auth)
            .with_valid_duration(TTL)
            .build(payload)?;
        let (mut parts, body) = req.into_parts();
        parts
            .headers
            .insert(header::AUTHORIZATION, vapid.try_into()?);
        let urgency = HeaderValue::from_static(if urgent { "high" } else { "normal" });
        parts.headers.insert("urgency", urgency);
        let req = reqwest::Request::try_from(axum::http::Request::from_parts(parts, body))?;
        // Errors carry the URL; the endpoint is a capability, kept out of logs.
        let resp = self.http.execute(req).await.map_err(|e| e.without_url())?;
        Ok(resp.status().as_u16())
    }
}

/// For logs: an endpoint is a bearer capability, its host isn't.
fn host(endpoint: &str) -> String {
    endpoint
        .parse::<Uri>()
        .ok()
        .and_then(|u| u.host().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(endpoint: &str) -> Subscription {
        let ua = p256::SecretKey::from_slice(&[7; 32]).unwrap();
        let point = ua.public_key().to_sec1_bytes();
        Subscription {
            endpoint: endpoint.into(),
            p256dh: Base64UrlUnpadded::encode_string(&point),
            auth: Base64UrlUnpadded::encode_string(&[1; 16]),
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("mis-push-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn add_replaces_same_endpoint_and_caps_dropping_oldest() {
        let mut subs = vec![];
        assert!(add(&mut subs, sub("https://p.example/a")));
        assert!(!add(&mut subs, sub("https://p.example/a")), "re-sent as is");
        let mut again = sub("https://p.example/a");
        again.auth = Base64UrlUnpadded::encode_string(&[2; 16]);
        assert!(add(&mut subs, again.clone()));
        assert_eq!(subs, vec![again]);
        for i in 0..MAX_SUBSCRIPTIONS {
            add(&mut subs, sub(&format!("https://p.example/{i}")));
        }
        assert_eq!(subs.len(), MAX_SUBSCRIPTIONS);
        assert_eq!(subs[0].endpoint, "https://p.example/0", "a went first");
        assert_eq!(
            subs.last().unwrap().endpoint,
            format!("https://p.example/{}", MAX_SUBSCRIPTIONS - 1)
        );
    }

    #[test]
    fn keys_reject_what_cannot_be_pushed_to() {
        assert!(keys(&sub("https://p.example/a")).is_ok());
        let padded = Subscription {
            auth: format!("{}==", sub("x").auth),
            ..sub("https://p.example/a")
        };
        assert!(keys(&padded).is_ok(), "browsers may pad");
        assert!(keys(&sub("http://127.0.0.1:9/a")).is_ok(), "local stand-in");
        assert!(keys(&sub("http://[::1]:9/a")).is_ok());
        for endpoint in [
            "",
            "/relative",
            "ftp://p.example/a",
            "not a url",
            "http://p.example/a",
            "http://169.254.169.254/a",
        ] {
            assert!(keys(&sub(endpoint)).is_err(), "{endpoint}");
        }
        let bad_key = Subscription {
            p256dh: Base64UrlUnpadded::encode_string(&[4; 65]),
            ..sub("https://p.example/a")
        };
        assert_eq!(keys(&bad_key).unwrap_err(), "bad p256dh key");
        let short_auth = Subscription {
            auth: Base64UrlUnpadded::encode_string(&[1; 8]),
            ..sub("https://p.example/a")
        };
        assert_eq!(keys(&short_auth).unwrap_err(), "bad auth secret");
    }

    #[test]
    fn only_404_and_410_mean_gone() {
        assert!(gone(404) && gone(410));
        assert!(!gone(201) && !gone(429) && !gone(500) && !gone(403));
    }

    #[test]
    fn payload_opens_the_task() {
        let task = uuid::Uuid::nil();
        let v: serde_json::Value = serde_json::from_str(&payload(task, "t", "b")).unwrap();
        assert_eq!(v["url"], format!("/#task/{task}"));
        assert_eq!(v["tag"], task.to_string());
        assert_eq!(
            (v["title"].as_str(), v["body"].as_str()),
            (Some("t"), Some("b"))
        );
    }

    #[test]
    fn key_and_subscriptions_survive_reopen_privately() {
        let dir = temp_dir();
        let push = Push::open(dir.clone(), "mailto:x@example.invalid".into()).unwrap();
        let public = push.public_key();
        push.subscribe(sub("https://p.example/a")).unwrap();
        push.subscribe(sub("https://p.example/b")).unwrap();
        push.unsubscribe("https://p.example/a");
        assert!(push.subscribe(sub("ftp://p.example/c")).is_err());
        drop(push);
        let push = Push::open(dir.clone(), "mailto:x@example.invalid".into()).unwrap();
        assert_eq!(push.public_key(), public);
        assert_eq!(*push.subs.lock().unwrap(), vec![sub("https://p.example/b")]);
        #[cfg(unix)]
        for f in [KEY_FILE, SUBS_FILE] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(f)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{f}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_subject_or_subscriptions_file() {
        let dir = temp_dir();
        assert!(Push::open(dir.clone(), "me@example.invalid".into()).is_err());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SUBS_FILE), "{").unwrap();
        let push = Push::open(dir.clone(), "https://example.invalid".into()).unwrap();
        assert!(
            push.subs.lock().unwrap().is_empty(),
            "corrupt list: start empty"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_corrupt_key_is_an_error_not_a_new_key() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(KEY_FILE), "garbage").unwrap();
        assert!(Push::open(dir.clone(), "mailto:s@example.invalid".into()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(KEY_FILE)).unwrap(),
            "garbage"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

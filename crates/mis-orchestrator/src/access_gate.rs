//! Shell around [`crate::access`]: holds the team's signing keys, fetches them
//! from Cloudflare, and refetches when a token names a key it doesn't have.

use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::access::{self, Identity, Jwks};

/// Header Cloudflare Access adds to every request it lets through.
pub const JWT_HEADER: &str = "cf-access-jwt-assertion";

/// At most one on-demand refetch per this interval, so bogus `kid`s can't
/// turn into a request flood against Cloudflare.
const REFETCH_EVERY: Duration = Duration::from_secs(60);

pub struct AccessGate {
    team: String,
    aud: String,
    keys: RwLock<Jwks>,
    /// None: keys were injected and are never fetched.
    certs_url: Option<String>,
    last_refetch: Mutex<Option<Instant>>,
    client: reqwest::Client,
}

impl AccessGate {
    /// Fixed keys, never fetched (tests).
    pub fn with_keys(team: String, aud: String, keys: Jwks) -> Self {
        Self::new(team, aud, keys, None)
    }

    /// Keys fetched from `certs_url` (see [`access::certs_url`]) by [`Self::refresh`].
    pub fn remote(team: String, aud: String, certs_url: String) -> Self {
        Self::new(team, aud, Jwks::default(), Some(certs_url))
    }

    fn new(team: String, aud: String, keys: Jwks, certs_url: Option<String>) -> Self {
        Self {
            team,
            aud,
            keys: RwLock::new(keys),
            certs_url,
            last_refetch: Mutex::new(None),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
        }
    }

    /// Replace the keys with a fresh fetch; on error the old keys stay.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let Some(url) = &self.certs_url else {
            return Ok(());
        };
        let body = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let keys: Jwks = serde_json::from_slice(&body)?;
        tracing::info!(keys = keys.keys.len(), "fetched Cloudflare Access keys");
        *self.keys.write().unwrap() = keys;
        Ok(())
    }

    /// Verify `jwt`, refetching keys once if it names an unknown `kid`.
    pub async fn verify(&self, jwt: &str) -> Result<Identity, access::Error> {
        let check = || {
            let keys = self.keys.read().unwrap();
            access::verify(jwt, &keys, &self.team, &self.aud, crate::unix_secs())
        };
        let r = check();
        if !matches!(r, Err(access::Error::UnknownKid)) || !self.refetch_due() {
            return r;
        }
        if let Err(e) = self.refresh().await {
            tracing::warn!("refetching Cloudflare Access keys: {e:#}");
        }
        check()
    }

    fn refetch_due(&self) -> bool {
        let mut last = self.last_refetch.lock().unwrap();
        if self.certs_url.is_none() || last.is_some_and(|t| t.elapsed() < REFETCH_EVERY) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }
}

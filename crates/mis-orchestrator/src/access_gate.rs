//! Shell around [`crate::access`]: holds the team's signing keys, fetches them
//! from Cloudflare, and refetches when they age or a token names a key it doesn't have.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::access::{self, Identity, Jwks};

/// Header Cloudflare Access adds to every request it lets through.
pub const JWT_HEADER: &str = "cf-access-jwt-assertion";

/// At most one refetch per this interval, so bogus `kid`s can't turn into a
/// request flood against Cloudflare.
const REFETCH_EVERY: Duration = Duration::from_secs(60);
/// Refetch keys this old, so a key Cloudflare retired stops being trusted.
const REFRESH_AFTER: Duration = Duration::from_secs(60 * 60);
/// Reject everything once keys are this old and can't be refetched.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub struct AccessGate {
    team: String,
    aud: String,
    /// Keys and when they were fetched (None: never).
    keys: RwLock<(Jwks, Option<Instant>)>,
    /// None: keys were injected and never age or refetch.
    certs_url: Option<String>,
    /// Last refetch attempt. Held across the fetch so concurrent callers wait
    /// for it and then see the new keys.
    last_refetch: tokio::sync::Mutex<Option<Instant>>,
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
            keys: RwLock::new((keys, None)),
            certs_url,
            last_refetch: Default::default(),
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
        *self.keys.write().unwrap() = (keys, Some(Instant::now()));
        Ok(())
    }

    /// Verify `jwt`, first refetching keys (rate-limited) if they are old or
    /// lack its `kid`.
    pub async fn verify(&self, jwt: &str) -> Result<Identity, access::Error> {
        let (r, stale) = self.check(jwt);
        if !stale && !matches!(r, Err(access::Error::UnknownKid)) {
            return r;
        }
        let mut last = self.last_refetch.lock().await;
        // A caller we waited on may have just refetched.
        let (r, stale) = self.check(jwt);
        if (!stale && !matches!(r, Err(access::Error::UnknownKid)))
            || last.is_some_and(|t| t.elapsed() < REFETCH_EVERY)
        {
            return r;
        }
        *last = Some(Instant::now());
        if let Err(e) = self.refresh().await {
            tracing::warn!("refetching Cloudflare Access keys: {e:#}");
        }
        self.check(jwt).0
    }

    /// Verify against the cached keys; also whether they are due a refetch.
    fn check(&self, jwt: &str) -> (Result<Identity, access::Error>, bool) {
        let (keys, fetched) = &*self.keys.read().unwrap();
        let age = match (&self.certs_url, fetched) {
            (None, _) => Duration::ZERO,
            (Some(_), Some(t)) => t.elapsed(),
            (Some(_), None) => Duration::MAX,
        };
        let r = match age > MAX_AGE {
            true => Err(access::Error::UnknownKid),
            false => access::verify(jwt, keys, &self.team, &self.aud, crate::unix_secs()),
        };
        (r, age > REFRESH_AFTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::tests::{keys, mint};

    /// Gate whose keys were fetched `age` ago from a local server serving `served`.
    async fn gate(age: Duration, served: &'static str) -> AccessGate {
        let app =
            axum::Router::new().route("/certs", axum::routing::get(move || async move { served }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/certs", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let g = AccessGate::remote("team".into(), "aud".into(), url);
        *g.keys.write().unwrap() = (keys(), Instant::now().checked_sub(age));
        g
    }

    fn jwt() -> String {
        let now = crate::unix_secs();
        let claims = serde_json::json!({
            "aud": "aud", "iss": "https://team.cloudflareaccess.com",
            "exp": now + 600, "email": "me@example.com",
        });
        mint(Some("test-kid"), &claims)
    }

    const NO_KEYS: &str = r#"{"keys":[]}"#;
    const HOUR: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn fresh_keys_are_used_without_refetch() {
        let g = gate(Duration::ZERO, NO_KEYS).await;
        g.verify(&jwt()).await.unwrap();
    }

    #[tokio::test]
    async fn old_keys_are_refetched_so_retired_keys_stop_working() {
        let g = gate(2 * HOUR, NO_KEYS).await;
        let r = g.verify(&jwt()).await;
        assert!(matches!(r, Err(access::Error::UnknownKid)), "{r:?}");
    }

    #[tokio::test]
    async fn old_keys_survive_a_failed_refetch_until_max_age() {
        let g = gate(2 * HOUR, "not json").await;
        g.verify(&jwt()).await.unwrap();
        let g = gate(MAX_AGE + HOUR, "not json").await;
        let r = g.verify(&jwt()).await;
        assert!(matches!(r, Err(access::Error::UnknownKid)), "{r:?}");
    }
}

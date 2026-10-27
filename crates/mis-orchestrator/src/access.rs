//! Cloudflare Access JWT verification (pure): checks a `Cf-Access-Jwt-Assertion`
//! against the team's signing keys, issuer, audience and validity window.

use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

/// Clock skew tolerated on `exp` / `nbf`, in seconds.
const LEEWAY: u64 = 60;

/// The team's key set, as served at [`certs_url`].
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

/// One signing key. Non-RSA keys parse but never match.
#[derive(Clone, Debug, Deserialize)]
pub struct Jwk {
    pub kid: String,
    pub kty: String,
    #[serde(default)]
    pub n: String,
    #[serde(default)]
    pub e: String,
}

/// Who Access let through: a user's email, or a service token's client id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
}

#[derive(Debug)]
pub enum Error {
    /// Not a JWT, or its header names no `kid`.
    Malformed,
    /// No RSA key with the token's `kid`; the key set may need refetching.
    UnknownKid,
    /// Signature, algorithm, issuer, audience or claim shape rejected.
    Rejected(ErrorKind),
    Expired,
    NotYetValid,
    /// Neither `email` nor `common_name` is set.
    NoSubject,
}

/// `https://<team>.cloudflareaccess.com`, the expected `iss`.
pub fn issuer(team: &str) -> String {
    format!("https://{team}.cloudflareaccess.com")
}

pub fn certs_url(team: &str) -> String {
    format!("{}/cdn-cgi/access/certs", issuer(team))
}

#[derive(Deserialize)]
struct Claims {
    exp: u64,
    nbf: Option<u64>,
    #[serde(default)]
    email: String,
    #[serde(default)]
    common_name: String,
}

/// Accept `jwt` only if an RS256 key in `keys` signed it for `aud`, issued by
/// `team`, and it is valid at `now` (unix seconds).
pub fn verify(jwt: &str, keys: &Jwks, team: &str, aud: &str, now: u64) -> Result<Identity, Error> {
    let kid = decode_header(jwt)
        .ok()
        .and_then(|h| h.kid)
        .ok_or(Error::Malformed)?;
    let jwk = keys
        .keys
        .iter()
        .find(|k| k.kid == kid && k.kty == "RSA")
        .ok_or(Error::UnknownKid)?;
    let key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
        .map_err(|e| Error::Rejected(e.into_kind()))?;
    let mut v = Validation::new(Algorithm::RS256);
    v.set_audience(&[aud]);
    v.set_issuer(&[issuer(team)]);
    v.set_required_spec_claims(&["exp", "iss", "aud"]);
    // Time is checked below against `now`, not the library's clock.
    v.validate_exp = false;
    let claims = decode::<Claims>(jwt, &key, &v)
        .map_err(|e| Error::Rejected(e.into_kind()))?
        .claims;
    if now > claims.exp.saturating_add(LEEWAY) {
        return Err(Error::Expired);
    }
    if claims
        .nbf
        .is_some_and(|nbf| nbf > now.saturating_add(LEEWAY))
    {
        return Err(Error::NotYetValid);
    }
    [claims.email, claims.common_name]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(|subject| Identity { subject })
        .ok_or(Error::NoSubject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};

    const KEY: &str = include_str!("../tests/fixtures/access-test-key.pem");
    const JWKS: &str = include_str!("../tests/fixtures/access-test-jwks.json");
    const NOW: u64 = 1_800_000_000;
    const AUD: &str = "aud-tag";

    fn keys() -> Jwks {
        serde_json::from_str(JWKS).unwrap()
    }

    fn claims() -> Value {
        json!({
            "aud": [AUD],
            "iss": "https://team.cloudflareaccess.com",
            "exp": NOW + 600,
            "nbf": NOW - 10,
            "iat": NOW - 10,
            "email": "me@example.com",
        })
    }

    fn mint(kid: Option<&str>, claims: &Value) -> String {
        let mut h = Header::new(Algorithm::RS256);
        h.kid = kid.map(String::from);
        let key = EncodingKey::from_rsa_pem(KEY.as_bytes()).unwrap();
        encode(&h, claims, &key).unwrap()
    }

    fn check(jwt: &str) -> Result<Identity, Error> {
        verify(jwt, &keys(), "team", AUD, NOW)
    }

    fn with(k: &str, v: Value) -> Value {
        let mut c = claims();
        c[k] = v;
        c
    }

    #[test]
    fn valid_token_yields_email() {
        let id = check(&mint(Some("test-kid"), &claims())).unwrap();
        assert_eq!(id.subject, "me@example.com");
    }

    #[test]
    fn service_token_yields_common_name() {
        let mut c = claims();
        c.as_object_mut().unwrap().remove("email");
        c["common_name"] = json!("abc.access");
        let id = check(&mint(Some("test-kid"), &c)).unwrap();
        assert_eq!(id.subject, "abc.access");
        c["common_name"] = json!("");
        let r = check(&mint(Some("test-kid"), &c));
        assert!(matches!(r, Err(Error::NoSubject)), "{r:?}");
    }

    #[test]
    fn wrong_aud() {
        let r = check(&mint(Some("test-kid"), &with("aud", json!(["other"]))));
        assert!(
            matches!(r, Err(Error::Rejected(ErrorKind::InvalidAudience))),
            "{r:?}"
        );
        let mut c = claims();
        c.as_object_mut().unwrap().remove("aud");
        let r = check(&mint(Some("test-kid"), &c));
        assert!(
            matches!(r, Err(Error::Rejected(ErrorKind::MissingRequiredClaim(_)))),
            "{r:?}"
        );
    }

    #[test]
    fn wrong_iss() {
        let iss = json!("https://evil.cloudflareaccess.com");
        let r = check(&mint(Some("test-kid"), &with("iss", iss)));
        assert!(
            matches!(r, Err(Error::Rejected(ErrorKind::InvalidIssuer))),
            "{r:?}"
        );
    }

    #[test]
    fn expired_and_not_yet_valid() {
        let r = check(&mint(Some("test-kid"), &with("exp", json!(NOW - 61))));
        assert!(matches!(r, Err(Error::Expired)), "{r:?}");
        // Within leeway still passes.
        check(&mint(Some("test-kid"), &with("exp", json!(NOW - 59)))).unwrap();
        let r = check(&mint(Some("test-kid"), &with("nbf", json!(NOW + 61))));
        assert!(matches!(r, Err(Error::NotYetValid)), "{r:?}");
        let mut c = claims();
        c.as_object_mut().unwrap().remove("exp");
        assert!(check(&mint(Some("test-kid"), &c)).is_err());
    }

    #[test]
    fn unknown_kid() {
        let r = check(&mint(Some("rotated"), &claims()));
        assert!(matches!(r, Err(Error::UnknownKid)), "{r:?}");
    }

    #[test]
    fn bad_signature() {
        // Swap in another payload under the original signature.
        let good = mint(Some("test-kid"), &claims());
        let forged = mint(Some("test-kid"), &with("email", json!("evil@example.com")));
        let p: Vec<&str> = good.split('.').collect();
        let f: Vec<&str> = forged.split('.').collect();
        let r = check(&format!("{}.{}.{}", p[0], f[1], p[2]));
        assert!(
            matches!(r, Err(Error::Rejected(ErrorKind::InvalidSignature))),
            "{r:?}"
        );
    }

    #[test]
    fn rejects_other_algorithms() {
        let mut h = Header::new(Algorithm::HS256);
        h.kid = Some("test-kid".into());
        let jwt = encode(&h, &claims(), &EncodingKey::from_secret(b"k")).unwrap();
        let r = check(&jwt);
        assert!(
            matches!(r, Err(Error::Rejected(ErrorKind::InvalidAlgorithm))),
            "{r:?}"
        );
    }

    #[test]
    fn missing_header_or_kid() {
        assert!(matches!(check(""), Err(Error::Malformed)));
        assert!(matches!(check("not.a.jwt"), Err(Error::Malformed)));
        let r = check(&mint(None, &claims()));
        assert!(matches!(r, Err(Error::Malformed)), "{r:?}");
    }
}

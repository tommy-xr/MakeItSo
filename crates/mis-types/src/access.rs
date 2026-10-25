//! Cloudflare Access service-token headers, for reaching an orchestrator
//! published behind a Cloudflare Tunnel with a service-auth policy.

/// Headers for `--cf-access-client-id` / `--cf-access-client-secret`.
/// Both or neither; Err names the missing flag.
pub fn access_headers(
    id: Option<&str>,
    secret: Option<&str>,
) -> Result<Vec<(&'static str, String)>, String> {
    match (id, secret) {
        (Some(id), Some(secret)) => Ok(vec![
            ("cf-access-client-id", id.into()),
            ("cf-access-client-secret", secret.into()),
        ]),
        (None, None) => Ok(vec![]),
        (Some(_), None) => Err(
            "--cf-access-client-id needs --cf-access-client-secret (CF_ACCESS_CLIENT_SECRET)"
                .into(),
        ),
        (None, Some(_)) => Err(
            "--cf-access-client-secret needs --cf-access-client-id (CF_ACCESS_CLIENT_ID)".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_headers_both_or_neither() {
        assert_eq!(
            access_headers(Some("abc.access"), Some("shh")).unwrap(),
            vec![
                ("cf-access-client-id", "abc.access".to_string()),
                ("cf-access-client-secret", "shh".to_string()),
            ]
        );
        assert!(access_headers(None, None).unwrap().is_empty());
        let e = access_headers(Some("abc.access"), None).unwrap_err();
        assert!(e.contains("CF_ACCESS_CLIENT_SECRET"), "{e}");
        let e = access_headers(None, Some("shh")).unwrap_err();
        assert!(e.contains("CF_ACCESS_CLIENT_ID"), "{e}");
    }
}

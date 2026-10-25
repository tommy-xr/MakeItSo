//! `worker.toml`: saved connection settings. Flags / env override the file.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const DEFAULT_URL: &str = "ws://127.0.0.1:7474";

/// One layer of settings: the file, or what flags / env supplied.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub url: Option<String>,
    pub token: Option<String>,
    pub cf_access_client_id: Option<String>,
    pub cf_access_client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PathBuf>,
}

/// Settings the worker runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub url: String,
    pub token: Option<String>,
    pub cf_access_client_id: Option<String>,
    pub cf_access_client_secret: Option<String>,
    /// Never empty.
    pub roots: Vec<PathBuf>,
}

/// `cli` beats `file` beats defaults (`DEFAULT_URL`, roots = `[cwd]`). The
/// Access id/secret pair comes whole from one layer so they never mix.
pub fn merge(cli: Config, file: Config, cwd: PathBuf) -> Resolved {
    let cli_access = cli.cf_access_client_id.is_some() || cli.cf_access_client_secret.is_some();
    let (cf_access_client_id, cf_access_client_secret) = if cli_access {
        (cli.cf_access_client_id, cli.cf_access_client_secret)
    } else {
        (file.cf_access_client_id, file.cf_access_client_secret)
    };
    let roots = [cli.roots, file.roots, vec![cwd]]
        .into_iter()
        .find(|r| !r.is_empty())
        .unwrap();
    Resolved {
        url: cli.url.or(file.url).unwrap_or_else(|| DEFAULT_URL.into()),
        token: cli.token.or(file.token),
        cf_access_client_id,
        cf_access_client_secret,
        roots,
    }
}

/// Missing file = empty config.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> Config {
        toml::from_str(
            r#"
            url = "wss://mis.example.com"
            token = "file-token"
            cf_access_client_id = "file.access"
            cf_access_client_secret = "file-secret"
            roots = ["/code"]
            "#,
        )
        .unwrap()
    }

    #[test]
    fn file_fills_unset_flags() {
        let r = merge(Config::default(), file(), "/cwd".into());
        assert_eq!(r.url, "wss://mis.example.com");
        assert_eq!(r.token.as_deref(), Some("file-token"));
        assert_eq!(r.cf_access_client_id.as_deref(), Some("file.access"));
        assert_eq!(r.roots, vec![PathBuf::from("/code")]);
    }

    #[test]
    fn flags_beat_file() {
        let cli = Config {
            url: Some("ws://flag".into()),
            token: Some("flag-token".into()),
            roots: vec!["/flag".into()],
            ..Default::default()
        };
        let r = merge(cli, file(), "/cwd".into());
        assert_eq!(r.url, "ws://flag");
        assert_eq!(r.token.as_deref(), Some("flag-token"));
        assert_eq!(r.roots, vec![PathBuf::from("/flag")]);
    }

    #[test]
    fn access_pair_never_mixes_layers() {
        let cli = Config {
            cf_access_client_id: Some("flag.access".into()),
            ..Default::default()
        };
        let r = merge(cli, file(), "/cwd".into());
        assert_eq!(r.cf_access_client_id.as_deref(), Some("flag.access"));
        assert_eq!(r.cf_access_client_secret, None);
    }

    #[test]
    fn defaults_when_both_empty() {
        let r = merge(Config::default(), Config::default(), "/cwd".into());
        assert_eq!(r.url, DEFAULT_URL);
        assert_eq!(r.token, None);
        assert_eq!(r.roots, vec![PathBuf::from("/cwd")]);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("root = [\"/x\"]").is_err());
    }
}

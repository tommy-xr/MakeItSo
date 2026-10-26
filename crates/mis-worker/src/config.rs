//! `worker.toml`: saved connection settings. Flags / env override the file.

use std::collections::BTreeMap;
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
    /// `[models] claude = [...]`: replaces that harness's discovered models.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, Vec<String>>,
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
    pub models: BTreeMap<String, Vec<String>>,
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
    let mut models = file.models;
    models.extend(cli.models);
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
        models,
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

/// Save `cfg`, readable only by this user on unix since it holds secrets.
pub fn write(path: &Path, cfg: &Config) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string(cfg)?; // before truncating, so a failure keeps the old file
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    // `mode` only applies on create; tighten a file that already existed.
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    std::io::Write::write_all(&mut f, body.as_bytes())?;
    Ok(())
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

            [models]
            codex = ["gpt-a"]
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
        assert_eq!(r.models["codex"], ["gpt-a"]);
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
    fn roundtrips_and_omits_unset() {
        let cfg = Config {
            url: Some("ws://x".into()),
            ..Default::default()
        };
        let s = toml::to_string(&cfg).unwrap();
        assert_eq!(s.trim(), r#"url = "ws://x""#);
        assert_eq!(toml::from_str::<Config>(&s).unwrap(), cfg);
        assert_eq!(
            toml::from_str::<Config>(&toml::to_string(&file()).unwrap()).unwrap(),
            file()
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("root = [\"/x\"]").is_err());
    }
}

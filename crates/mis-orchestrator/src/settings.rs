//! Settings: `settings.json` in the state dir, beside the ledger, edited from the Settings page.
//! `clean` is pure; `load` and `save` read and write the file.

use std::path::Path;

use mis_types::Settings;

pub const FILE: &str = "settings.json";
/// Longest value: a path, with room.
const MAX_CHARS: usize = 512;

/// `s` with its values trimmed; Err: why one can't be saved.
pub fn clean(mut s: Settings) -> Result<Settings, String> {
    let a = &mut s.apps;
    for (name, v) in [
        ("apps harness", &mut a.harness),
        ("apps model", &mut a.model),
        ("apps repo", &mut a.repo),
    ] {
        *v = v.trim().to_string();
        if v.is_empty() || v.chars().count() > MAX_CHARS || v.contains(char::is_control) {
            return Err(format!("{name}: one line of 1-{MAX_CHARS} characters"));
        }
    }
    Ok(s)
}

/// What `dir` holds; the defaults until something is saved.
pub fn load(dir: &Path) -> anyhow::Result<Settings> {
    let path = dir.join(FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e.into()),
    }
}

/// Replaces `dir`'s settings with `s`, atomically.
pub fn save(dir: &Path, s: &Settings) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(s).expect("settings serialize");
    crate::push::write_private(&dir.join(FILE), &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_what_a_file_leaves_out() {
        let s: Settings = serde_json::from_str(r#"{"apps": {"model": "sonnet"}}"#).unwrap();
        assert_eq!(s.apps.model, "sonnet");
        assert_eq!(
            (s.apps.harness.as_str(), s.apps.repo.as_str()),
            ("claude", "~/code/mis-notes")
        );
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(s.apps.model, "opus");
    }

    #[test]
    fn clean_trims_and_refuses_blank_or_multiline_values() {
        let mut s = Settings::default();
        s.apps.model = "  sonnet ".into();
        assert_eq!(clean(s.clone()).unwrap().apps.model, "sonnet");
        for bad in ["", "  ", "a\nb", &"x".repeat(MAX_CHARS + 1)] {
            let mut t = s.clone();
            t.apps.repo = bad.into();
            assert!(clean(t).unwrap_err().starts_with("apps repo"), "{bad:?}");
        }
    }

    #[test]
    fn saved_settings_load_back() {
        let dir = std::env::temp_dir().join(format!("mis-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir).unwrap(), Settings::default());
        let mut s = Settings::default();
        s.apps.repo = "/srv/notes".into();
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir).unwrap(), s);
        std::fs::write(dir.join(FILE), "not json").unwrap();
        assert!(load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

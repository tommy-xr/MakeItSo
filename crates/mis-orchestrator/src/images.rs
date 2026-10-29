//! Uploaded images, kept as `<dir>/<uuid>.<ext>` until attached to a task,
//! served back for thumbnails, and swept once a week old. The checks are pure;
//! `Images` is the filesystem shell around them.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use base64ct::{Base64, Encoding};
use mis_types::{ImageFile, ImageKind, MAX_IMAGES};
use uuid::Uuid;

/// Older images are deleted, attached or not.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// The upload's format: `content_type` must name an accepted one and `bytes`
/// must start with its magic number.
pub fn validate(content_type: Option<&str>, bytes: &[u8]) -> Result<ImageKind, String> {
    let kind = content_type
        .and_then(ImageKind::from_mime)
        .ok_or("Content-Type must be image/jpeg, image/png, image/webp or image/gif")?;
    match ImageKind::sniff(bytes) {
        Some(k) if k == kind => Ok(kind),
        _ => Err(format!("body is not {}", kind.mime())),
    }
}

/// Images for one task: at most `MAX_IMAGES`, each one `known`.
pub fn check(ids: &[Uuid], known: impl Fn(Uuid) -> bool) -> Result<(), String> {
    if ids.len() > MAX_IMAGES {
        return Err(format!("at most {MAX_IMAGES} images per task"));
    }
    match ids.iter().find(|id| !known(**id)) {
        Some(id) => Err(format!("unknown image {id}; upload it first")),
        None => Ok(()),
    }
}

/// Files last modified more than `max_age` before `now`.
pub fn expired(
    files: Vec<(PathBuf, SystemTime)>,
    now: SystemTime,
    max_age: Duration,
) -> Vec<PathBuf> {
    files
        .into_iter()
        .filter(|(_, mtime)| now.duration_since(*mtime).is_ok_and(|age| age > max_age))
        .map(|(path, _)| path)
        .collect()
}

pub struct Images {
    dir: PathBuf,
}

impl Images {
    /// Use `dir`, creating it readable only by this user.
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn store(&self, kind: ImageKind, bytes: &[u8]) -> std::io::Result<Uuid> {
        let id = Uuid::new_v4();
        crate::push::write_private(&self.path(id, kind), bytes)?;
        Ok(id)
    }

    fn path(&self, id: Uuid, kind: ImageKind) -> PathBuf {
        self.dir.join(format!("{id}.{}", kind.ext()))
    }

    /// Stored kind of image `id`, if it is still here.
    pub fn find(&self, id: Uuid) -> Option<ImageKind> {
        ImageKind::ALL
            .into_iter()
            .find(|k| self.path(id, *k).is_file())
    }

    pub fn read(&self, id: Uuid) -> Option<(ImageKind, Vec<u8>)> {
        let kind = self.find(id)?;
        Some((kind, std::fs::read(self.path(id, kind)).ok()?))
    }

    /// `ids` with their bytes, for a task's assignment. One gone since it was
    /// attached (swept) is left out with a warning.
    pub fn files(&self, ids: &[Uuid]) -> Vec<ImageFile> {
        ids.iter()
            .filter_map(|id| {
                let Some((_, bytes)) = self.read(*id) else {
                    tracing::warn!(%id, "attached image is gone; sending the task without it");
                    return None;
                };
                Some(ImageFile {
                    id: *id,
                    data: Base64::encode_string(&bytes),
                })
            })
            .collect()
    }

    /// Delete expired files; how many went.
    pub fn sweep(&self, now: SystemTime) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let files = entries
            .flatten()
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
            .collect();
        expired(files, now, MAX_AGE)
            .into_iter()
            .filter(|p| std::fs::remove_file(p).is_ok())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0 rest";

    #[test]
    fn upload_needs_a_named_format_its_bytes_match() {
        assert_eq!(validate(Some("image/jpeg"), JPEG), Ok(ImageKind::Jpeg));
        assert!(validate(Some("image/png"), JPEG).is_err(), "header lies");
        assert!(validate(None, JPEG).is_err());
        assert!(validate(Some("image/svg+xml"), b"<svg").is_err());
        assert!(validate(Some("image/jpeg"), b"<html>").is_err());
    }

    #[test]
    fn task_images_are_capped_and_must_exist() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        assert_eq!(check(&ids[..4], |_| true), Ok(()));
        assert_eq!(check(&[], |_| false), Ok(()));
        assert!(check(&ids, |_| true).unwrap_err().contains("at most 4"));
        let unknown = ids[2];
        let err = check(&ids[..4], |id| id != unknown).unwrap_err();
        assert!(err.contains(&unknown.to_string()), "{err}");
    }

    #[test]
    fn only_files_past_the_max_age_expire() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 24 * 3600);
        let day = Duration::from_secs(24 * 3600);
        let files = vec![
            ("old".into(), now - 8 * day),
            ("edge".into(), now - MAX_AGE),
            ("new".into(), now - day),
            ("future".into(), now + day),
        ];
        assert_eq!(expired(files, now, MAX_AGE), [PathBuf::from("old")]);
    }

    #[test]
    fn stores_finds_and_sweeps() {
        let dir = std::env::temp_dir().join(format!("mis-images-test-{}", Uuid::new_v4()));
        let images = Images::open(dir.clone()).unwrap();
        let id = images.store(ImageKind::Jpeg, JPEG).unwrap();
        assert_eq!(images.read(id), Some((ImageKind::Jpeg, JPEG.to_vec())));
        assert_eq!(images.find(Uuid::new_v4()), None);
        let files = images.files(&[id, Uuid::new_v4()]);
        assert_eq!(files.len(), 1, "a missing image is left out");
        assert_eq!(Base64::decode_vec(&files[0].data).unwrap(), JPEG);
        assert_eq!(images.sweep(SystemTime::now()), 0);
        assert_eq!(images.sweep(SystemTime::now() + MAX_AGE * 2), 1);
        assert_eq!(images.find(id), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

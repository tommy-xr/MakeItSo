//! Uploads (images and documents), kept as `<dir>/<uuid>.<ext>` (and `<uuid>.name`, the name
//! it came with) until attached to a task, served back for thumbnails, and swept once a week
//! old. The checks are pure; `Images` is the filesystem shell around them.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use base64ct::{Base64, Encoding};
use mis_types::{
    AttachedFile, DocKind, ImageFile, ImageKind, UploadKind, MAX_ATTACHED, MAX_ATTACHED_BYTES,
    MAX_NAME_BYTES,
};
use uuid::Uuid;

/// Older images are deleted, attached or not.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// The upload's format: an image `content_type` names, its bytes starting with that one's
/// magic number; else a document `name`'s extension names, its bytes fitting it.
pub fn validate(
    content_type: Option<&str>,
    name: Option<&str>,
    bytes: &[u8],
) -> Result<UploadKind, String> {
    if let Some(kind) = content_type.and_then(ImageKind::from_mime) {
        return match ImageKind::sniff(bytes) {
            Some(k) if k == kind => Ok(UploadKind::Image(kind)),
            _ => Err(format!("body is not {}", kind.mime())),
        };
    }
    let Some(doc) = name.and_then(DocKind::from_name) else {
        return Err(
            "images are jpeg, png, webp or gif (by Content-Type); documents are \
                    .pdf, .txt, .md, .csv or .json (by ?name=)"
                .into(),
        );
    };
    match doc.fits(bytes) {
        true => Ok(UploadKind::Doc(doc)),
        false if doc == DocKind::Pdf => Err("body is not a PDF".into()),
        false => Err(format!("a .{} file is UTF-8 text", doc.ext())),
    }
}

/// `name` as kept for a `kind` upload: its last path segment, control characters dropped, at
/// most `MAX_NAME_BYTES`, ending in the kind's extension. Blank: none.
pub fn clean_name(name: &str, kind: UploadKind) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let base: String = base.chars().filter(|c| !c.is_control()).collect();
    let base = base.trim().trim_start_matches('.');
    if base.is_empty() {
        return None;
    }
    let ext = format!(".{}", kind.ext());
    let has_ext = base.len() > ext.len() && base.to_ascii_lowercase().ends_with(&ext);
    let stem = match has_ext {
        true => &base[..base.len() - ext.len()],
        false => base,
    };
    let room = MAX_NAME_BYTES - ext.len();
    let cut = (stem.char_indices().map(|(i, c)| i + c.len_utf8()))
        .take_while(|end| *end <= room)
        .last()
        .unwrap_or(0);
    Some(stem[..cut].to_string() + &ext)
}

/// Uploads for one task: at most `MAX_ATTACHED` of `MAX_ATTACHED_BYTES` in all, each one
/// stored (`size`: its bytes, None if not).
pub fn check(ids: &[Uuid], size: impl Fn(Uuid) -> Option<u64>) -> Result<(), String> {
    if ids.len() > MAX_ATTACHED {
        return Err(format!("at most {MAX_ATTACHED} images and files per task"));
    }
    let mut total = 0;
    for id in ids {
        total += size(*id).ok_or_else(|| format!("unknown upload {id}; upload it first"))?;
    }
    if total > MAX_ATTACHED_BYTES as u64 {
        let mb = MAX_ATTACHED_BYTES >> 20;
        return Err(format!("at most {mb} MB of images and files per task"));
    }
    Ok(())
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

/// Create `dir`, readable only by this user.
pub(crate) fn private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Delete the files in `dir` older than `max_age`; how many went.
pub(crate) fn sweep_dir(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let files = entries
        .flatten()
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    expired(files, now, max_age)
        .into_iter()
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

pub struct Images {
    dir: PathBuf,
}

impl Images {
    /// Use `dir`, creating it readable only by this user.
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        private_dir(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Keep `bytes` as a `kind` upload, named `name` (see [`clean_name`]) if given.
    pub fn store(
        &self,
        kind: UploadKind,
        bytes: &[u8],
        name: Option<&str>,
    ) -> std::io::Result<Uuid> {
        let id = Uuid::new_v4();
        if let Some(name) = name.and_then(|n| clean_name(n, kind)) {
            crate::push::write_private(&self.dir.join(format!("{id}.name")), name.as_bytes())?;
        }
        crate::push::write_private(&self.path(id, kind), bytes)?;
        Ok(id)
    }

    fn path(&self, id: Uuid, kind: UploadKind) -> PathBuf {
        self.dir.join(format!("{id}.{}", kind.ext()))
    }

    /// Stored kind of upload `id`, if it is still here.
    pub fn find(&self, id: Uuid) -> Option<UploadKind> {
        UploadKind::all().find(|k| self.path(id, *k).is_file())
    }

    /// Upload `id`'s bytes on disk, if it is still here.
    pub fn size(&self, id: Uuid) -> Option<u64> {
        let kind = self.find(id)?;
        Some(std::fs::metadata(self.path(id, kind)).ok()?.len())
    }

    /// The name upload `id` came with, else `<id>.<ext>`.
    pub fn name(&self, id: Uuid, kind: UploadKind) -> String {
        let named = std::fs::read_to_string(self.dir.join(format!("{id}.name")));
        named.unwrap_or_else(|_| format!("{id}.{}", kind.ext()))
    }

    pub fn read(&self, id: Uuid) -> Option<(UploadKind, Vec<u8>)> {
        let kind = self.find(id)?;
        Some((kind, std::fs::read(self.path(id, kind)).ok()?))
    }

    /// `ids` with their bytes, for a task's assignment: its images, then its documents. One gone
    /// since it was attached (swept) is left out with a warning.
    pub fn files(&self, ids: &[Uuid]) -> (Vec<ImageFile>, Vec<AttachedFile>) {
        let (mut images, mut docs) = (vec![], vec![]);
        for id in ids {
            let Some((kind, bytes)) = self.read(*id) else {
                tracing::warn!(%id, "attached upload is gone; sending the task without it");
                continue;
            };
            let data = Base64::encode_string(&bytes);
            match kind {
                UploadKind::Image(_) => images.push(ImageFile { id: *id, data }),
                UploadKind::Doc(_) => docs.push(AttachedFile {
                    id: *id,
                    name: self.name(*id, kind),
                    data,
                }),
            }
        }
        (images, docs)
    }

    /// Delete expired files; how many went.
    pub fn sweep(&self, now: SystemTime) -> usize {
        sweep_dir(&self.dir, now, MAX_AGE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0 rest";

    #[test]
    fn upload_needs_a_named_format_its_bytes_match() {
        let jpeg = UploadKind::Image(ImageKind::Jpeg);
        assert_eq!(validate(Some("image/jpeg"), None, JPEG), Ok(jpeg));
        assert!(
            validate(Some("image/png"), None, JPEG).is_err(),
            "header lies"
        );
        assert!(validate(None, None, JPEG).is_err());
        assert!(validate(Some("image/svg+xml"), None, b"<svg").is_err());
        assert!(validate(Some("image/jpeg"), None, b"<html>").is_err());
        // A document goes by its name, whatever the browser called it.
        let pdf = validate(
            Some("application/octet-stream"),
            Some("Chores.pdf"),
            b"%PDF-1.4",
        );
        assert_eq!(pdf, Ok(UploadKind::Doc(DocKind::Pdf)));
        assert_eq!(
            validate(None, Some("rules.md"), b"# Rules"),
            Ok(UploadKind::Doc(DocKind::Markdown))
        );
        assert!(validate(None, Some("x.pdf"), b"<html>").is_err());
        assert!(validate(None, Some("x.md"), b"\xFF\xD8\xFF").is_err());
        assert!(validate(Some("text/html"), Some("page.html"), b"<p>").is_err());
        // An image's Content-Type wins over a document name.
        assert!(validate(Some("image/png"), Some("x.txt"), b"hello").is_err());
    }

    #[test]
    fn names_keep_their_last_segment_and_end_in_their_kind() {
        let pdf = UploadKind::Doc(DocKind::Pdf);
        assert_eq!(
            clean_name("Chore list.pdf", pdf).as_deref(),
            Some("Chore list.pdf")
        );
        assert_eq!(clean_name("C:\\x\\../a.PDF", pdf).as_deref(), Some("a.pdf"));
        assert_eq!(
            clean_name("../../etc/passwd", pdf).as_deref(),
            Some("passwd.pdf")
        );
        assert_eq!(clean_name("..pdf", pdf).as_deref(), Some("pdf.pdf"));
        assert_eq!(clean_name("a\nb\u{0}.pdf", pdf).as_deref(), Some("ab.pdf"));
        assert_eq!(clean_name(" / ", pdf), None);
        let long = clean_name(&"界".repeat(300), pdf).unwrap();
        assert!(
            long.len() <= MAX_NAME_BYTES && long.len() > MAX_NAME_BYTES - 3,
            "{long}"
        );
        assert!(long.ends_with(".pdf"));
    }

    #[test]
    fn task_uploads_are_capped_in_number_and_bytes_and_must_exist() {
        let ids: Vec<Uuid> = (0..=MAX_ATTACHED).map(|_| Uuid::new_v4()).collect();
        let small = |_| Some(1);
        assert_eq!(check(&ids[..MAX_ATTACHED], small), Ok(()));
        assert_eq!(check(&[], |_| None), Ok(()));
        assert!(check(&ids, small).unwrap_err().contains("at most 12"));
        let unknown = ids[2];
        let err = check(&ids[..4], |id| (id != unknown).then_some(1)).unwrap_err();
        assert!(err.contains(&unknown.to_string()), "{err}");
        let half = (MAX_ATTACHED_BYTES / 2) as u64;
        assert_eq!(check(&ids[..2], |_| Some(half)), Ok(()));
        assert!(check(&ids[..3], |_| Some(half)).unwrap_err().contains("MB"));
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
        let jpeg = UploadKind::Image(ImageKind::Jpeg);
        let id = images.store(jpeg, JPEG, None).unwrap();
        let pdf = UploadKind::Doc(DocKind::Pdf);
        let doc = images
            .store(pdf, b"%PDF-1.4", Some("dir/Chores.pdf"))
            .unwrap();
        assert_eq!(images.read(id), Some((jpeg, JPEG.to_vec())));
        assert_eq!((images.find(doc), images.size(doc)), (Some(pdf), Some(8)));
        assert_eq!(images.name(doc, pdf), "Chores.pdf");
        assert_eq!(images.name(id, jpeg), format!("{id}.jpg"));
        assert_eq!(images.find(Uuid::new_v4()), None);
        let (imgs, docs) = images.files(&[doc, id, Uuid::new_v4()]);
        assert_eq!(
            (imgs.len(), docs.len()),
            (1, 1),
            "a missing upload is left out"
        );
        assert_eq!(Base64::decode_vec(&imgs[0].data).unwrap(), JPEG);
        assert_eq!((docs[0].id, docs[0].name.as_str()), (doc, "Chores.pdf"));
        assert_eq!(images.sweep(SystemTime::now()), 0);
        assert_eq!(
            images.sweep(SystemTime::now() + MAX_AGE * 2),
            3,
            "the name goes too"
        );
        assert_eq!(images.find(id), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}

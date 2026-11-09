//! App drafts (pure): what `mis apps propose` may propose, and the PR a draft names.
//! Storing them is [`crate::apps::Apps`]'s; publishing (merge, then activate) is the shell's.

use base64ct::{Base64, Encoding};
use mis_types::{ImageKind, ProposeDraft, MAX_DRAFT_IMAGE_BYTES, MAX_DRAFT_NOTE_CHARS};

/// `https://github.com/<owner>/<repo>/pull/<n>` as (`owner/repo`, n). Err: not one.
pub fn parse_pr(url: &str) -> Result<(String, u64), String> {
    let bad =
        || format!("{url:?} isn't a GitHub PR link (https://github.com/<owner>/<repo>/pull/<n>)");
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")
        .ok_or_else(bad)?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let (owner, repo, pull, n) = (parts.next(), parts.next(), parts.next(), parts.next());
    let name_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    match (owner, repo, pull, n, parts.next()) {
        (Some(o), Some(r), Some("pull"), Some(n), None) if name_ok(o) && name_ok(r) => {
            let n: u64 = n.parse().ok().filter(|n| *n > 0).ok_or_else(bad)?;
            Ok((format!("{o}/{r}"), n))
        }
        _ => Err(bad()),
    }
}

/// `p`, checked against the app's `releases` and live `active` one: a staged release, a PR
/// link, a 40-hex head, a short note. Err: why not.
pub fn check(p: &ProposeDraft, releases: &[u32], active: Option<u32>) -> Result<(), String> {
    if !releases.contains(&p.release) {
        return Err(format!("no release {}: publish it first", p.release));
    }
    if active == Some(p.release) {
        return Err(format!("release {} is live already", p.release));
    }
    parse_pr(&p.pr)?;
    if let Some(h) = &p.head {
        if h.len() != 40 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("head {h:?} isn't a full commit sha"));
        }
    }
    if p.note
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_DRAFT_NOTE_CHARS)
    {
        return Err(format!(
            "the note is over {MAX_DRAFT_NOTE_CHARS} characters"
        ));
    }
    Ok(())
}

/// A screenshot's base64 as bytes: PNG, JPEG or GIF, at most [`MAX_DRAFT_IMAGE_BYTES`].
pub fn image(what: &str, b64: &str) -> Result<Vec<u8>, String> {
    // Base64 of the cap, so an oversized one isn't decoded at all.
    if b64.len() > MAX_DRAFT_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(format!(
            "{what} is over {} MiB",
            MAX_DRAFT_IMAGE_BYTES >> 20
        ));
    }
    let bytes = Base64::decode_vec(b64.trim()).map_err(|e| format!("{what}: {e}"))?;
    match ImageKind::sniff(&bytes) {
        Some(ImageKind::Png | ImageKind::Jpeg | ImageKind::Gif) => Ok(bytes),
        _ => Err(format!("{what} isn't a PNG, JPEG or GIF")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn propose(release: u32, pr: &str) -> ProposeDraft {
        ProposeDraft {
            release,
            pr: pr.into(),
            head: None,
            note: None,
            before: None,
            after: None,
            task: None,
        }
    }

    const PR: &str = "https://github.com/tommy-xr/mis-notes/pull/7";

    #[test]
    fn pr_links_are_github_pulls() {
        assert_eq!(parse_pr(PR), Ok(("tommy-xr/mis-notes".into(), 7)));
        assert_eq!(
            parse_pr(&format!("{PR}/")),
            Ok(("tommy-xr/mis-notes".into(), 7))
        );
        for bad in [
            "http://github.com/o/r/pull/7",
            "https://github.com/o/r/issues/7",
            "https://github.com/o/r/pull/0",
            "https://github.com/o/r/pull/7/files",
            "https://github.com/o/r;x/pull/7",
            "https://gitlab.com/o/r/pull/7",
        ] {
            assert!(parse_pr(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_draft_is_a_staged_release_with_a_pr() {
        assert_eq!(check(&propose(2, PR), &[1, 2], Some(1)), Ok(()));
        assert!(check(&propose(3, PR), &[1, 2], Some(1))
            .unwrap_err()
            .contains("publish it"));
        assert!(check(&propose(1, PR), &[1, 2], Some(1))
            .unwrap_err()
            .contains("live"));
        assert!(check(&propose(2, "x"), &[1, 2], None).is_err());
        let mut p = propose(2, PR);
        p.head = Some("abc".into());
        assert!(check(&p, &[2], None).is_err());
        p.head = Some("a".repeat(40));
        assert_eq!(check(&p, &[2], None), Ok(()));
        p.note = Some("x".repeat(MAX_DRAFT_NOTE_CHARS + 1));
        assert!(check(&p, &[2], None).is_err());
    }

    #[test]
    fn screenshots_are_capped_png_jpeg_or_gif() {
        let b64 = |b: &[u8]| Base64::encode_string(b);
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0];
        assert_eq!(image("before", &b64(&png)).unwrap(), png);
        assert!(image("before", &b64(b"GIF89a....")).is_ok());
        assert!(image("before", &b64(b"RIFF\0\0\0\0WEBPxx"))
            .unwrap_err()
            .contains("PNG"));
        assert!(image("before", &b64(b"<svg/>")).is_err());
        assert!(image("before", "not base64!").is_err());
        let big = vec![0u8; MAX_DRAFT_IMAGE_BYTES + 1];
        assert!(image("after", &b64(&big)).unwrap_err().contains("MiB"));
    }
}

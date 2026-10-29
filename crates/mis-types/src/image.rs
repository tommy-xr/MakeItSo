//! Image attachments: the formats accepted, told apart by their magic bytes.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Most images one task may carry.
pub const MAX_IMAGES: usize = 4;
/// Largest image accepted for upload, in bytes.
pub const MAX_IMAGE_BYTES: usize = 10 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Webp,
    Gif,
}

impl ImageKind {
    pub const ALL: [Self; 4] = [Self::Jpeg, Self::Png, Self::Webp, Self::Gif];

    /// The format `bytes` are in, by magic number; None if not one of ours.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0xFF, 0xD8, 0xFF, ..] => Some(Self::Jpeg),
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some(Self::Png),
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(Self::Webp),
            [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some(Self::Gif),
            _ => None,
        }
    }

    /// From a `Content-Type`, ignoring case and parameters: `image/JPEG; q=1`.
    pub fn from_mime(content_type: &str) -> Option<Self> {
        let mime = content_type.split(';').next()?.trim();
        Self::ALL
            .into_iter()
            .find(|k| k.mime().eq_ignore_ascii_case(mime))
    }

    pub fn from_ext(ext: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.ext() == ext)
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }
}

/// An attached image's bytes, sent with its task's assignment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageFile {
    pub id: Uuid,
    /// Standard base64, padded.
    pub data: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_magic_bytes_not_names() {
        let webp = b"RIFF\x10\0\0\0WEBPVP8 ";
        for (bytes, kind) in [
            (&b"\xFF\xD8\xFF\xE0\0\x10JFIF"[..], ImageKind::Jpeg),
            (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", ImageKind::Png),
            (webp, ImageKind::Webp),
            (b"GIF89a\x01\0", ImageKind::Gif),
            (b"GIF87a\x01\0", ImageKind::Gif),
        ] {
            assert_eq!(ImageKind::sniff(bytes), Some(kind), "{kind:?}");
        }
        for bytes in [
            &b""[..],
            b"\xFF\xD8",
            b"<svg xmlns=",
            b"RIFF\x10\0\0\0WAVEfmt ",
            b"GIF90a",
            b"<html>\x89PNG",
        ] {
            assert_eq!(ImageKind::sniff(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn mime_and_ext_round_trip() {
        for k in ImageKind::ALL {
            assert_eq!(ImageKind::from_mime(k.mime()), Some(k));
            assert_eq!(ImageKind::from_ext(k.ext()), Some(k));
        }
        assert_eq!(
            ImageKind::from_mime("IMAGE/JPEG; charset=x"),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(ImageKind::from_mime("image/svg+xml"), None);
        assert_eq!(ImageKind::from_mime("text/html"), None);
        assert_eq!(ImageKind::from_ext("jpeg"), None);
    }
}

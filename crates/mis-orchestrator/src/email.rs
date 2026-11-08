//! A raw email (RFC 5322) as a [`Capture`]: subject as the note, body as the shared
//! text, image parts as images. Pure; the HTTP handler feeds it the request body.

use mail_parser::{MessageParser, PartKind};
use mis_types::{ImageKind, MAX_IMAGES, MAX_IMAGE_BYTES};

use crate::capture::{Capture, Refused, MAX_TEXT_BYTES};

/// Read `raw` as a capture `via` email. The subject is the note (`#project` works); the
/// text body (else the HTML body as text) is the shared text, with `\n` line ends, cut
/// to [`MAX_TEXT_BYTES`]; the HTML body's first http(s) `href` is the url, as its links'
/// text rarely shows them. The first [`MAX_IMAGES`] JPEG, PNG, WebP or GIF image parts,
/// attached or inline, are the images; other image parts count as `skipped`, and the
/// rest of the parts are ignored.
pub fn capture(raw: &[u8]) -> Result<Capture, Refused> {
    let msg = MessageParser::new()
        .parse(raw)
        .ok_or_else(|| Refused::Invalid("not an email".into()))?;
    let text = msg.subject().map(Into::into);
    let body = (msg.text_bodies().collect::<Vec<_>>().join("\n")).replace("\r\n", "\n");
    let url = (msg.html_body())
        .filter(|p| matches!(p.kind(), PartKind::Html))
        .find_map(|p| first_href(&p.text()?));
    let parts: Vec<_> = (msg.root().parts())
        .filter(|p| matches!(p.kind(), PartKind::Binary | PartKind::InlineBinary))
        .filter(|p| {
            p.content_type()
                .is_some_and(|t| t.ctype().eq_ignore_ascii_case("image"))
        })
        .collect();
    // Chosen before `Capture::new`, so a big HEIC or 5th photo is skipped, not refused.
    let images: Vec<_> = (parts.iter())
        .map(|p| p.decoded().into_owned())
        .filter(|b| ImageKind::sniff(b).is_some())
        .take(MAX_IMAGES)
        .collect();
    let mut c = Capture::new(url, text, Some(cut(body)), images, Some("email".into()))?;
    c.skipped = parts.len() - c.images.len();
    Ok(c)
}

/// `s` cut to at most [`MAX_TEXT_BYTES`], at a char boundary.
fn cut(mut s: String) -> String {
    if s.len() > MAX_TEXT_BYTES {
        let end = (0..=MAX_TEXT_BYTES).rev().find(|&i| s.is_char_boundary(i));
        s.truncate(end.unwrap_or(0));
    }
    s
}

/// The first `href` attribute that is an http(s) URL: `<a HREF = "https://x.com/?a=1&amp;b">`
/// -> `https://x.com/?a=1&b`. Not `data-href`; only `&amp;` is decoded.
fn first_href(html: &str) -> Option<String> {
    // ASCII lowercasing keeps byte offsets, so matches index `html` too.
    let lower = html.to_ascii_lowercase();
    lower.match_indices("href").find_map(|(i, _)| {
        if !html[..i].ends_with(|c: char| c.is_ascii_whitespace()) {
            return None;
        }
        let rest = html[i + 4..].trim_start().strip_prefix('=')?.trim_start();
        let value = match rest.chars().next()? {
            q @ ('"' | '\'') => rest[1..].split(q).next()?,
            _ => rest
                .split(|c: char| c.is_ascii_whitespace() || c == '>')
                .next()?,
        };
        let value = value.trim().replace("&amp;", "&");
        crate::capture::web_url(&value).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_is_the_note_and_body_the_shared_text() {
        let raw = b"From: a@example.com\r\nSubject: =?UTF-8?B?8J+TjCBuZWF0ICNtaXM=?=\r\n\r\n\
                    look\r\nhttps://example.com/a\r\n";
        let c = capture(raw).unwrap();
        assert_eq!(c.text.as_deref(), Some("\u{1F4CC} neat #mis"));
        assert_eq!(c.url.as_deref(), Some("https://example.com/a"));
        assert_eq!(c.shared.as_deref(), Some("look\nhttps://example.com/a"));
        assert_eq!(c.via, "email");
    }

    #[test]
    fn html_body_gives_its_first_link_and_its_text() {
        let raw = b"Subject: \r\nContent-Type: text/html\r\n\r\n\
                    <p>Read <a href=\"mailto:x@y.z\">me</a> \
                    <A HREF='https://example.com/p?a=1&amp;b=2'>this</A></p>\r\n";
        let c = capture(raw).unwrap();
        assert_eq!(c.text, None, "blank subject");
        assert_eq!(c.url.as_deref(), Some("https://example.com/p?a=1&b=2"));
        assert_eq!(c.shared.as_deref().map(str::trim), Some("Read me this"));
        for (html, url) in [
            (
                "<a data-href=\"https://no.example\" href = \"https://x.com/a\">",
                "https://x.com/a",
            ),
            ("<a\nhref=https://x.com/b>b</a>", "https://x.com/b"),
        ] {
            assert_eq!(first_href(html).as_deref(), Some(url), "{html}");
        }
    }

    #[test]
    fn link_comes_from_the_html_alternative() {
        // The text alternative shows only the link's text.
        let raw = b"Subject: s\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n\
                    --b\r\nContent-Type: text/plain\r\n\r\nRead this\r\n\
                    --b\r\nContent-Type: text/html\r\n\r\n<a href=\"https://x.com/a\">Read this</a>\r\n\
                    --b--\r\n";
        let c = capture(raw).unwrap();
        assert_eq!(c.url.as_deref(), Some("https://x.com/a"));
        assert_eq!(c.shared.as_deref(), Some("Read this"));
    }

    #[test]
    fn keeps_image_parts_and_ignores_others() {
        let jpeg = b"\xFF\xD8\xFF\xE0 photo";
        let part = |ctype: &str, bytes: &[u8]| {
            let b64 = <base64ct::Base64 as base64ct::Encoding>::encode_string(bytes);
            format!("--b\r\nContent-Type: {ctype}\r\nContent-Transfer-Encoding: base64\r\n\r\n{b64}\r\n")
        };
        let raw = |parts: &[String]| {
            format!(
                "Subject: pics\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
                 --b\r\nContent-Type: text/plain\r\n\r\nhi\r\n{}--b--\r\n",
                parts.concat()
            )
        };
        let c = capture(
            raw(&[
                part("application/pdf", b"%PDF-1.4"),
                part("image/jpeg", jpeg),
                part("image/heic", b"\0\0\0\x18ftypheic"),
            ])
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(c.images, [(ImageKind::Jpeg, jpeg.to_vec())]);
        assert_eq!(c.skipped, 1, "heic");
        // Past the first 4, or not a kept kind, a part too big to keep is skipped, not refused.
        let mut big = jpeg.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        let mut parts = vec![part("image/heic", &[0; MAX_IMAGE_BYTES + 1])];
        parts.extend(std::iter::repeat_n(part("image/jpeg", jpeg), 4));
        parts.push(part("image/jpeg", &big));
        let c = capture(raw(&parts).as_bytes()).unwrap();
        assert_eq!((c.images.len(), c.skipped), (4, 2));
        // A kept one too big still refuses the capture.
        let r = capture(raw(&[part("image/jpeg", &big)]).as_bytes());
        assert!(matches!(r, Err(Refused::TooLarge(_))));
    }

    #[test]
    fn long_bodies_are_cut_not_refused() {
        let raw = format!("Subject: s\r\n\r\n{}\r\n", "é".repeat(MAX_TEXT_BYTES));
        let shared = capture(raw.as_bytes()).unwrap().shared.unwrap();
        assert_eq!(shared.len(), MAX_TEXT_BYTES);
        assert!(capture(b"").is_err());
    }
}

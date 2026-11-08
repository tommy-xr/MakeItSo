//! A raw email (RFC 5322) as a [`Capture`]: subject as the note, body as the shared
//! text, image parts as images. Pure; the HTTP handler feeds it the request body.

use mail_parser::{MessageParser, PartKind};

use crate::capture::{Capture, Refused, MAX_TEXT_BYTES};

/// Read `raw` as a capture `via` email. The subject is the note (`#project` works); the
/// text body (else the HTML body as text) is the shared text, with `\n` line ends, cut
/// to [`MAX_TEXT_BYTES`];
/// an HTML body's first http(s) `href` is the url, as its links' text rarely shows them.
/// Image parts, attached or inline, in order, are the images; other parts are ignored.
pub fn capture(raw: &[u8]) -> Result<Capture, Refused> {
    let msg = MessageParser::new()
        .parse(raw)
        .ok_or_else(|| Refused::Invalid("not an email".into()))?;
    let text = msg.subject().map(Into::into);
    let body = (msg.text_bodies().collect::<Vec<_>>().join("\n")).replace("\r\n", "\n");
    let url = (msg.text_body())
        .filter(|p| matches!(p.kind(), PartKind::Html))
        .find_map(|p| first_href(&p.text()?));
    let images = (msg.root().parts())
        .filter(|p| matches!(p.kind(), PartKind::Binary | PartKind::InlineBinary))
        .filter(|p| {
            p.content_type()
                .is_some_and(|t| t.ctype().eq_ignore_ascii_case("image"))
        })
        .map(|p| p.decoded().into_owned())
        .collect();
    Capture::new(url, text, Some(cut(body)), images, Some("email".into()))
}

/// `s` cut to at most [`MAX_TEXT_BYTES`], at a char boundary.
fn cut(mut s: String) -> String {
    if s.len() > MAX_TEXT_BYTES {
        let end = (0..=MAX_TEXT_BYTES).rev().find(|&i| s.is_char_boundary(i));
        s.truncate(end.unwrap_or(0));
    }
    s
}

/// The first quoted `href` that is an http(s) URL: `<a HREF="https://x.com/?a=1&amp;b">`
/// -> `https://x.com/?a=1&b`.
fn first_href(html: &str) -> Option<String> {
    // ASCII lowercasing keeps byte offsets, so matches index `html` too.
    let lower = html.to_ascii_lowercase();
    lower.match_indices("href=").find_map(|(i, _)| {
        let rest = &html[i + 5..];
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let value = rest[1..].split(quote).next()?.trim().replace("&amp;", "&");
        crate::capture::web_url(&value).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mis_types::ImageKind;

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
    }

    #[test]
    fn keeps_image_parts_and_ignores_others() {
        let jpeg = b"\xFF\xD8\xFF\xE0 photo";
        let part = |ctype: &str, bytes: &[u8]| {
            let b64 = <base64ct::Base64 as base64ct::Encoding>::encode_string(bytes);
            format!("--b\r\nContent-Type: {ctype}\r\nContent-Transfer-Encoding: base64\r\n\r\n{b64}\r\n")
        };
        let raw = format!(
            "Subject: pics\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
             --b\r\nContent-Type: text/plain\r\n\r\nhi\r\n{}{}{}--b--\r\n",
            part("application/pdf", b"%PDF-1.4"),
            part("image/jpeg", jpeg),
            part("image/heic", b"\0\0\0\x18ftypheic"),
        );
        let c = capture(raw.as_bytes()).unwrap();
        assert_eq!(c.images, [(ImageKind::Jpeg, jpeg.to_vec())]);
        assert_eq!(c.skipped, 1, "heic");
    }

    #[test]
    fn long_bodies_are_cut_not_refused() {
        let raw = format!("Subject: s\r\n\r\n{}\r\n", "é".repeat(MAX_TEXT_BYTES));
        let shared = capture(raw.as_bytes()).unwrap().shared.unwrap();
        assert_eq!(shared.len(), MAX_TEXT_BYTES);
        assert!(capture(b"").is_err());
    }
}

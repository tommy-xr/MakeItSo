"""Writes the synthetic .eml fixtures beside this file, shaped like what iOS Mail and
Outlook send (headers, nesting, encodings); no real mail. Needs Pillow and macOS `sips`
(for HEIC). Run: python3 make.py"""

import io
import subprocess
import tempfile
from email.message import EmailMessage
from email.policy import SMTP
from pathlib import Path

from PIL import Image

HERE = Path(__file__).parent


def image(color, fmt):
    buf = io.BytesIO()
    Image.new("RGB", (64, 48), color).save(buf, fmt)
    return buf.getvalue()


def heic(color):
    with tempfile.TemporaryDirectory() as d:
        src, out = Path(d, "a.png"), Path(d, "a.heic")
        src.write_bytes(image(color, "PNG"))
        subprocess.run(["sips", "-s", "format", "heic", str(src), "--out", str(out)],
                       check=True, capture_output=True)
        return out.read_bytes()


def base(n, subject, mailer="iPhone Mail (22A3354)"):
    m = EmailMessage(policy=SMTP)
    m["Received"] = "from mx.example.net by mx.m6o.app; Sun, 8 Nov 2026 12:00:00 +0000"
    m["From"] = "Demo User <demo@example.com>"
    m["To"] = "capture@m6o.app"
    m["Date"] = "Sun, 8 Nov 2026 12:00:00 +0000"
    m["Message-ID"] = f"<fixture-{n}@example.com>"
    m["X-Mailer"] = mailer
    if subject is not None:
        m["Subject"] = subject
    return m


def ios_photos(n, subject, photos):
    """iOS Mail photo share: multipart/mixed, each photo inline, signature after."""
    m = base(n, subject)
    m.set_content("", cte="7bit")
    for i, (data, sub) in enumerate(photos, 1):
        m.add_attachment(data, maintype="image", subtype=sub, filename=f"IMG_000{i}.{sub}",
                         disposition="inline")
    m.add_attachment("\n\nSent from my iPhone\n", subtype="plain", disposition="inline")
    return m


def write(name, m, boundary):
    if boundary:
        m.set_boundary(boundary)
    (HERE / name).write_bytes(m.as_bytes())


# (a) Safari page shared to Mail: subject is the page title, body the link.
a = base(1, "Liminal spaces: empty malls at night")
link = "https://example.com/articles/liminal-spaces"
a.set_content(f"{link}\n\nSent from my iPhone\n", charset="utf-8", cte="quoted-printable")
a.add_alternative(f'<html><body dir="auto"><a href="{link}">{link}</a><br><br>'
                  '<div dir="ltr">Sent from my iPhone</div></body></html>\n',
                  subtype="html", charset="utf-8", cte="quoted-printable")
write("a-ios-safari-link.eml", a, "Apple-Mail-A1")

# (b) One photo from Photos, no subject.
write("b-ios-photo.eml", ios_photos(2, None, [(image("tomato", "JPEG"), "jpeg")]),
      "Apple-Mail-B1")

# (c) Five photos: the first 4 are kept.
colors = ["navy", "teal", "olive", "maroon", "purple"]
write("c-ios-five-photos.eml",
      ios_photos(3, "Kitchen ideas #home", [(image(c, "JPEG"), "jpeg") for c in colors]),
      "Apple-Mail-C1")

# (d) Outlook: HTML only, a signature logo by cid, a PDF and an invite attached.
d = base(4, "FW: Great article #amf", mailer="Microsoft Outlook 16.0")
d.set_content(
    '<html><body><p>Worth a read: <a href="https://example.com/news/great-article?utm=mail&amp;id=7">'
    'Great article</a></p><p>Demo User<br><img src="cid:logo001"></p></body></html>\n',
    subtype="html", charset="utf-8", cte="quoted-printable")
d.add_related(image("gold", "PNG"), maintype="image", subtype="png", cid="<logo001>",
              filename="image001.png", disposition="inline")
d.set_boundary("Outlook-Related-D2")
d.make_mixed()
d.add_attachment(b"%PDF-1.4\n% synthetic\n", maintype="application", subtype="pdf",
                 filename="article.pdf")
d.add_attachment("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n", subtype="calendar",
                 filename="invite.ics")
write("d-outlook-html.eml", d, "Outlook-Mixed-D1")

# (e) UTF-8 subject with an emoji and a tag; quoted-printable body with soft breaks.
e = base(5, "\U0001F4CC Café notes #mis")
e.set_content("Notes from the café — try the cortado.\n"
              "https://example.com/places/cafe-with-a-rather-long-name-that-wraps"
              "?ref=notes&lang=en\n",
              charset="utf-8", cte="quoted-printable")
write("e-utf8-qp.eml", e, None)

# (f) A HEIC photo: not a kept image kind, so it is skipped.
write("f-ios-heic.eml", ios_photos(6, "Sunset", [(heic("orange"), "heic")]), "Apple-Mail-F1")

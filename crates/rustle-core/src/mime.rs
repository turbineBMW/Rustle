//! Reading a stored message: bodies, attachments, the headers the reader
//! shows, and the sandbox the HTML body is rendered in.

use crate::models::Attachment;
use crate::{dates, html};
use mail_parser::{MessageParser, MimeHeaders, PartType};

/// Where a mailing list says it will accept an unsubscribe request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unsubscribe {
    pub url: String,
    pub mailto: String,
    pub is_one_click: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedMessage {
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub attachments: Vec<Attachment>,
    pub subject: String,
    pub from_header: String,
    pub reply_to_header: String,
    pub from_display: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    /// The raw Date header, for quoting in a reply.
    pub date_header: String,
    /// The Date header formatted for the Details section.
    pub date: String,
    pub unsubscribe: Option<Unsubscribe>,
}

/// The most characters a conversation-list preview keeps. Two lines of a
/// narrow sidebar never show more; the rest would only bloat the database.
pub const PREVIEW_CHARS: usize = 240;

/// A one-paragraph snippet of a message body for the conversation list: the
/// plain-text part when there is one, otherwise the HTML flattened, with all
/// whitespace collapsed to single spaces.
pub fn preview(parsed: &ParsedMessage) -> String {
    let text = match (&parsed.text_body, &parsed.html_body) {
        (Some(text), _) if !text.trim().is_empty() => text.clone(),
        (_, Some(html)) => html::html_to_text(html),
        (Some(text), None) => text.clone(),
        (None, None) => String::new(),
    };
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = undo_truncated_base64(&collapsed);
    collapsed.chars().take(PREVIEW_CHARS).collect()
}

/// A base64 part cut off by a partial fetch fails to decode, and mail-parser
/// then hands back the raw encoding. Decode what is there ourselves.
fn undo_truncated_base64(text: &str) -> String {
    use base64::Engine;
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let looks_encoded = compact.len() >= 32
        && compact
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=');
    if !looks_encoded {
        return text.to_string();
    }
    let usable = compact.trim_end_matches('=');
    let usable = &usable[..usable.len() - usable.len() % 4];
    match base64::engine::general_purpose::STANDARD_NO_PAD.decode(usable) {
        Ok(bytes) => {
            let decoded = String::from_utf8_lossy(&bytes);
            let decoded = if decoded.trim_start().starts_with('<') {
                html::html_to_text(&decoded)
            } else {
                decoded.into_owned()
            };
            decoded.split_whitespace().collect::<Vec<_>>().join(" ")
        }
        Err(_) => text.to_string(),
    }
}

/// Build a preview from a header block and the first bytes of the body, as
/// fetched in one IMAP round trip. `text` may stop mid-part: mail-parser is
/// lenient about a missing closing boundary or a truncated encoding, and a
/// snippet cut short is still a snippet.
pub fn preview_from_slices(headers: &[u8], text: &[u8]) -> String {
    let mut raw = Vec::with_capacity(headers.len() + text.len() + 4);
    raw.extend_from_slice(headers);
    if !raw.ends_with(b"\r\n\r\n") && !raw.ends_with(b"\n\n") {
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(text);
    preview(&parse_message(&raw))
}

pub fn parse_message(raw: &[u8]) -> ParsedMessage {
    let Some(message) = MessageParser::default().parse(raw) else {
        return ParsedMessage::default();
    };

    let mut result = ParsedMessage {
        subject: message.subject().unwrap_or("").to_string(),
        from_header: raw_header(&message, "From"),
        reply_to_header: raw_header(&message, "Reply-To"),
        from_display: addresses(message.from()).join(", "),
        to: addresses(message.to()),
        cc: addresses(message.cc()),
        bcc: addresses(message.bcc()),
        date_header: raw_header(&message, "Date"),
        ..ParsedMessage::default()
    };
    result.date = if result.date_header.is_empty() {
        String::new()
    } else {
        dates::long_label(&result.date_header)
    };
    result.unsubscribe = unsubscribe(&message);

    for part in &message.parts {
        let is_attachment = part
            .content_disposition()
            .is_some_and(|disposition| disposition.ctype().eq_ignore_ascii_case("attachment"));
        match &part.body {
            PartType::Multipart(_) => continue, // its children are visited on their own
            PartType::Text(text) if !is_attachment && result.text_body.is_none() => {
                result.text_body = Some(text.to_string());
            }
            PartType::Html(html) if !is_attachment && result.html_body.is_none() => {
                result.html_body = Some(html.to_string());
            }
            PartType::Message(_) => continue,
            // Anything else (an inline image, an unrecognised type) is offered
            // as an attachment rather than silently dropped.
            _ => result.attachments.push(as_attachment(part)),
        }
    }

    result
}

fn raw_header(message: &mail_parser::Message, name: &str) -> String {
    message
        .header_raw(name)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn addresses(address: Option<&mail_parser::Address>) -> Vec<String> {
    let Some(address) = address else {
        return Vec::new();
    };
    address
        .iter()
        .filter_map(|addr| {
            let name = addr.name().unwrap_or("").trim();
            let email = addr.address().unwrap_or("").trim();
            match (name.is_empty(), email.is_empty()) {
                (false, false) => Some(format!("{name} <{email}>")),
                (true, false) => Some(email.to_string()),
                (false, true) => Some(name.to_string()),
                (true, true) => None,
            }
        })
        .collect()
}

fn as_attachment(part: &mail_parser::MessagePart) -> Attachment {
    let mime_type = part
        .content_type()
        .map(|ct| match ct.subtype() {
            Some(subtype) => format!("{}/{}", ct.ctype(), subtype),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Attachment {
        filename: part.attachment_name().unwrap_or("attachment").to_string(),
        mime_type,
        content: part.contents().to_vec(),
    }
}

fn unsubscribe(message: &mail_parser::Message) -> Option<Unsubscribe> {
    let header = message.header_raw("List-Unsubscribe").unwrap_or("");
    let mut url = String::new();
    let mut mailto = String::new();
    // RFC 2369 wraps each target in angle brackets and separates them with
    // commas, which may also appear inside a target -- so match the brackets.
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        let bracketed = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        // These URLs carry a long opaque token, so senders fold the header --
        // and unfolding keeps the continuation whitespace inside the URL.
        // Strip every space, not just the ends.
        let target: String = bracketed.split_whitespace().collect();
        let scheme = target.split(':').next().unwrap_or("").to_lowercase();
        // A stranger's header may name any scheme. http is honoured only as a
        // link, never as a request this app makes itself -- so an https target
        // wins even when an http one was published first.
        if (scheme == "https" || scheme == "http") && !url.to_lowercase().starts_with("https:") {
            url = target;
        } else if scheme == "mailto" && mailto.is_empty() {
            mailto = target;
        }
    }
    if url.is_empty() && mailto.is_empty() {
        return None;
    }
    let post = message
        .header_raw("List-Unsubscribe-Post")
        .unwrap_or("")
        .to_lowercase();
    let is_one_click = url.to_lowercase().starts_with("https:") && post.contains("one-click");
    Some(Unsubscribe {
        url,
        mailto,
        is_one_click,
    })
}

// WebKit's auto-load-images setting only gates <img>; a remote stylesheet,
// @import, @font-face or <iframe> loads regardless and leaks the read just the
// same. Only img-src is toggled: remote CSS is never needed to read mail.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; font-src data:; img-src data:";
const CSP_WITH_IMAGES: &str =
    "default-src 'none'; style-src 'unsafe-inline'; font-src data:; img-src data: https: http:";

/// Wrap a message body in a document whose CSP blocks remote subresources.
/// `style` is injected as the document's own stylesheet, so the reader can
/// hand the accent colour to links without touching the message's markup.
pub fn sandbox_html(html: &str, are_remote_images_allowed: bool, style: &str) -> String {
    let policy = if are_remote_images_allowed {
        CSP_WITH_IMAGES
    } else {
        CSP
    };
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\"><style>{style}</style></head><body>{html}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_prefers_text_and_collapses_whitespace() {
        let raw = b"Subject: hi\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nHello\n  there\r\n\r\nworld\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>Hello</p>\r\n--b--\r\n";
        assert_eq!(preview(&parse_message(raw)), "Hello there world");
        let html =
            b"Content-Type: text/html\r\n\r\n<style>p{}</style><p>One</p><p>Two &amp; three</p>";
        assert_eq!(preview(&parse_message(html)), "One Two & three");
    }

    #[test]
    fn preview_from_partial_fetch() {
        let headers = b"Content-Type: multipart/alternative; boundary=b\r\nContent-Transfer-Encoding: 7bit\r\n\r\n";
        // Cut off before the closing boundary, as a `<0.N>` fetch would.
        let text = b"--b\r\nContent-Type: text/plain\r\n\r\nStart of the body";
        assert_eq!(preview_from_slices(headers, text), "Start of the body");
        assert_eq!(
            preview_from_slices(b"Subject: x\r\n", b"plain body"),
            "plain body"
        );
        assert_eq!(preview_from_slices(b"", b""), "");
        // A base64 part cut mid-stream still reads as text.
        let headers = b"Content-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\n";
        assert_eq!(
            preview_from_slices(
                headers,
                b"KipCdW1waW5nIHRoaXMgdG8gdGhlIHRvcCoq\r\nIGFuZCBtb3JlIHRleH"
            ),
            "**Bumping this to the top** and more te"
        );
    }

    const RAW: &[u8] = b"From: Ada Lovelace <ada@example.com>\r\nTo: Bob <bob@example.org>, carol@example.net\r\nCc: dan@example.net\r\nDate: Wed, 16 Jul 2026 10:00:00 +0000\r\nSubject: Hello\r\nList-Unsubscribe: <mailto:leave@list.example>, <https://list.example/u?\r\n token=abc>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n--a\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--a\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n--a--\r\n--b\r\nContent-Type: application/pdf; name=\"doc.pdf\"\r\nContent-Disposition: attachment; filename=\"doc.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n--b--\r\n";

    #[test]
    fn parses_bodies_headers_and_attachments() {
        let parsed = parse_message(RAW);
        assert_eq!(parsed.subject, "Hello");
        assert_eq!(parsed.from_display, "Ada Lovelace <ada@example.com>");
        assert_eq!(parsed.from_header, "Ada Lovelace <ada@example.com>");
        assert_eq!(
            parsed.to,
            vec!["Bob <bob@example.org>", "carol@example.net"]
        );
        assert_eq!(parsed.cc, vec!["dan@example.net"]);
        assert_eq!(parsed.text_body.as_deref(), Some("plain body"));
        assert_eq!(parsed.html_body.as_deref(), Some("<p>html body</p>"));
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename, "doc.pdf");
        assert_eq!(parsed.attachments[0].mime_type, "application/pdf");
        assert_eq!(parsed.attachments[0].content, b"Hello");
        assert!(parsed.date.starts_with("Jul 16, 2026"), "{}", parsed.date);
        let unsubscribe = parsed.unsubscribe.unwrap();
        assert_eq!(unsubscribe.url, "https://list.example/u?token=abc");
        assert_eq!(unsubscribe.mailto, "mailto:leave@list.example");
        assert!(unsubscribe.is_one_click);
    }

    #[test]
    fn sandbox_blocks_remote_images_by_default() {
        let html = sandbox_html("<p>x</p>", false, "a{color:red}");
        assert!(html.contains("img-src data:\""));
        assert!(sandbox_html("", true, "").contains("img-src data: https: http:"));
        assert!(html.contains("<style>a{color:red}</style>"));
    }
}

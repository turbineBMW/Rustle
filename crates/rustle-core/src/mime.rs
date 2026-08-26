//! Reading a stored message: bodies, attachments, the headers the reader
//! shows, and the sandbox the HTML body is rendered in.

use crate::dates;
use crate::models::Attachment;
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

//! Reading a stored message: bodies, attachments, the headers the reader
//! shows, and the sandbox the HTML body is rendered in.

use crate::invite::Invitation;
use crate::models::Attachment;
use crate::{dates, html, invite};
use mail_parser::{MessageParser, MimeHeaders, PartType};

/// Where a mailing list says it will accept an unsubscribe request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unsubscribe {
    pub url: String,
    pub mailto: String,
    pub is_one_click: bool,
    /// Found as a link in the body, not in a List-Unsubscribe header: only
    /// ever opened in the browser.
    pub from_body: bool,
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
    /// The Message-ID header, angle brackets included; "" when there is none.
    pub message_id: String,
    pub unsubscribe: Option<Unsubscribe>,
    /// The meeting a calendar invite, update, cancellation or reply is about.
    pub invitation: Option<Invitation>,
    /// What the receiving server's Authentication-Results said.
    pub authentication: crate::verify::Verdict,
}

/// The most characters a email-list preview keeps. Two lines of a
/// narrow sidebar never show more; the rest would only bloat the database.
pub const PREVIEW_CHARS: usize = 240;

/// A one-paragraph snippet of a message body for the email list: the
/// plain-text part when there is one, otherwise the HTML flattened, with all
/// whitespace collapsed to single spaces.
pub fn preview(parsed: &ParsedMessage) -> String {
    body_text(parsed).chars().take(PREVIEW_CHARS).collect()
}

/// The most characters of a body kept for local search. Enough for nearly
/// any message written by a person; a newsletter's tail isn't worth the space.
pub const SEARCH_TEXT_CHARS: usize = 20_000;

/// A body's words for the search index: what the preview shows, kept longer.
pub fn search_text(parsed: &ParsedMessage) -> String {
    body_text(parsed).chars().take(SEARCH_TEXT_CHARS).collect()
}

/// The plain-text part when there is one, otherwise the HTML flattened, with
/// all whitespace collapsed to single spaces.
fn body_text(parsed: &ParsedMessage) -> String {
    let text = match (&parsed.text_body, &parsed.html_body) {
        (Some(text), _) if !text.trim().is_empty() => text.clone(),
        (_, Some(html)) => html::html_to_text(html),
        (Some(text), None) => text.clone(),
        (None, None) => String::new(),
    };
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    undo_truncated_base64(&collapsed)
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
    texts_from_slices(headers, text).0
}

/// The preview and the search text of a partial fetch, from one parse.
pub fn texts_from_slices(headers: &[u8], text: &[u8]) -> (String, String) {
    let mut raw = Vec::with_capacity(headers.len() + text.len() + 4);
    raw.extend_from_slice(headers);
    if !raw.ends_with(b"\r\n\r\n") && !raw.ends_with(b"\n\n") {
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(text);
    let parsed = parse_message(&raw);
    (preview(&parsed), search_text(&parsed))
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
        message_id: message
            .message_id()
            .map(|id| format!("<{id}>"))
            .unwrap_or_default(),
        ..ParsedMessage::default()
    };
    result.date = if result.date_header.is_empty() {
        String::new()
    } else {
        dates::long_label(&result.date_header)
    };
    result.unsubscribe = unsubscribe(&message);
    result.authentication = crate::verify::verdict(
        message
            .headers_raw()
            .filter(|(name, _)| name.eq_ignore_ascii_case("Authentication-Results"))
            .map(|(_, value)| value),
    );

    // Inline parts that the HTML references by Content-ID (`<img src="cid:…">`,
    // the shape Outlook and most rich composers produce) are folded into the
    // body as data: URIs and kept out of the attachment list.
    let mut inline: Vec<(String, Attachment)> = Vec::new();
    for part in &message.parts {
        let is_attachment = part
            .content_disposition()
            .is_some_and(|disposition| disposition.ctype().eq_ignore_ascii_case("attachment"));
        // The invite's calendar part. Exchange sends it unnamed beside the
        // body; it is the invitation card, not an attachment. A named .ics
        // stays listed as well.
        if result.invitation.is_none() && is_calendar(part) {
            if let Some(invitation) = invite::parse(part.contents()) {
                result.invitation = Some(invitation);
                if part.attachment_name().is_none() {
                    continue;
                }
            }
        }
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
            _ => match part.content_id().filter(|_| !is_attachment) {
                Some(cid) => inline.push((
                    cid.trim_matches(|c| c == '<' || c == '>').to_string(),
                    as_attachment(part),
                )),
                None => result.attachments.push(as_attachment(part)),
            },
        }
    }

    if let Some(html) = result.html_body.take() {
        let (html, unused) = embed_inline_parts(&html, inline);
        result.html_body = Some(html);
        result.attachments.extend(unused);
    } else {
        result
            .attachments
            .extend(inline.into_iter().map(|(_, a)| a));
    }

    if result.unsubscribe.is_none() {
        result.unsubscribe = result.html_body.as_deref().and_then(unsubscribe_link);
    }
    result
}

/// Replace every `cid:` reference in `html` with a data: URI of the matching
/// part. Parts nothing refers to are handed back so they can still be offered
/// as attachments.
fn embed_inline_parts(html: &str, parts: Vec<(String, Attachment)>) -> (String, Vec<Attachment>) {
    use base64::Engine;
    if parts.is_empty() || !html.contains("cid:") {
        return (
            html.to_string(),
            parts.into_iter().map(|(_, a)| a).collect(),
        );
    }
    let mut html = html.to_string();
    let mut unused = Vec::new();
    for (cid, part) in parts {
        // Attribute values and CSS url() both carry the reference; the id may
        // be percent-encoded or bare. Match the URL body up to its delimiter.
        let pattern = format!(
            r#"cid:(?:{}|{})(["'\s)>])"#,
            regex::escape(&cid),
            regex::escape(&urlencoding_lite(&cid))
        );
        let re = regex::Regex::new(&pattern).expect("escaped cid pattern is valid");
        if !re.is_match(&html) {
            unused.push(part);
            continue;
        }
        let data = format!(
            "data:{};base64,{}",
            part.mime_type,
            base64::engine::general_purpose::STANDARD.encode(&part.content)
        );
        let replacement = format!("{data}$1");
        html = re.replace_all(&html, replacement.as_str()).into_owned();
    }
    (html, unused)
}

/// Percent-encode the few characters that appear in Content-IDs and that a
/// composer might encode when writing them into a URL.
fn urlencoding_lite(cid: &str) -> String {
    cid.replace('%', "%25")
        .replace('@', "%40")
        .replace(' ', "%20")
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

fn is_calendar(part: &mail_parser::MessagePart) -> bool {
    part.content_type().is_some_and(|ct| {
        ct.ctype().eq_ignore_ascii_case("text")
            && ct
                .subtype()
                .is_some_and(|subtype| subtype.eq_ignore_ascii_case("calendar"))
    })
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
        from_body: false,
    })
}

/// A newsletter without a List-Unsubscribe header usually still has an
/// unsubscribe link in its footer: the last http(s) link whose text or
/// address says so.
pub fn unsubscribe_link(html: &str) -> Option<Unsubscribe> {
    static LINK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<a\b[^>]*?\bhref\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a>"#)
            .expect("a valid pattern")
    });
    static TAG: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]*>").expect("a valid pattern"));
    const WORDS: [&str; 4] = ["unsubscribe", "opt out", "opt-out", "email preferences"];
    LINK.captures_iter(html)
        .filter_map(|captures| {
            let url = captures[1].trim().replace("&amp;", "&");
            let lower = url.to_lowercase();
            if !lower.starts_with("https:") && !lower.starts_with("http:") {
                return None;
            }
            let text = TAG.replace_all(&captures[2], " ").to_lowercase();
            let says_so = WORDS
                .iter()
                .any(|word| text.contains(word) || lower.contains(&word.replace(' ', "")));
            says_so.then_some(url)
        })
        .last()
        .map(|url| Unsubscribe {
            url,
            from_body: true,
            ..Unsubscribe::default()
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

/// A message laid out for paper: a block of its headers, then the body.
/// Paper has no reading pane around it, so the headers the pane shows have
/// to be part of the page. Goes through `sandbox_html` like the screen copy.
pub fn print_html(parsed: &ParsedMessage) -> String {
    use crate::html::escape;
    let mut rows = String::new();
    let fields = [
        ("From", parsed.from_header.clone()),
        ("To", parsed.to.join(", ")),
        ("Cc", parsed.cc.join(", ")),
        ("Date", parsed.date.clone()),
    ];
    for (name, value) in fields {
        if value.trim().is_empty() {
            continue;
        }
        rows.push_str(&format!(
            "<tr><th>{}</th><td>{}</td></tr>",
            escape(name),
            escape(value.trim())
        ));
    }
    let body = match (&parsed.html_body, &parsed.text_body) {
        (Some(html), _) => html.clone(),
        (None, Some(text)) => format!("<pre class=\"rustle-print-text\">{}</pre>", escape(text)),
        (None, None) => String::new(),
    };
    format!(
        "<div class=\"rustle-print-header\"><h1>{}</h1><table>{rows}</table></div>{body}",
        escape(&parsed.subject)
    )
}

/// The stylesheet that goes with `print_html`.
pub const PRINT_STYLE: &str = "body { margin: 0; font-family: sans-serif; } \
    .rustle-print-header { border-bottom: 1px solid #888; margin-bottom: 1em; \
      padding-bottom: 0.5em; font-size: 10pt; } \
    .rustle-print-header h1 { font-size: 14pt; margin: 0 0 0.4em; } \
    .rustle-print-header th { text-align: right; padding-right: 0.8em; \
      vertical-align: top; color: #555; font-weight: normal; } \
    .rustle-print-text { white-space: pre-wrap; font-family: monospace; }";

/// A file name for saving a message as .eml: its subject, with whatever a
/// file system or a shell would trip on taken out.
pub fn eml_filename(subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let mut name = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let name_is_usable = !name.trim_matches('.').is_empty();
    if !name_is_usable {
        name = "message".to_string();
    }
    // Leave room for the extension inside the common 255-byte limit.
    while name.len() > 200 {
        name.pop();
    }
    format!("{name}.eml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_links_offer_an_unsubscribe() {
        let html = r#"<p>News</p><a href="https://x.example/read">Read more</a>
            <p><a href='https://x.example/u?id=1&amp;t=2'><span>Unsubscribe</span></a>
            | <a href="mailto:x@x.example">unsubscribe by mail</a></p>"#;
        let found = unsubscribe_link(html).unwrap();
        assert_eq!(found.url, "https://x.example/u?id=1&t=2");
        assert!(found.from_body && !found.is_one_click);
        assert!(unsubscribe_link("<a href=\"https://x/read\">Read</a>").is_none());
        // A header's target wins over the footer's.
        let raw = b"List-Unsubscribe: <https://list.example/u>\r\nContent-Type: text/html\r\n\r\n<a href=\"https://x/unsubscribe\">Unsubscribe</a>";
        let parsed = parse_message(raw);
        assert_eq!(parsed.unsubscribe.unwrap().url, "https://list.example/u");
    }

    #[test]
    fn reads_the_providers_verdict() {
        let raw = b"Authentication-Results: mx.google.com; dmarc=fail header.from=bank.example\r\nAuthentication-Results: evil; dmarc=pass\r\nFrom: bank@bank.example\r\n\r\nhi";
        assert_eq!(
            parse_message(raw).authentication,
            crate::verify::Verdict::Fail
        );
    }

    #[test]
    fn eml_names_are_safe_file_names() {
        assert_eq!(eml_filename("Re: Q3 / budget?"), "Re Q3 budget.eml");
        assert_eq!(eml_filename(""), "message.eml");
        assert_eq!(eml_filename(".."), "message.eml");
        assert_eq!(eml_filename("tab\there"), "tab here.eml");
        assert!(eml_filename(&"é".repeat(300)).len() <= 204);
    }

    #[test]
    fn print_layout_heads_the_body_with_escaped_headers() {
        let raw = b"From: Ada <ada@x.y>\r\nTo: bob@x.y\r\nSubject: <b>Plan</b>\r\nDate: Wed, 16 Jul 2026 10:00:00 +0000\r\n\r\nline one\r\n";
        let html = print_html(&parse_message(raw));
        assert!(html.contains("<h1>&lt;b&gt;Plan&lt;/b&gt;</h1>"), "{html}");
        assert!(
            html.contains("<th>From</th><td>Ada &lt;ada@x.y&gt;</td>"),
            "{html}"
        );
        assert!(!html.contains("<th>Cc</th>"));
        assert!(
            html.contains("<pre class=\"rustle-print-text\">line one"),
            "{html}"
        );
    }

    #[test]
    fn inline_cid_images_are_embedded_and_not_listed_as_attachments() {
        let raw = b"Subject: logo\r\nContent-Type: multipart/related; boundary=r\r\n\r\n--r\r\nContent-Type: text/html\r\n\r\n<p>hi</p><img src=\"cid:logo@x\"><img src='cid:missing@x'>\r\n--r\r\nContent-Type: image/png; name=\"logo.png\"\r\nContent-ID: <logo@x>\r\nContent-Disposition: inline; filename=\"logo.png\"\r\nContent-Transfer-Encoding: base64\r\n\r\nAQID\r\n--r\r\nContent-Type: image/png; name=\"other.png\"\r\nContent-ID: <other@x>\r\nContent-Transfer-Encoding: base64\r\n\r\nAQID\r\n--r--\r\n";
        let parsed = parse_message(raw);
        let html = parsed.html_body.unwrap();
        assert!(
            html.contains("<img src=\"data:image/png;base64,AQID\">"),
            "{html}"
        );
        assert!(html.contains("cid:missing@x"));
        let names: Vec<_> = parsed
            .attachments
            .iter()
            .map(|a| a.filename.as_str())
            .collect();
        assert_eq!(names, ["other.png"]);
    }

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

    #[test]
    fn calendar_part_becomes_the_invitation() {
        // Exchange's shape: HTML and the unnamed calendar part, no text/plain.
        let raw = b"Subject: Weekly\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>Join</p>\r\n--b\r\nContent-Type: text/calendar; charset=\"utf-8\"; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nSUMMARY:Weekly\r\nDTSTART:20261008T180000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n";
        let parsed = parse_message(raw);
        assert_eq!(parsed.invitation.unwrap().summary, "Weekly");
        assert!(parsed.attachments.is_empty());
        assert!(parsed.text_body.is_none());
        assert_eq!(parsed.html_body.as_deref(), Some("<p>Join</p>"));
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

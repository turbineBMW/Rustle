//! Building outgoing mail: reply and forward bodies, the MIME message that
//! goes on the wire, and the composer's address helpers.

use crate::address::{self, Mailbox};
use crate::html::{escape, html_to_text, to_html};
use crate::models::Attachment;
use lettre::address::Envelope;
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{
    Attachment as LettreAttachment, Mailbox as LettreMailbox, MessageBuilder, MultiPart, SinglePart,
};
use lettre::Message;
use percent_encoding::percent_decode_str;
use thiserror::Error;

pub fn reply_subject(subject: &str) -> String {
    if subject.to_lowercase().starts_with("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

pub fn forward_subject(subject: &str) -> String {
    let lower = subject.to_lowercase();
    if lower.starts_with("fwd:") || lower.starts_with("fw:") {
        subject.to_string()
    } else {
        format!("Fwd: {subject}")
    }
}

/// Reply All includes the original To and
/// Cc, minus ourselves and minus whoever the reply is already addressed to.
pub fn reply_all_cc(to_header: &str, cc_header: &str, own_email: &str, to_addr: &str) -> String {
    let excluded = [own_email.to_lowercase(), to_addr.to_lowercase()];
    let mut unique: Vec<String> = Vec::new();
    for mailbox in address::parse_list(to_header)
        .into_iter()
        .chain(address::parse_list(cc_header))
    {
        let addr = mailbox.address;
        if addr.is_empty() || excluded.contains(&addr.to_lowercase()) {
            continue;
        }
        if !unique.iter().any(|seen| seen.eq_ignore_ascii_case(&addr)) {
            unique.push(addr);
        }
    }
    unique.join(", ")
}

/// Wraps a signature (an HTML fragment, see `Account::signature_html`) in
/// the block the composer swaps per account. The "-- " delimiter is the RFC
/// 3676 convention for a signature.
pub fn signature_block(html: &str) -> String {
    format!("<div class=\"signature\">-- <br>{html}</div>")
}

fn signature_or_empty(signature: &str) -> String {
    if signature.is_empty() {
        String::new()
    } else {
        signature_block(signature)
    }
}

pub fn quote_reply_body(
    original_from: &str,
    original_date: &str,
    original_text: &str,
    signature: &str,
) -> String {
    format!(
        "<div><br></div>{}<div>On {}, {} wrote:</div><blockquote>{}</blockquote>",
        signature_or_empty(signature),
        escape(original_date),
        escape(original_from),
        to_html(original_text)
    )
}

pub fn forward_body(
    original_from: &str,
    original_date: &str,
    original_subject: &str,
    original_text: &str,
    signature: &str,
) -> String {
    format!(
        "<div><br></div>{}<div>---------- Forwarded message ----------<br>From: {}<br>Date: {}<br>Subject: {}</div><blockquote>{}</blockquote>",
        signature_or_empty(signature),
        escape(original_from),
        escape(original_date),
        escape(original_subject),
        to_html(original_text)
    )
}

#[derive(Debug, Error)]
pub enum ComposeError {
    #[error("invalid address {0:?}")]
    Address(String),
    #[error("could not build the message: {0}")]
    Build(#[from] lettre::error::Error),
}

fn lettre_mailbox(text: &str) -> Result<LettreMailbox, ComposeError> {
    let parsed =
        address::parse_first(text).ok_or_else(|| ComposeError::Address(text.to_string()))?;
    let addr: lettre::Address = parsed
        .address
        .parse()
        .map_err(|_| ComposeError::Address(text.to_string()))?;
    let name = if parsed.name.is_empty() {
        None
    } else {
        Some(parsed.name)
    };
    Ok(LettreMailbox::new(name, addr))
}

/// An image the composer embedded as a `data:` URI, lifted out of the HTML
/// so it can travel as a `multipart/related` part instead.
struct InlineImage {
    content_id: String,
    mime_type: String,
    content: Vec<u8>,
}

/// Replace every `<img src="data:image/…;base64,…">` in `html` with a
/// `cid:` reference and return the decoded images. Pasted and dropped images
/// land in the editor as data: URIs; sending those verbatim inflates the
/// HTML and several major clients refuse to show them, so they go out as
/// inline parts, the same shape the reader already understands.
fn extract_inline_images(html: &str) -> (String, Vec<InlineImage>) {
    use base64::Engine;
    if !html.contains("data:image/") {
        return (html.to_string(), Vec::new());
    }
    let re = regex::Regex::new(
        r#"(<img\b[^>]*?\bsrc\s*=\s*)(["'])data:(image/[A-Za-z0-9.+-]+);base64,([^"']*)["']"#,
    )
    .expect("inline image pattern is valid");
    let mut images = Vec::new();
    let html = re.replace_all(html, |captures: &regex::Captures| {
        let payload: String = captures[4].chars().filter(|c| !c.is_whitespace()).collect();
        match base64::engine::general_purpose::STANDARD.decode(payload) {
            Ok(content) => {
                let content_id = format!("{}@rustle", uuid::Uuid::new_v4().simple());
                let quote = &captures[2];
                let reference = format!("{}{quote}cid:{content_id}{quote}", &captures[1]);
                images.push(InlineImage {
                    content_id,
                    mime_type: captures[3].to_string(),
                    content,
                });
                reference
            }
            Err(_) => captures[0].to_string(),
        }
    });
    (html.into_owned(), images)
}

/// What goes into an outgoing message. `bcc` is only written into a draft:
/// on a message being sent it belongs on the envelope.
#[derive(Clone, Copy, Debug, Default)]
pub struct Outgoing<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body_html: &'a str,
    pub attachments: &'a [Attachment],
    /// The Message-ID header, angle brackets included; None makes one up.
    pub message_id: Option<&'a str>,
}

/// The raw message bytes for the wire: text and HTML alternatives, inline
/// images the HTML refers to, plus any attachments. Bcc is never written
/// into it -- that goes on the envelope.
pub fn build_mime_message(message: &Outgoing) -> Result<Vec<u8>, ComposeError> {
    build_message(message, false)
}

/// A draft, for the Drafts mailbox: the same message, with its Bcc kept so
/// whoever finishes it still has the addresses.
pub fn build_draft_message(message: &Outgoing) -> Result<Vec<u8>, ComposeError> {
    build_message(message, true)
}

/// A draft's recipients as typed. A half-written address is normal in a
/// draft and must not stop it being saved, so a list that doesn't parse
/// goes in as plain header text.
fn draft_recipients(
    builder: MessageBuilder,
    name: &'static str,
    texts: &[String],
) -> MessageBuilder {
    if texts.is_empty() {
        return builder;
    }
    let mailboxes: Result<Vec<LettreMailbox>, _> =
        texts.iter().map(|text| lettre_mailbox(text)).collect();
    match mailboxes {
        Ok(mailboxes) => mailboxes
            .into_iter()
            .fold(builder, |builder, mailbox| match name {
                "To" => builder.to(mailbox),
                "Cc" => builder.cc(mailbox),
                _ => builder.bcc(mailbox),
            }),
        Err(_) => builder.raw_header(HeaderValue::new(
            HeaderName::new_from_ascii_str(name),
            texts.join(", "),
        )),
    }
}

/// A fresh Message-ID for a draft, kept across saves so each save can
/// replace the copy before it.
pub fn new_message_id(from_addr: &str) -> String {
    let domain = address::first_address(from_addr)
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_string())
        .filter(|domain| !domain.is_empty())
        .unwrap_or_else(|| "rustle".to_string());
    format!("<{}@{domain}>", uuid::Uuid::new_v4().simple())
}

/// The part of a stored HTML body the editor can take: what is inside
/// <body> when the message is a whole document, otherwise all of it.
pub fn editable_body(html: &str) -> String {
    static BODY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?is)<body\b[^>]*>(.*?)(?:</body\s*>|$)").unwrap()
    });
    match BODY.captures(html) {
        Some(captures) => captures[1].trim().to_string(),
        None => html.trim().to_string(),
    }
}

fn build_message(message: &Outgoing, is_draft: bool) -> Result<Vec<u8>, ComposeError> {
    let from = lettre_mailbox(message.from)?;
    let mut builder = Message::builder()
        .from(from.clone())
        .subject(message.subject)
        .date_now();
    if let Some(id) = message.message_id {
        builder = builder.message_id(Some(id.to_string()));
    }
    if is_draft {
        // A draft goes nowhere, so its envelope is a formality, and one
        // with no recipient yet is still a draft.
        builder = builder
            .envelope(Envelope::new(Some(from.email.clone()), vec![from.email])?)
            .keep_bcc();
        builder = draft_recipients(builder, "To", message.to);
        builder = draft_recipients(builder, "Cc", message.cc);
        builder = draft_recipients(builder, "Bcc", message.bcc);
    } else {
        for to in message.to {
            builder = builder.to(lettre_mailbox(to)?);
        }
        for cc in message.cc {
            builder = builder.cc(lettre_mailbox(cc)?);
        }
    }
    let attachments = message.attachments;

    let (body_html, images) = extract_inline_images(message.body_html);
    let alternative = MultiPart::alternative_plain_html(html_to_text(&body_html), body_html);
    let body = if images.is_empty() {
        alternative
    } else {
        let mut related = MultiPart::related().multipart(alternative);
        for image in images {
            let content_type = ContentType::parse(&image.mime_type)
                .or_else(|_| ContentType::parse("application/octet-stream"))
                .expect("octet-stream is a valid content type");
            let part: SinglePart =
                LettreAttachment::new_inline(image.content_id).body(image.content, content_type);
            related = related.singlepart(part);
        }
        related
    };
    let message = if attachments.is_empty() {
        builder.multipart(body)?
    } else {
        let mut mixed = MultiPart::mixed().multipart(body);
        for attachment in attachments {
            let content_type = ContentType::parse(&attachment.mime_type)
                .or_else(|_| ContentType::parse("application/octet-stream"))
                .expect("octet-stream is a valid content type");
            let part: SinglePart = LettreAttachment::new(attachment.filename.clone())
                .body(attachment.content.clone(), content_type);
            mixed = mixed.singlepart(part);
        }
        builder.multipart(mixed)?
    };
    Ok(message.formatted())
}

/// Read the To/Cc headers back out of a stored message, for retrying from the
/// Outbox. Bcc addresses are never written to the stored message, so a Bcc'd
/// recipient is lost if the original send failed and is retried later.
pub fn extract_recipients(raw: &[u8]) -> Vec<String> {
    let Some(headers) = mail_parser::MessageParser::default().parse_headers(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in ["To", "Cc"] {
        for value in headers.header_values(name) {
            let text = value
                .as_text()
                .map(str::to_string)
                .unwrap_or_else(|| addresses_of(value));
            for mailbox in address::parse_list(&text) {
                if !mailbox.address.is_empty() {
                    out.push(mailbox.address);
                }
            }
        }
    }
    out
}

fn addresses_of(value: &mail_parser::HeaderValue) -> String {
    value
        .as_address()
        .map(|address| {
            address
                .iter()
                .filter_map(|addr| addr.address())
                .map(str::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// The composer fields a mailto: link asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailtoDraft {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body_html: String,
}

/// Split an RFC 6068 mailto: URI into composer fields. The body is escaped
/// into HTML here: it arrives from outside the app, and the composer renders
/// its body as HTML. "+" is kept as-is: RFC 6068 encodes spaces as %20 only,
/// and plus-addressed recipients must survive.
pub fn parse_mailto(uri: &str) -> MailtoDraft {
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let decode = |text: &str| percent_decode_str(text).decode_utf8_lossy().into_owned();

    let mut headers: Vec<(String, String)> = Vec::new();
    for part in query.split('&') {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        if key.is_empty() {
            continue;
        }
        let key = decode(key).to_lowercase();
        if !headers.iter().any(|(k, _)| *k == key) {
            headers.push((key, decode(value)));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };

    let addressed = decode(path.split_once(':').map(|(_, rest)| rest).unwrap_or(""));
    let recipients: Vec<String> = [addressed, header("to")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let body = header("body");
    MailtoDraft {
        to: recipients.join(", "),
        cc: header("cc"),
        bcc: header("bcc"),
        subject: header("subject"),
        body_html: if body.is_empty() {
            String::new()
        } else {
            to_html(&body)
        },
    }
}

/// Known addresses matching the one being typed after the last comma.
pub fn suggest_addresses<'a>(text: &str, addresses: &'a [String], limit: usize) -> Vec<&'a String> {
    let typed = text.rsplit(',').next().unwrap_or("").trim().to_lowercase();
    if typed.is_empty() {
        return Vec::new();
    }
    addresses
        .iter()
        .filter(|a| a.to_lowercase().contains(&typed))
        .take(limit)
        .collect()
}

/// Swap the address being typed for a picked one, ready for the next.
pub fn replace_last_address(text: &str, address: &str) -> String {
    match text.rsplit_once(',') {
        Some((head, _)) => format!("{head}, {address}, "),
        None => format!("{address}, "),
    }
}

/// Split a comma-separated entry into its non-empty pieces.
pub fn split_addresses(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// A link as typed, made absolute: a bare host gets `https://`, a bare
/// address `mailto:`, and anything already carrying a scheme is kept.
pub fn normalize_link(input: &str) -> String {
    let link = input.trim();
    if link.is_empty() {
        return String::new();
    }
    let has_scheme = link.split_once(':').is_some_and(|(scheme, rest)| {
        let mut chars = scheme.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            // "localhost:8080" is a host and port, not a scheme.
            && !rest.starts_with(|c: char| c.is_ascii_digit())
    });
    if has_scheme {
        link.to_string()
    } else if link.contains('@') && !link.contains('/') {
        format!("mailto:{link}")
    } else {
        format!("https://{link}")
    }
}

/// A To header as (display name, address), both empty if it names nobody.
pub fn first_recipient(to_header: &str) -> (String, String) {
    match address::parse_first(to_header) {
        Some(Mailbox { name, address }) => {
            let address = address.trim().to_lowercase();
            let name = if name.is_empty() {
                address.clone()
            } else {
                name
            };
            (name, address)
        }
        None => (String::new(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_made_absolute() {
        assert_eq!(normalize_link(" google.com "), "https://google.com");
        assert_eq!(normalize_link("https://x.org/a?b=1"), "https://x.org/a?b=1");
        assert_eq!(normalize_link("mailto:a@b.org"), "mailto:a@b.org");
        assert_eq!(normalize_link("a@b.org"), "mailto:a@b.org");
        assert_eq!(
            normalize_link("localhost:8080/x"),
            "https://localhost:8080/x"
        );
        assert_eq!(normalize_link("ftp://host/file"), "ftp://host/file");
        assert_eq!(normalize_link("  "), "");
    }

    #[test]
    fn subjects() {
        assert_eq!(reply_subject("Hi"), "Re: Hi");
        assert_eq!(reply_subject("RE: Hi"), "RE: Hi");
        assert_eq!(forward_subject("Hi"), "Fwd: Hi");
        assert_eq!(forward_subject("FW: Hi"), "FW: Hi");
    }

    #[test]
    fn reply_all_excludes_self_and_target() {
        let cc = reply_all_cc(
            "me@x.y, ada@x.y",
            "bob@x.y, Ada <ada@x.y>",
            "me@x.y",
            "bob@x.y",
        );
        assert_eq!(cc, "ada@x.y");
    }

    #[test]
    fn builds_and_reads_back() {
        let raw = build_mime_message(&Outgoing {
            from: "me@example.com",
            to: &["Ada <ada@example.com>".into()],
            cc: &["bob@example.org".into()],
            bcc: &["hidden@example.org".into()],
            subject: "Hello",
            body_html: "<div>Hi <b>there</b></div>",
            attachments: &[Attachment {
                filename: "a.txt".into(),
                mime_type: "text/plain".into(),
                content: b"x".to_vec(),
            }],
            message_id: None,
        })
        .unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("Subject: Hello"));
        assert!(text.contains("multipart/mixed"));
        assert!(text.contains("multipart/alternative"));
        assert!(text.contains("a.txt"));
        assert_eq!(
            extract_recipients(&raw),
            vec!["ada@example.com", "bob@example.org"]
        );
        assert!(!text.contains("hidden@example.org"));
        let nonsense = Outgoing {
            from: "nonsense",
            ..Outgoing::default()
        };
        assert!(build_mime_message(&nonsense).is_err());
    }

    #[test]
    fn drafts_keep_bcc_and_their_message_id() {
        let id = new_message_id("Me <me@example.com>");
        assert!(id.starts_with('<') && id.ends_with("@example.com>"));
        assert_ne!(id, new_message_id("me@example.com"));
        assert!(new_message_id("").ends_with("@rustle>"));

        let raw = build_draft_message(&Outgoing {
            from: "me@example.com",
            to: &["ada@example.com".into()],
            bcc: &["hidden@example.org".into()],
            subject: "Later",
            body_html: "<div>half a thought</div>",
            message_id: Some(&id),
            ..Outgoing::default()
        })
        .unwrap();
        let parsed = crate::mime::parse_message(&raw);
        assert_eq!(parsed.bcc, vec!["hidden@example.org"]);
        assert_eq!(parsed.message_id, id);
        assert_eq!(parsed.subject, "Later");

        // Nobody to send it to yet, or an address still being typed.
        let raw = build_draft_message(&Outgoing {
            from: "me@example.com",
            cc: &["ada@exa".into(), "bob".into()],
            ..Outgoing::default()
        })
        .unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("Cc: ada@exa, bob"), "{text}");
        assert!(!text.contains("To:"));
    }

    #[test]
    fn editable_body_unwraps_a_whole_document() {
        assert_eq!(editable_body("<div>hi</div>"), "<div>hi</div>");
        assert_eq!(
            editable_body(
                "<html><head><style>p{}</style></head><BODY class=x>\n<p>hi</p>\n</body></html>"
            ),
            "<p>hi</p>"
        );
        assert_eq!(editable_body("<html><body><p>cut short"), "<p>cut short");
    }

    #[test]
    fn data_images_become_inline_parts() {
        let html = "<div>see <img src=\"data:image/png;base64,AQID\" alt=\"x\"> and \
                    <img src='data:image/gif;base64,not base64!'></div>";
        let (html, images) = extract_inline_images(html);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].content, vec![1, 2, 3]);
        assert!(html.contains(&format!(
            "<img src=\"cid:{}\" alt=\"x\">",
            images[0].content_id
        )));
        // Undecodable data stays put rather than being dropped.
        assert!(html.contains("data:image/gif;base64,not base64!"));

        let raw = build_mime_message(&Outgoing {
            from: "me@example.com",
            to: &["ada@example.com".into()],
            subject: "Pic",
            body_html: "<div><img src=\"data:image/png;base64,AQID\"></div>",
            attachments: &[Attachment {
                filename: "a.txt".into(),
                mime_type: "text/plain".into(),
                content: b"x".to_vec(),
            }],
            ..Outgoing::default()
        })
        .unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("multipart/mixed"));
        assert!(text.contains("multipart/related"));
        assert!(text.contains("Content-Disposition: inline"));
        assert!(!text.contains("data:image/png"));

        // The reader puts the image back where it was and does not list it
        // as an attachment.
        let parsed = crate::mime::parse_message(&raw);
        assert!(parsed
            .html_body
            .as_deref()
            .unwrap_or("")
            .contains("data:image/png;base64,AQID"));
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename, "a.txt");
    }

    #[test]
    fn mailto_parsing() {
        let draft =
            parse_mailto("mailto:a+b@x.y?subject=Hi%20there&cc=c@x.y&body=line1%0Aline2&to=d@x.y");
        assert_eq!(draft.to, "a+b@x.y, d@x.y");
        assert_eq!(draft.cc, "c@x.y");
        assert_eq!(draft.subject, "Hi there");
        assert_eq!(draft.body_html, "line1<br>line2");
        assert_eq!(parse_mailto("mailto:"), MailtoDraft::default());
    }

    #[test]
    fn address_helpers() {
        let known = vec!["Ada <ada@x.y>".to_string(), "bob@x.y".to_string()];
        assert_eq!(suggest_addresses("bob@x.y, AD", &known, 5), vec![&known[0]]);
        assert!(suggest_addresses("bob@x.y, ", &known, 5).is_empty());
        assert_eq!(
            replace_last_address("bob@x.y, ad", "ada@x.y"),
            "bob@x.y, ada@x.y, "
        );
        assert_eq!(replace_last_address("ad", "ada@x.y"), "ada@x.y, ");
        assert_eq!(split_addresses(" a@b ,, c@d"), vec!["a@b", "c@d"]);
        assert_eq!(
            first_recipient("Ada <ADA@x.y>, bob@x.y"),
            ("Ada".into(), "ada@x.y".into())
        );
        assert_eq!(first_recipient(""), (String::new(), String::new()));
    }
}

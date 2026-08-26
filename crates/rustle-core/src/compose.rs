//! Building outgoing mail: reply and forward bodies, the MIME message that
//! goes on the wire, and the composer's address helpers.

use crate::address::{self, Mailbox};
use crate::html::{escape, html_to_text, to_html};
use crate::models::Attachment;
use lettre::message::header::ContentType;
use lettre::message::{
    Attachment as LettreAttachment, Mailbox as LettreMailbox, MultiPart, SinglePart,
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

/// Reply All keeps the rest of the thread in the loop: the original To and
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

/// The raw message bytes for the wire: text and HTML alternatives, plus any
/// attachments. Bcc is never written into it -- that goes on the envelope.
pub fn build_mime_message(
    from_addr: &str,
    to_addrs: &[String],
    cc_addrs: &[String],
    subject: &str,
    body_html: &str,
    attachments: &[Attachment],
) -> Result<Vec<u8>, ComposeError> {
    let mut builder = Message::builder()
        .from(lettre_mailbox(from_addr)?)
        .subject(subject)
        .date_now();
    for to in to_addrs {
        builder = builder.to(lettre_mailbox(to)?);
    }
    for cc in cc_addrs {
        builder = builder.cc(lettre_mailbox(cc)?);
    }

    let alternative =
        MultiPart::alternative_plain_html(html_to_text(body_html), body_html.to_string());
    let message = if attachments.is_empty() {
        builder.multipart(alternative)?
    } else {
        let mut mixed = MultiPart::mixed().multipart(alternative);
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
        let raw = build_mime_message(
            "me@example.com",
            &["Ada <ada@example.com>".into()],
            &["bob@example.org".into()],
            "Hello",
            "<div>Hi <b>there</b></div>",
            &[Attachment {
                filename: "a.txt".into(),
                mime_type: "text/plain".into(),
                content: b"x".to_vec(),
            }],
        )
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
        assert!(build_mime_message("nonsense", &[], &[], "s", "", &[]).is_err());
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

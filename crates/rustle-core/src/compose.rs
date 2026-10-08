//! Building outgoing mail: reply and forward bodies, the MIME message that
//! goes on the wire, and the composer's address helpers.

use crate::address::{self, Mailbox};
use crate::html::{escape, html_to_text, to_html};
use crate::mime::ParsedMessage;
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

/// Who a reply is addressed to, as composer field entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplyRecipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
}

/// The recipients of a reply to `original`. Reply goes to every Reply-To
/// address, else to the sender. Reply All goes there too, and copies the
/// sender (when Reply-To pointed elsewhere, as a mailing list or a ticket
/// queue does) and everyone on the original To and Cc. A reply to a
/// message `own` sent (one in Sent, say) goes on to its To, and Reply All
/// to its Cc as well, as it does in Thunderbird and Gmail. `own` (the
/// account's addresses) are left out, and nobody is named twice.
pub fn reply_recipients(
    original: &ParsedMessage,
    own: &[String],
    should_reply_all: bool,
) -> ReplyRecipients {
    let parse = |texts: &[String]| -> Vec<Mailbox> {
        texts
            .iter()
            .flat_map(|text| address::parse_list(text))
            .filter(|mailbox| !mailbox.address.is_empty())
            .collect()
    };
    let is_own = |mailbox: &Mailbox| {
        own.iter()
            .any(|address| address.eq_ignore_ascii_case(&mailbox.address))
    };
    let from = parse(std::slice::from_ref(&original.from_header));
    // A Reply-To naming only us is no reason to write to ourselves.
    let mut reply_to = parse(std::slice::from_ref(&original.reply_to_header));
    reply_to.retain(|mailbox| !is_own(mailbox));
    let (original_to, original_cc) = (parse(&original.to), parse(&original.cc));
    let is_ours = !from.is_empty() && from.iter().all(is_own);
    let (to, cc) = if is_ours {
        let cc = if should_reply_all {
            original_cc
        } else {
            Vec::new()
        };
        (original_to, cc)
    } else {
        let cc = if should_reply_all {
            from.iter()
                .cloned()
                .chain(original_to)
                .chain(original_cc)
                .collect()
        } else {
            Vec::new()
        };
        let to = if reply_to.is_empty() {
            from.clone()
        } else {
            reply_to
        };
        (to, cc)
    };
    // Who to write to when that leaves nobody but ourselves: a note to self.
    let to_self = to.first().or(from.first()).map(recipient_entry);

    let mut seen: Vec<String> = Vec::new();
    let mut keep = |mailboxes: Vec<Mailbox>| -> Vec<String> {
        mailboxes
            .into_iter()
            .filter(|mailbox| !is_own(mailbox))
            .filter(|mailbox| {
                let key = mailbox.address.to_lowercase();
                let is_new = !seen.contains(&key);
                seen.push(key);
                is_new
            })
            .map(|mailbox| recipient_entry(&mailbox))
            .collect()
    };
    let (mut to, mut cc) = (keep(to), keep(cc));
    if to.is_empty() {
        to = if cc.is_empty() {
            to_self.into_iter().collect()
        } else {
            std::mem::take(&mut cc)
        };
    }
    ReplyRecipients { to, cc }
}

/// A mailbox as one entry of a composer's address field, which splits on
/// commas: `Name <address>`, or the bare address when the name would not
/// survive that.
fn recipient_entry(mailbox: &Mailbox) -> String {
    let name = &mailbox.name;
    if name.is_empty() || name.contains([',', '"', '<', '>']) {
        mailbox.address.clone()
    } else {
        mailbox.display()
    }
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
    #[error("OpenPGP: {0}")]
    Pgp(String),
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
    /// Send text/plain alone: the body's text, no HTML part.
    pub plain_text: bool,
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

/// An answer to a meeting invitation (iMIP, RFC 6047): a line of text for
/// people, and the iTIP REPLY from `invite::reply_ics` for the organizer's
/// calendar, as `text/calendar; method=REPLY` alternatives.
pub fn invitation_reply_message(
    from: &str,
    organizer: &str,
    subject: &str,
    text: &str,
    ics: &str,
) -> Result<Vec<u8>, ComposeError> {
    let calendar = ContentType::parse("text/calendar; charset=utf-8; method=REPLY")
        .expect("a valid content type");
    let message = Message::builder()
        .from(lettre_mailbox(from)?)
        .to(lettre_mailbox(organizer)?)
        .subject(subject)
        .date_now()
        .message_id(Some(new_message_id(from)))
        .multipart(
            MultiPart::alternative()
                .singlepart(SinglePart::plain(text.to_string()))
                .singlepart(SinglePart::builder().header(calendar).body(ics.to_string())),
        )?;
    Ok(message.formatted())
}

/// A message's body: one part, or a tree of them.
enum Body {
    Single(SinglePart),
    Multi(MultiPart),
}

impl Body {
    /// The part as it goes on the wire, its own headers included.
    fn formatted(&self) -> Vec<u8> {
        match self {
            Body::Single(part) => part.formatted(),
            Body::Multi(part) => part.formatted(),
        }
    }
}

/// The body of `message`: text (and HTML), inline images, attachments.
fn body(message: &Outgoing) -> Body {
    let attachments = message.attachments;
    if message.plain_text {
        let text = SinglePart::plain(html_to_text(message.body_html));
        return if attachments.is_empty() {
            Body::Single(text)
        } else {
            Body::Multi(with_attachments(
                MultiPart::mixed().singlepart(text),
                attachments,
            ))
        };
    }
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
    Body::Multi(if attachments.is_empty() {
        body
    } else {
        with_attachments(MultiPart::mixed().multipart(body), attachments)
    })
}

fn headers(message: &Outgoing, is_draft: bool) -> Result<MessageBuilder, ComposeError> {
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
    Ok(builder)
}

fn build_message(message: &Outgoing, is_draft: bool) -> Result<Vec<u8>, ComposeError> {
    let builder = headers(message, is_draft)?;
    let message = match body(message) {
        Body::Single(part) => builder.singlepart(part)?,
        Body::Multi(part) => builder.multipart(part)?,
    };
    Ok(message.formatted())
}

/// What OpenPGP does to a message on its way out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Protection {
    pub sign: bool,
    pub encrypt: bool,
}

/// The OpenPGP operations a protected message needs, so the MIME here is
/// testable without gpg (`pgp.rs` provides the real ones).
pub trait Crypto {
    /// A detached, armored signature over `entity`.
    fn sign(&self, entity: &[u8]) -> Result<Vec<u8>, String>;
    /// `entity` encrypted (and signed inside, when asked), armored.
    fn encrypt(&self, entity: &[u8], also_sign: bool) -> Result<Vec<u8>, String>;
}

/// A message signed and/or encrypted the RFC 3156 way: its body becomes
/// `multipart/signed` (the body, then its signature) or
/// `multipart/encrypted` (a version part, then the ciphertext). The
/// headers stay outside, readable. Without protection, the plain message.
pub fn build_protected_message(
    message: &Outgoing,
    protection: Protection,
    crypto: &dyn Crypto,
) -> Result<Vec<u8>, ComposeError> {
    if !protection.sign && !protection.encrypt {
        return build_mime_message(message);
    }
    let entity = body(message).formatted();
    // The CRLF before a boundary belongs to the boundary: the part, signed
    // or encrypted, ends before it.
    let entity = entity
        .strip_suffix(b"\r\n")
        .map(<[u8]>::to_vec)
        .unwrap_or(entity);
    let boundary = format!("rustle-{}", uuid::Uuid::new_v4().simple());
    let (content_type, mut content) = if protection.encrypt {
        let armored = crypto
            .encrypt(&entity, protection.sign)
            .map_err(ComposeError::Pgp)?;
        let mut content = format!(
            "This is an OpenPGP/MIME encrypted message (RFC 4880 and 3156)\r\n\
             --{boundary}\r\n\
             Content-Type: application/pgp-encrypted\r\n\
             Content-Description: PGP/MIME version identification\r\n\r\n\
             Version: 1\r\n\r\n\
             --{boundary}\r\n\
             Content-Type: application/octet-stream; name=\"encrypted.asc\"\r\n\
             Content-Description: OpenPGP encrypted message\r\n\
             Content-Disposition: inline; filename=\"encrypted.asc\"\r\n\r\n"
        )
        .into_bytes();
        content.extend_from_slice(&crlf(&armored));
        (
            format!(
                "multipart/encrypted; protocol=\"application/pgp-encrypted\";\r\n boundary=\"{boundary}\""
            ),
            content,
        )
    } else {
        let signature = crypto.sign(&entity).map_err(ComposeError::Pgp)?;
        let mut content = format!(
            "This is an OpenPGP/MIME signed message (RFC 4880 and 3156)\r\n--{boundary}\r\n"
        )
        .into_bytes();
        content.extend_from_slice(&entity);
        content.extend_from_slice(
            format!(
                "\r\n--{boundary}\r\n\
                 Content-Type: application/pgp-signature; name=\"signature.asc\"\r\n\
                 Content-Description: OpenPGP digital signature\r\n\
                 Content-Disposition: attachment; filename=\"signature.asc\"\r\n\r\n"
            )
            .as_bytes(),
        );
        content.extend_from_slice(&crlf(&signature));
        (
            format!(
                "multipart/signed; micalg=pgp-sha256;\r\n protocol=\"application/pgp-signature\"; boundary=\"{boundary}\""
            ),
            content,
        )
    };
    if !content.ends_with(b"\r\n") {
        content.extend_from_slice(b"\r\n");
    }
    content.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    // The headers lettre writes, less the body's description, which is ours.
    let shell = headers(message, false)?.body(String::new())?.formatted();
    let shell = String::from_utf8_lossy(&shell);
    let head = shell.split("\r\n\r\n").next().unwrap_or("");
    let mut out = String::new();
    let mut skipping = false;
    for line in head.split("\r\n") {
        if !line.starts_with([' ', '\t']) {
            let name = line.split(':').next().unwrap_or("").to_ascii_lowercase();
            skipping = name == "content-type" || name == "content-transfer-encoding";
        }
        if !skipping {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    if !out.to_ascii_lowercase().contains("mime-version:") {
        out.push_str("MIME-Version: 1.0\r\n");
    }
    out.push_str(&format!("Content-Type: {content_type}\r\n\r\n"));
    let mut raw = out.into_bytes();
    raw.extend_from_slice(&content);
    Ok(raw)
}

/// Armored output as gpg writes it (LF) on the wire (CRLF).
fn crlf(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + text.len() / 32);
    let mut previous = 0u8;
    for &byte in text {
        if byte == b'\n' && previous != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        previous = byte;
    }
    out
}

fn with_attachments(mut mixed: MultiPart, attachments: &[Attachment]) -> MultiPart {
    for attachment in attachments {
        let content_type = ContentType::parse(&attachment.mime_type)
            .or_else(|_| ContentType::parse("application/octet-stream"))
            .expect("octet-stream is a valid content type");
        let part: SinglePart = LettreAttachment::new(attachment.filename.clone())
            .body(attachment.content.clone(), content_type);
        mixed = mixed.singlepart(part);
    }
    mixed
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

/// Known addresses matching the one being typed after the last comma, in
/// the order given (most wanted first) -- except that one where a word
/// starts with what's typed beats one that only contains it: "al" means Alex
/// before it means Sally.
pub fn suggest_addresses<'a>(text: &str, addresses: &'a [String], limit: usize) -> Vec<&'a String> {
    let typed = text.rsplit(',').next().unwrap_or("").trim().to_lowercase();
    if typed.is_empty() {
        return Vec::new();
    }
    let mut starts = Vec::new();
    let mut contains = Vec::new();
    for address in addresses {
        let lower = address.to_lowercase();
        if !lower.contains(&typed) {
            continue;
        }
        let word_starts = lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word.starts_with(&typed))
            || lower.starts_with(&typed);
        if word_starts {
            starts.push(address);
        } else {
            contains.push(address);
        }
        if starts.len() >= limit {
            break;
        }
    }
    starts.into_iter().chain(contains).take(limit).collect()
}

/// The suggestion list a composer offers: people written to before, then
/// the desktop address books, then everyone else seen in mail. Each address
/// once, in its first place.
pub fn merge_suggestions(ranked: &[(String, u32)], book: &[(String, String)]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    let mut add = |label: String, merged: &mut Vec<String>| {
        let key = address::first_address(&label).to_lowercase();
        if seen.insert(if key.is_empty() {
            label.to_lowercase()
        } else {
            key
        }) {
            merged.push(label);
        }
    };
    for (label, _) in ranked.iter().filter(|(_, sent)| *sent > 0) {
        add(label.clone(), &mut merged);
    }
    for (name, address) in book {
        add(crate::db::contact_label(name, address), &mut merged);
    }
    for (label, _) in ranked.iter().filter(|(_, sent)| *sent == 0) {
        add(label.clone(), &mut merged);
    }
    merged
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

    /// Records what it was asked to sign or encrypt.
    struct FakeCrypto(std::cell::RefCell<Vec<u8>>);

    impl Crypto for FakeCrypto {
        fn sign(&self, entity: &[u8]) -> Result<Vec<u8>, String> {
            self.0.replace(entity.to_vec());
            Ok(b"-----BEGIN PGP SIGNATURE-----\nsig\n-----END PGP SIGNATURE-----\n".to_vec())
        }

        fn encrypt(&self, entity: &[u8], _also_sign: bool) -> Result<Vec<u8>, String> {
            self.0.replace(entity.to_vec());
            Ok(b"-----BEGIN PGP MESSAGE-----\nct\n-----END PGP MESSAGE-----\n".to_vec())
        }
    }

    #[test]
    fn protected_messages_take_rfc_3156_shape() {
        let to = vec!["Bob <bob@x.y>".to_string()];
        let attachment = Attachment {
            filename: "a.txt".into(),
            mime_type: "text/plain".into(),
            content: b"x".to_vec(),
        };
        let message = Outgoing {
            from: "me@x.y",
            to: &to,
            subject: "Secret plans",
            body_html: "<p>Hello <b>Bob</b></p>",
            attachments: std::slice::from_ref(&attachment),
            ..Outgoing::default()
        };
        let crypto = FakeCrypto(Default::default());
        let signed = build_protected_message(
            &message,
            Protection {
                sign: true,
                encrypt: false,
            },
            &crypto,
        )
        .unwrap();
        assert_eq!(crate::pgp::detect(&signed), crate::pgp::Protection::Signed);
        // A verifier cuts out exactly the bytes that were signed.
        let (cut, signature) = crate::pgp::signed_parts(&signed).unwrap();
        assert_eq!(cut, *crypto.0.borrow());
        assert!(signature.starts_with(b"-----BEGIN PGP SIGNATURE-----"));
        let parsed = crate::mime::parse_message(&signed);
        assert_eq!(parsed.subject, "Secret plans");
        assert_eq!(parsed.to, ["Bob <bob@x.y>"]);

        let encrypted = build_protected_message(
            &message,
            Protection {
                sign: true,
                encrypt: true,
            },
            &crypto,
        )
        .unwrap();
        assert_eq!(
            crate::pgp::detect(&encrypted),
            crate::pgp::Protection::Encrypted
        );
        let text = String::from_utf8_lossy(&encrypted);
        assert!(!text.contains("Hello"), "the body is inside the ciphertext");
        assert!(
            text.contains("Subject: Secret plans"),
            "the headers stay outside"
        );
        assert_eq!(
            crate::pgp::encrypted_payload(&encrypted).unwrap(),
            b"-----BEGIN PGP MESSAGE-----\r\nct\r\n-----END PGP MESSAGE-----"
        );
        // What was encrypted is the whole body, attachment and all.
        let inner = String::from_utf8_lossy(&crypto.0.borrow()).into_owned();
        assert!(inner.contains("a.txt"), "{inner}");
    }

    #[test]
    fn plain_text_messages_have_no_html_part() {
        let to = vec!["bob@x.y".to_string()];
        let message = Outgoing {
            from: "me@x.y",
            to: &to,
            subject: "Hi",
            body_html: "<p>Hello <b>Bob</b></p>",
            plain_text: true,
            ..Outgoing::default()
        };
        let raw = build_mime_message(&message).unwrap();
        let parsed = crate::mime::parse_message(&raw);
        assert!(
            parsed.html_body.is_none(),
            "{}",
            String::from_utf8_lossy(&raw)
        );
        assert_eq!(
            parsed.text_body.as_deref().map(str::trim),
            Some("Hello Bob")
        );
        let attachment = Attachment {
            filename: "a.txt".into(),
            mime_type: "text/plain".into(),
            content: b"x".to_vec(),
        };
        let with_file = Outgoing {
            attachments: std::slice::from_ref(&attachment),
            ..message
        };
        let parsed = crate::mime::parse_message(&build_mime_message(&with_file).unwrap());
        assert!(parsed.html_body.is_none());
        assert_eq!(parsed.attachments.len(), 1);
    }

    #[test]
    fn invitation_replies_carry_the_calendar_part() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:1\r\n\
                   DTSTART:20261008T180000Z\r\nATTENDEE;PARTSTAT=DECLINED:mailto:me@x.y\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        let raw = invitation_reply_message(
            "Me <me@x.y>",
            "Boss <boss@x.y>",
            "Declined: Plan",
            "Me declined.",
            ics,
        )
        .unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("method=REPLY"), "{text}");
        assert!(text.contains("To: Boss <boss@x.y>"), "{text}");
        let parsed = crate::mime::parse_message(&raw);
        assert_eq!(
            parsed.text_body.as_deref().map(str::trim),
            Some("Me declined.")
        );
        let invitation = parsed.invitation.expect("the calendar part is read back");
        assert_eq!(invitation.method, crate::invite::Method::Reply);
    }

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

    fn original(from: &str, reply_to: &str, to: &[&str], cc: &[&str]) -> ParsedMessage {
        ParsedMessage {
            from_header: from.into(),
            reply_to_header: reply_to.into(),
            to: to.iter().map(|text| text.to_string()).collect(),
            cc: cc.iter().map(|text| text.to_string()).collect(),
            ..ParsedMessage::default()
        }
    }

    fn recipients(to: &[&str], cc: &[&str]) -> ReplyRecipients {
        ReplyRecipients {
            to: to.iter().map(|text| text.to_string()).collect(),
            cc: cc.iter().map(|text| text.to_string()).collect(),
        }
    }

    #[test]
    fn replies_go_to_the_sender_and_reply_all_to_everyone_else() {
        let own = vec!["Me@x.y".to_string()];
        let message = original(
            "Bob Ng <bob@x.y>",
            "",
            &["me@x.y", "Ada <ada@x.y>"],
            &["BOB@x.y", "ada@x.y", "\"Lee, Cy\" <cy@x.y>"],
        );
        assert_eq!(
            reply_recipients(&message, &own, false),
            recipients(&["Bob Ng <bob@x.y>"], &[])
        );
        assert_eq!(
            reply_recipients(&message, &own, true),
            recipients(&["Bob Ng <bob@x.y>"], &["Ada <ada@x.y>", "cy@x.y"])
        );
    }

    #[test]
    fn replies_to_our_own_message_go_where_it_went() {
        let own = vec!["me@x.y".to_string()];
        let sent = original(
            "Me <ME@x.y>",
            "",
            &["Ada <ada@x.y>", "bob@x.y"],
            &["cy@x.y", "me@x.y"],
        );
        assert_eq!(
            reply_recipients(&sent, &own, false),
            recipients(&["Ada <ada@x.y>", "bob@x.y"], &[])
        );
        assert_eq!(
            reply_recipients(&sent, &own, true),
            recipients(&["Ada <ada@x.y>", "bob@x.y"], &["cy@x.y"])
        );
        // Sent to ourselves and Cc'd on: the Cc moves up to To.
        let sent = original("me@x.y", "", &["me@x.y"], &["cy@x.y"]);
        assert_eq!(
            reply_recipients(&sent, &own, true),
            recipients(&["cy@x.y"], &[])
        );
        // A note to self stays one.
        assert_eq!(
            reply_recipients(&sent, &own, false),
            recipients(&["me@x.y"], &[])
        );
        let bcc_only = original("Me <me@x.y>", "", &[], &[]);
        assert_eq!(
            reply_recipients(&bcc_only, &own, true),
            recipients(&["Me <me@x.y>"], &[])
        );
    }

    #[test]
    fn reply_to_wins_and_reply_all_keeps_the_author() {
        let own = vec!["me@x.y".to_string()];
        // A list that sets Reply-To to itself, naming two addresses.
        let message = original(
            "Ada <ada@x.y>",
            "list@x.y, Tickets <tickets@x.y>",
            &["list@x.y"],
            &["me@x.y", "bob@x.y"],
        );
        assert_eq!(
            reply_recipients(&message, &own, false),
            recipients(&["list@x.y", "Tickets <tickets@x.y>"], &[])
        );
        assert_eq!(
            reply_recipients(&message, &own, true),
            recipients(
                &["list@x.y", "Tickets <tickets@x.y>"],
                &["Ada <ada@x.y>", "bob@x.y"]
            )
        );
        // A Reply-To pointing at us is passed over.
        let message = original("Ada <ada@x.y>", "me@x.y", &["me@x.y"], &["bob@x.y"]);
        assert_eq!(
            reply_recipients(&message, &own, true),
            recipients(&["Ada <ada@x.y>"], &["bob@x.y"])
        );
        // Reply-To the sender's own address changes nothing.
        let message = original("Ada <ada@x.y>", "ADA@x.y", &["me@x.y"], &[]);
        assert_eq!(
            reply_recipients(&message, &own, true),
            recipients(&["ADA@x.y"], &[])
        );
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
            plain_text: false,
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
    }

    #[test]
    fn word_starts_beat_middles() {
        let known: Vec<String> = [
            "Sally Ng <sally@x.y>",
            "Alex Kim <alex@x.y>",
            "Val <v@al.y>",
        ]
        .map(String::from)
        .to_vec();
        let picked = suggest_addresses("al", &known, 5);
        assert_eq!(picked, [&known[1], &known[2], &known[0]]);
        assert_eq!(suggest_addresses("al", &known, 1), [&known[1]]);
    }

    #[test]
    fn suggestions_put_correspondents_then_books_then_the_rest() {
        let ranked = vec![
            ("Ada <ada@x.y>".to_string(), 3),
            ("Bob <bob@x.y>".to_string(), 0),
            ("cy@x.y".to_string(), 0),
        ];
        let book = vec![
            ("Cy Young".to_string(), "CY@x.y".to_string()),
            ("Ada L".to_string(), "ada@x.y".to_string()),
            ("Dee".to_string(), "dee@x.y".to_string()),
        ];
        assert_eq!(
            merge_suggestions(&ranked, &book),
            [
                "Ada <ada@x.y>",
                "Cy Young <CY@x.y>",
                "Dee <dee@x.y>",
                "Bob <bob@x.y>"
            ]
        );
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

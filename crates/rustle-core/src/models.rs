//! The plain records the rest of the crate speaks in. The GTK layer wraps
//! these in GObjects where a list model needs one; nothing here depends on it.

use std::fmt;

/// TCP ports are 16-bit and 0 is not dialable.
pub const MIN_PORT: u32 = 1;
pub const MAX_PORT: u32 = 65535;

/// SMTP over implicit TLS (SMTPS). Any other port is assumed to use STARTTLS.
pub const IMPLICIT_TLS_PORT: u16 = 465;

/// How a connection is secured. Persisted as its lowercase name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Security {
    /// Implicit TLS from the first byte.
    Tls,
    /// Plaintext, upgraded with STARTTLS before any credential is sent.
    StartTls,
    /// Plaintext, for a bridge on localhost (DavMail, Proton Bridge) that
    /// fronts the real server itself. Credentials go over the wire in the
    /// clear, so the dialog labels it as such.
    None,
}

impl Security {
    /// In the order the account dialog's combo rows list them.
    pub const ALL: [Security; 3] = [Security::Tls, Security::StartTls, Security::None];

    pub fn as_str(self) -> &'static str {
        match self {
            Security::Tls => "tls",
            Security::StartTls => "starttls",
            Security::None => "none",
        }
    }

    /// Parses the stored form; anything unknown reads as TLS, the safe default.
    pub fn parse(text: &str) -> Security {
        match text {
            "starttls" => Security::StartTls,
            "none" => Security::None,
            _ => Security::Tls,
        }
    }

    pub fn index(self) -> u32 {
        Security::ALL.iter().position(|s| *s == self).unwrap_or(0) as u32
    }

    pub fn from_index(index: u32) -> Security {
        Security::ALL
            .get(index as usize)
            .copied()
            .unwrap_or(Security::Tls)
    }

    /// The default SMTP security for a port: 465 is implicit TLS, everything
    /// else is assumed to negotiate up from plaintext.
    pub fn default_for_smtp_port(port: u16) -> Security {
        if port == IMPLICIT_TLS_PORT {
            Security::Tls
        } else {
            Security::StartTls
        }
    }
}

impl fmt::Display for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A port number from user input, or None when it isn't one. Returns None
/// rather than an error: the caller is validating a text entry, so "not a port
/// yet" is an expected state.
pub fn parse_port(text: &str) -> Option<u16> {
    let port: u32 = text.trim().parse().ok()?;
    (MIN_PORT..=MAX_PORT).contains(&port).then_some(port as u16)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: i64,
    pub email: String,
    pub display_name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    /// Set when the account came from GNOME Online Accounts, which is then
    /// where its credentials live instead of the keyring.
    pub goa_id: String,
    /// The colour that marks this account's mail, as a CSS hex string
    /// (`#rrggbb`). Empty until the user picks one; see [`Account::color_hex`].
    pub color: String,
    /// The signature appended to mail sent from this account; empty for none.
    /// HTML as saved by the signature editor; older rows hold plain text,
    /// which [`Account::signature_html`] converts.
    pub signature: String,
    /// What the user calls this account ("Work"), shown wherever the app
    /// names it instead of the address. Empty means the address.
    pub label: String,
}

/// The GNOME accent palette, handed out to accounts that haven't chosen a
/// colour so that two accounts never start out looking alike.
pub const ACCOUNT_PALETTE: [&str; 9] = [
    "#3584e4", "#2190a4", "#3a944a", "#c88800", "#ed5b00", "#e62d42", "#d56199", "#9141ac",
    "#6f8396",
];

impl Account {
    /// The chosen colour, or a palette default keyed by the account id so it
    /// is stable across launches without being stored.
    pub fn color_hex(&self) -> &str {
        if is_hex_color(&self.color) {
            &self.color
        } else {
            ACCOUNT_PALETTE[(self.id.max(1) as usize - 1) % ACCOUNT_PALETTE.len()]
        }
    }

    /// The signature to append to outgoing mail as an HTML fragment, or ""
    /// when there is none. Plain text saved before the editor could format
    /// is escaped on the way out. The editor may save a bare text node ahead
    /// of the first tag ("Name<div>Title</div>"), so HTML is recognised by
    /// containing markup, not by its first character.
    pub fn signature_html(&self) -> String {
        let signature = self.signature.trim();
        if looks_like_html(signature) {
            crate::html::strip_scripts(signature)
        } else {
            crate::html::to_html(signature)
        }
    }

    /// How the app names this account: the user's label, or the address.
    pub fn name(&self) -> &str {
        let label = self.label.trim();
        if label.is_empty() {
            &self.email
        } else {
            label
        }
    }

    /// The short name the unified inbox tags this account's mail with: the
    /// user's label, then the display name, then the part of the address
    /// before the `@`.
    pub fn short_label(&self) -> &str {
        let label = self.label.trim();
        let name = self.display_name.trim();
        if !label.is_empty() {
            label
        } else if !name.is_empty() {
            name
        } else {
            self.email.split('@').next().unwrap_or(&self.email)
        }
    }
}

/// `#rrggbb`, and nothing else: this is interpolated into CSS.
pub fn is_hex_color(text: &str) -> bool {
    text.len() == 7 && text.starts_with('#') && text[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

impl Account {
    pub fn is_online_account(&self) -> bool {
        !self.goa_id.is_empty()
    }
}

/// Everything `Database::save_account` needs; the id is assigned by SQLite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAccount {
    pub email: String,
    pub display_name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub goa_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub icon_name: String,
    pub parent_id: Option<i64>,
    pub delimiter: String,
}

impl Folder {
    /// Strip to the leaf name only when there is a parent row to indent under.
    pub fn display_delimiter(&self) -> Option<&str> {
        self.parent_id.map(|_| self.delimiter.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Email {
    pub id: i64,
    pub folder_id: i64,
    /// None until a sync assigns a UID -- a Sent copy saved locally right
    /// after sending has no server-side counterpart yet.
    pub server_id: Option<String>,
    pub sender: String,
    pub sender_address: String,
    /// Who the message went to. Only outgoing folders show it, where every
    /// message was sent by the account itself.
    pub recipient: String,
    pub recipient_address: String,
    pub subject: String,
    pub preview: String,
    /// ISO-8601 timestamp, or whatever the Date header held if unparseable.
    pub date: String,
    pub is_unread: bool,
    pub is_starred: bool,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub conversation_id: Option<i64>,
}

impl Email {
    /// A proxy for when a message arrived, for ordering threads/messages.
    ///
    /// The IMAP UID is guaranteed by the protocol to increase with arrival
    /// order within a folder, unlike the local autoincrement id: once
    /// load-on-scroll backfills older mail in a later fetch, that older mail
    /// gets a *newer* local id. A message with no UID yet (a Sent copy saved
    /// right after sending) sorts as the newest.
    pub fn arrival_key(&self) -> u32 {
        self.server_id
            .as_deref()
            .and_then(|uid| uid.parse().ok())
            .unwrap_or(u32::MAX)
    }
}

/// A thread: emails sorted oldest first, so `latest()` is the last one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub emails: Vec<Email>,
}

impl Conversation {
    pub fn new(emails: Vec<Email>) -> Self {
        Self { emails }
    }

    pub fn id(&self) -> i64 {
        let first = &self.emails[0];
        first.conversation_id.unwrap_or(first.id)
    }

    pub fn latest(&self) -> &Email {
        self.emails
            .last()
            .expect("a conversation holds at least one email")
    }

    pub fn subject(&self) -> &str {
        &self.latest().subject
    }

    pub fn date(&self) -> &str {
        &self.latest().date
    }

    pub fn preview(&self) -> &str {
        &self.latest().preview
    }

    pub fn count(&self) -> usize {
        self.emails.len()
    }

    pub fn folder_id(&self) -> i64 {
        self.latest().folder_id
    }

    pub fn is_unread(&self) -> bool {
        self.emails.iter().any(|mail| mail.is_unread)
    }

    pub fn is_starred(&self) -> bool {
        self.emails.iter().any(|mail| mail.is_starred)
    }

    /// Every distinct sender, in first-seen order.
    pub fn participants(&self) -> String {
        let mut seen: Vec<&str> = Vec::new();
        for mail in &self.emails {
            if !seen.contains(&mail.sender.as_str()) {
                seen.push(&mail.sender);
            }
        }
        seen.join(", ")
    }

    /// The IMAP UIDs of the messages, skipping any without one. A locally
    /// saved copy has nothing on the server to act on yet.
    pub fn server_uids(&self) -> Vec<String> {
        self.emails
            .iter()
            .filter_map(|mail| mail.server_id.clone())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub content: Vec<u8>,
}

impl Attachment {
    pub fn size(&self) -> usize {
        self.content.len()
    }
}

/// A fetched message's headers, already cleaned up for display: the sender is
/// a display name, the date is an ISO timestamp, and `is_unread` is the
/// inverse of the server's `\Seen` flag.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageHeader {
    pub uid: String,
    pub sender: String,
    pub sender_address: String,
    pub recipient: String,
    pub recipient_address: String,
    pub subject: String,
    pub date: String,
    pub is_unread: bool,
    pub is_starred: bool,
    pub preview: String,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    /// Every (name, address) pair on the message, for the contacts list.
    pub addresses: Vec<(String, String)>,
}

/// A closing tag or a `<br>` is markup; a lone `<` in "Me <me@example.com>"
/// is not.
fn looks_like_html(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("</") || lower.contains("<br")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_markup_is_recognised_after_a_leading_text_node() {
        assert!(looks_like_html("Brandon<div><b>Title</b></div>"));
        assert!(looks_like_html("Line one<BR>Line two"));
        assert!(!looks_like_html("Me <me@example.com>\nCheers"));
        assert!(!looks_like_html("Brandon"));
    }

    #[test]
    fn ports_are_validated() {
        assert_eq!(parse_port(" 993 "), Some(993));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("65536"), None);
        assert_eq!(parse_port("abc"), None);
    }

    #[test]
    fn security_round_trips() {
        for security in Security::ALL {
            assert_eq!(Security::parse(security.as_str()), security);
            assert_eq!(Security::from_index(security.index()), security);
        }
        assert_eq!(Security::parse("garbage"), Security::Tls);
        assert_eq!(Security::default_for_smtp_port(465), Security::Tls);
        assert_eq!(Security::default_for_smtp_port(587), Security::StartTls);
    }

    fn email(id: i64, uid: Option<&str>) -> Email {
        Email {
            id,
            folder_id: 1,
            server_id: uid.map(String::from),
            sender: format!("s{id}"),
            sender_address: String::new(),
            recipient: String::new(),
            recipient_address: String::new(),
            subject: "s".into(),
            preview: String::new(),
            date: String::new(),
            is_unread: id % 2 == 0,
            is_starred: false,
            message_id: String::new(),
            in_reply_to: String::new(),
            references: String::new(),
            conversation_id: Some(1),
        }
    }

    #[test]
    fn conversation_aggregates() {
        let conversation = Conversation::new(vec![email(1, Some("5")), email(2, None)]);
        assert_eq!(conversation.id(), 1);
        assert!(conversation.is_unread());
        assert_eq!(conversation.participants(), "s1, s2");
        assert_eq!(conversation.server_uids(), vec!["5".to_string()]);
        assert_eq!(email(2, None).arrival_key(), u32::MAX);
    }
}

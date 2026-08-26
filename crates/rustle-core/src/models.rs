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

#[cfg(test)]
mod tests {
    use super::*;

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

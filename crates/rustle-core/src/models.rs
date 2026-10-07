//! The plain records the rest of the crate speaks in. The GTK layer wraps
//! these in GObjects where a list model needs one; nothing here depends on it.

use std::fmt;

/// Stored placeholder for messages without a subject.
pub const NO_SUBJECT: &str = "(no subject)";

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

/// How an account signs in. Persisted as its lowercase name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Auth {
    /// A password from the keyring.
    #[default]
    Password,
    /// A short-lived token from Evolution Data Server (GNOME Online
    /// Accounts or EDS's own OAuth services), fetched for every connection.
    OAuth2,
}

impl Auth {
    pub fn as_str(self) -> &'static str {
        match self {
            Auth::Password => "password",
            Auth::OAuth2 => "oauth2",
        }
    }

    pub fn parse(text: &str) -> Auth {
        match text {
            "oauth2" => Auth::OAuth2,
            _ => Auth::Password,
        }
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
    /// GNOME Online Accounts' id when the account comes from there; only
    /// used to recognise the account across the move to EDS.
    pub goa_id: String,
    /// The account's mail account source in Evolution Data Server, where its
    /// servers and sign-in live. Empty only for an account not moved there yet.
    pub eds_uid: String,
    /// The SMTP transport source, which signs in separately.
    pub eds_smtp_uid: String,
    /// The top of the account's source tree: removed with the account, and
    /// where a password stored once for the whole account is kept.
    pub eds_root_uid: String,
    pub imap_user: String,
    pub imap_auth: Auth,
    pub smtp_user: String,
    pub smtp_auth: Auth,
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
    /// The new-mail sound, in the form `sounds::NotificationSound` stores;
    /// empty means the app-wide default.
    pub notification_sound: String,
    /// The file name of the picture the user gave this account, inside the
    /// app's `account-pictures` data directory; empty for none. See
    /// [`Account::picture_file`].
    pub picture: String,
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

impl Account {
    /// The account picture's file name, if it has one that stays inside the
    /// pictures directory: a bare name, never a path.
    pub fn picture_file(&self) -> Option<&str> {
        let name = self.picture.as_str();
        let is_bare = !name.is_empty()
            && !name.starts_with('.')
            && !name.contains(['/', '\\'])
            && !name.contains('\0');
        is_bare.then_some(name)
    }
}

/// `#rrggbb`, and nothing else: this is interpolated into CSS.
pub fn is_hex_color(text: &str) -> bool {
    text.len() == 7 && text.starts_with('#') && text[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

impl Account {
    /// Signs in with OAuth (Online Accounts and the like) rather than a
    /// password.
    pub fn is_online_account(&self) -> bool {
        self.imap_auth == Auth::OAuth2 || !self.goa_id.is_empty()
    }

    /// Created by this app in EDS, so removing it deletes it there too;
    /// any other account is only hidden.
    pub fn is_own(&self) -> bool {
        self.eds_root_uid.starts_with(crate::eds::OWN_PREFIX)
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
    pub eds_uid: String,
    pub eds_smtp_uid: String,
    pub eds_root_uid: String,
    pub imap_user: String,
    pub imap_auth: Auth,
    pub smtp_user: String,
    pub smtp_auth: Auth,
}

impl NewAccount {
    /// The server and sign-in part of a stored account, to compare against
    /// what EDS reports.
    pub fn from_row(account: &Account) -> Self {
        NewAccount {
            email: account.email.clone(),
            display_name: account.display_name.clone(),
            imap_host: account.imap_host.clone(),
            imap_port: account.imap_port,
            imap_security: account.imap_security,
            smtp_host: account.smtp_host.clone(),
            smtp_port: account.smtp_port,
            smtp_security: account.smtp_security,
            goa_id: account.goa_id.clone(),
            eds_uid: account.eds_uid.clone(),
            eds_smtp_uid: account.eds_smtp_uid.clone(),
            eds_root_uid: account.eds_root_uid.clone(),
            imap_user: account.imap_user.clone(),
            imap_auth: account.imap_auth,
            smtp_user: account.smtp_user.clone(),
            smtp_auth: account.smtp_auth,
        }
    }
}

impl From<&crate::eds::MailAccount> for NewAccount {
    fn from(account: &crate::eds::MailAccount) -> Self {
        NewAccount {
            email: account.email.clone(),
            display_name: if account.name.is_empty() {
                account.email.split('@').next().unwrap_or("").to_string()
            } else {
                account.name.clone()
            },
            imap_host: account.imap.host.clone(),
            imap_port: account.imap.port,
            imap_security: account.imap.security,
            smtp_host: account.smtp.host.clone(),
            smtp_port: account.smtp.port,
            smtp_security: account.smtp.security,
            goa_id: account.goa_id.clone(),
            eds_uid: account.uid.clone(),
            eds_smtp_uid: account.smtp.uid.clone(),
            eds_root_uid: account.root_uid.clone(),
            imap_user: account.imap.user.clone(),
            imap_auth: account.imap.auth,
            smtp_user: account.smtp.user.clone(),
            smtp_auth: account.smtp.auth,
        }
    }
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
    /// Outlook's pin-to-top; pinned messages sort above the day sections.
    pub is_pinned: bool,
    pub message_id: String,
}

impl Email {
    /// A proxy for when a message arrived, for ordering messages.
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
    pub is_pinned: bool,
    pub preview: String,
    pub message_id: String,
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
            is_pinned: false,
            message_id: String::new(),
        }
    }

    #[test]
    fn arrival_order() {
        assert_eq!(email(1, Some("5")).arrival_key(), 5);
        assert_eq!(email(2, None).arrival_key(), u32::MAX);
    }
}

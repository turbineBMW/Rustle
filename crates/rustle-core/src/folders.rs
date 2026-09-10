//! What a mailbox is for, worked out from its name. Every provider spells its
//! special folders differently ("Sent Items", "[Gmail]/Sent Mail", "Deleted
//! Items"), so the classification is by word, tolerant of casing.

use base64::Engine;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    Starred,
    Other,
}

/// The canonical IMAP inbox name. Servers vary the casing, so this is the
/// fallback rather than something to compare against -- see `inbox_name`.
pub const INBOX_MAILBOX: &str = "INBOX";

/// Folders Rustle maintains itself rather than mirroring from the server.
/// Outbox is local-only (`prune_folders` keeps it); Sent and Drafts are
/// created on demand when the server has no folder of that role.
pub const OUTBOX_FOLDER: &str = "Outbox";
pub const SENT_FOLDER: &str = "Sent";
pub const DRAFTS_FOLDER: &str = "Drafts";

/// Gmail nests its special folders under an unselectable "[Gmail]" container.
/// It isn't a real mailbox, so it's hidden and its children sit at the top.
pub const NAMESPACE_ROOTS: [&str; 2] = ["[Gmail]", "[Google Mail]"];

/// How servers spell each role, in the order tried: the first match wins, so
/// "Sent/Drafts" is a sent mailbox rather than a drafts one. Word boundaries
/// rather than bare substrings, so "Consent forms" isn't taken for Sent.
static ROLE_PATTERNS: LazyLock<Vec<(Regex, FolderRole)>> = LazyLock::new(|| {
    [
        (r"(?i)^inbox$", FolderRole::Inbox),
        (r"(?i)\bsent\b", FolderRole::Sent),
        (r"(?i)\bdrafts?\b", FolderRole::Drafts),
        (r"(?i)\b(trash|deleted|bin)\b", FolderRole::Trash),
        (r"(?i)\b(junk|spam)\b", FolderRole::Junk),
        (r"(?i)\b(archives?|all mail)\b", FolderRole::Archive),
        (r"(?i)\b(starred|flagged)\b", FolderRole::Starred),
    ]
    .into_iter()
    .map(|(pattern, role)| (Regex::new(pattern).expect("static pattern"), role))
    .collect()
});

pub fn role_for_folder(name: &str) -> FolderRole {
    ROLE_PATTERNS
        .iter()
        .find(|(pattern, _)| pattern.is_match(name))
        .map(|(_, role)| *role)
        .unwrap_or(FolderRole::Other)
}

/// Whether new mail landing in this folder deserves a notification. Junk
/// and Trash fill up on their own, and Sent/Drafts hold the user's own mail.
/// All Mail mirrors arrivals in other folders, so notifying duplicates them.
pub fn notifies_on_arrival(name: &str) -> bool {
    if display_name_for_folder(name, None).eq_ignore_ascii_case("All Mail") {
        return false;
    }
    !matches!(
        role_for_folder(name),
        FolderRole::Junk | FolderRole::Trash | FolderRole::Sent | FolderRole::Drafts
    )
}

/// Whether a folder holds mail this account sent, so the list shows the
/// recipient instead of the sender. Outbox is local-only and unknown to
/// `role_for_folder`, which classifies what the server offers.
pub fn is_outgoing_folder(name: &str) -> bool {
    name == OUTBOX_FOLDER || matches!(role_for_folder(name), FolderRole::Sent | FolderRole::Drafts)
}

/// The mailbox this server uses for a role, or None when it lists none.
pub fn mailbox_with_role<'a, I>(names: I, role: FolderRole) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    names.into_iter().find(|name| role_for_folder(name) == role)
}

/// The server's inbox mailbox: matched by role (Yahoo lists "Inbox"), with
/// the canonical name as the fallback.
pub fn inbox_name<'a, I>(names: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    mailbox_with_role(names, FolderRole::Inbox)
        .unwrap_or(INBOX_MAILBOX)
        .to_string()
}

/// The symbolic icon the sidebar shows for a mailbox.
pub fn icon_for_folder(name: &str) -> &'static str {
    match role_for_folder(name) {
        FolderRole::Inbox => "mail-unread-symbolic",
        FolderRole::Sent => "mail-send-symbolic",
        FolderRole::Drafts => "document-edit-symbolic",
        FolderRole::Archive => "mail-archive-symbolic",
        FolderRole::Trash => "user-trash-symbolic",
        FolderRole::Junk => "mail-mark-junk-symbolic",
        FolderRole::Starred => "starred-symbolic",
        FolderRole::Other => "folder-symbolic",
    }
}

/// The mailbox enclosing `name`, or "" when it sits at the top level. A
/// namespace root counts as the top level, since it is never shown.
pub fn parent_mailbox_name(name: &str, delimiter: &str) -> String {
    if delimiter.is_empty() {
        return String::new();
    }
    let parent = match name.rfind(delimiter) {
        Some(index) => &name[..index],
        None => "",
    };
    if NAMESPACE_ROOTS.contains(&parent) {
        String::new()
    } else {
        parent.to_string()
    }
}

/// The name a row shows: decoded from modified UTF-7, without the Gmail
/// namespace prefix, and cut to the leaf when the row is indented under one.
pub fn display_name_for_folder(name: &str, delimiter: Option<&str>) -> String {
    let mut name = decode_mailbox_name(name);
    for root in NAMESPACE_ROOTS {
        let prefix = format!("{root}/");
        if let Some(rest) = name.strip_prefix(&prefix) {
            name = rest.to_string();
            break;
        }
    }
    if let Some(delimiter) = delimiter.filter(|d| !d.is_empty()) {
        if let Some(index) = name.rfind(delimiter) {
            name = name[index + delimiter.len()..].to_string();
        }
    }
    name
}

/// Decode a mailbox name from modified UTF-7 (RFC 3501 5.1.3), so
/// "Entw&APw-rfe" reads as "Entwürfe".
pub fn decode_mailbox_name(name: &str) -> String {
    static CHUNK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&([A-Za-z0-9+,]*)-").unwrap());
    CHUNK
        .replace_all(name, |captures: &regex::Captures| {
            let encoded = &captures[1];
            if encoded.is_empty() {
                return "&".to_string(); // "&-" encodes a literal ampersand
            }
            let padded = format!("{}===", encoded.replace(',', "/"));
            let engine = base64::engine::general_purpose::STANDARD;
            let trimmed = &padded[..padded.len() - padded.len() % 4];
            match engine.decode(trimmed) {
                Ok(bytes) => {
                    let units: Vec<u16> = bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_be_bytes(*pair))
                        .collect();
                    String::from_utf16(&units).unwrap_or_else(|_| "\u{FFFD}".to_string())
                }
                Err(_) => "\u{FFFD}".to_string(),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn notification_folder_filter() {
        for name in [
            "Junk",
            "Spam",
            "Junk Mail",
            "[Gmail]/Spam",
            "Trash",
            "Deleted Items",
            "Sent",
            "Drafts",
            "All Mail",
            "[Gmail]/All Mail",
            "[Google Mail]/All Mail",
            "[Gmail]/all mail",
        ] {
            assert!(!super::notifies_on_arrival(name), "{name}");
        }
        for name in ["INBOX", "Work", "Archive", "Newsletters"] {
            assert!(super::notifies_on_arrival(name), "{name}");
        }
    }

    use super::*;

    #[test]
    fn classifies_by_name() {
        assert_eq!(role_for_folder("INBOX"), FolderRole::Inbox);
        assert_eq!(role_for_folder("Inbox"), FolderRole::Inbox);
        assert_eq!(role_for_folder("[Gmail]/Sent Mail"), FolderRole::Sent);
        assert_eq!(role_for_folder("Sent/Drafts"), FolderRole::Sent);
        assert_eq!(role_for_folder("Consent forms"), FolderRole::Other);
        assert_eq!(role_for_folder("Deleted Items"), FolderRole::Trash);
        assert_eq!(role_for_folder("[Gmail]/All Mail"), FolderRole::Archive);
        assert_eq!(role_for_folder("Projects"), FolderRole::Other);
        assert!(is_outgoing_folder("Outbox"));
        assert!(!is_outgoing_folder("INBOX"));
    }

    #[test]
    fn finds_role_mailboxes() {
        let names = ["Inbox", "Sent Items", "Junk"];
        assert_eq!(
            mailbox_with_role(names, FolderRole::Sent),
            Some("Sent Items")
        );
        assert_eq!(inbox_name(names), "Inbox");
        assert_eq!(inbox_name(["Sent"]), "INBOX");
    }

    #[test]
    fn hierarchy_helpers() {
        assert_eq!(parent_mailbox_name("[Gmail]/Sent Mail", "/"), "");
        assert_eq!(parent_mailbox_name("Work/2026", "/"), "Work");
        assert_eq!(parent_mailbox_name("Work", "/"), "");
        assert_eq!(parent_mailbox_name("Work.Sub", ""), "");
        assert_eq!(
            display_name_for_folder("[Gmail]/Sent Mail", None),
            "Sent Mail"
        );
        assert_eq!(display_name_for_folder("Work/2026", Some("/")), "2026");
        assert_eq!(display_name_for_folder("Entw&APw-rfe", None), "Entwürfe");
        assert_eq!(decode_mailbox_name("Tom &- Jerry"), "Tom & Jerry");
    }
}

//! The operations a worker thread runs: each connects, signs in, does one
//! thing and tears the session down. Nothing here touches the database.

use crate::address;
use crate::dates;
use crate::folders::{self, FolderRole};
use crate::models::{Account, MessageHeader};
use crate::net::auth::Credential;
use crate::net::errors::NetError;
use crate::net::imap::{FetchedHeader, ImapSession, MailboxInfo, GMAIL_CAPABILITY};
use crate::net::smtp::SmtpSession;
use crate::threader::NO_SUBJECT;
use log::warn;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, NetError>;

/// How many recent messages to pull per sync.
pub const RECENT_LIMIT: u32 = 50;

/// RFC 8058: the body is the whole request, and the server matches it verbatim.
const ONE_CLICK_BODY: &str = "List-Unsubscribe=One-Click";

/// Names the app rather than mimicking a browser: bot filters reject an
/// anonymous agent, not an honest one.
pub const UNSUBSCRIBE_USER_AGENT: &str = "Rustle";

#[derive(Clone, Debug, Default)]
pub struct SyncResult {
    pub folders: Vec<MailboxInfo>,
    pub messages: Vec<MessageHeader>,
    pub folder: String,
    /// Total messages in the selected mailbox.
    pub exists: u32,
    /// How far back from the newest this fetch reached.
    pub offset: u32,
    /// Authoritative UID snapshot, only for the newest page.
    pub all_uids: Option<HashSet<String>>,
    /// Server unread counts by mailbox name, for the folders not fetched.
    pub unread_counts: HashMap<String, u32>,
}

/// Results from the commands attempted by a mailbox move: a move that fails
/// part-way reports how many succeeded.
#[derive(Clone, Debug, Default)]
pub struct MoveResult {
    pub destination_uids: Vec<Option<String>>,
    pub failed_index: Option<usize>,
    pub error: Option<String>,
}

fn open_imap(account: &Account, credential: &Credential) -> Result<ImapSession> {
    let mut session =
        ImapSession::new(&account.imap_host, account.imap_port, account.imap_security);
    session.connect()?;
    session.sign_in(credential)?;
    Ok(session)
}

/// Connect, log in, and return the folder list + recent headers of one
/// mailbox (the inbox when `folder` is None). `offset` pages backwards.
pub fn fetch_mailbox(
    account: &Account,
    credential: &Credential,
    folder: Option<&str>,
    limit: u32,
    offset: u32,
) -> Result<SyncResult> {
    let mut session = open_imap(account, credential)?;
    let mailboxes = session.list_folders()?;
    let target = match folder {
        Some(name) => name.to_string(),
        None => folders::inbox_name(mailboxes.iter().map(|m| m.name.as_str())),
    };
    let exists = session.select(&target, false)?;
    let all_uids = if offset == 0 {
        Some(session.search_all_uids()?)
    } else {
        None
    };
    let raw = session.fetch_recent_headers(exists, limit, offset)?;
    let unread_counts = if offset == 0 {
        unread_counts(&mut session, &mailboxes, &target)
    } else {
        HashMap::new()
    };
    session.logout();

    Ok(SyncResult {
        folders: mailboxes,
        messages: raw.into_iter().map(to_message_header).collect(),
        folder: target,
        exists,
        offset,
        all_uids,
        unread_counts,
    })
}

/// Server unread counts for the role folders this sync didn't fetch. A
/// folder that won't answer is skipped rather than failing the sync.
fn unread_counts(
    session: &mut ImapSession,
    mailboxes: &[MailboxInfo],
    target: &str,
) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for mailbox in mailboxes {
        if mailbox.name == target || !mailbox.is_selectable {
            continue;
        }
        if folders::role_for_folder(&mailbox.name) == FolderRole::Other {
            continue;
        }
        match session.unseen_count(&mailbox.name) {
            Ok(count) => {
                counts.insert(mailbox.name.clone(), count);
            }
            Err(error) => warn!(
                "could not read the unread count of {}: {error}",
                mailbox.name
            ),
        }
    }
    counts
}

/// Turn raw wire headers into the display-ready form: the sender becomes a
/// display name, the date a timestamp, and `\Seen` inverts into `is_unread`.
pub fn to_message_header(fetched: FetchedHeader) -> MessageHeader {
    let (recipient, recipient_address) = crate::compose::first_recipient(&fetched.to_header);
    let addresses = [&fetched.from_header, &fetched.to_header, &fetched.cc_header]
        .into_iter()
        .flat_map(|header| address::parse_list(header))
        .map(|mailbox| (mailbox.name, mailbox.address))
        .collect();
    MessageHeader {
        uid: fetched.uid,
        sender: address::display_name(&fetched.from_header),
        sender_address: address::first_address(&fetched.from_header),
        recipient,
        recipient_address,
        subject: if fetched.subject.is_empty() {
            NO_SUBJECT.to_string()
        } else {
            fetched.subject
        },
        date: dates::to_iso(&fetched.date),
        is_unread: !fetched.is_seen,
        is_starred: fetched.is_flagged,
        preview: String::new(),
        message_id: fetched.message_id,
        in_reply_to: fetched.in_reply_to,
        references: fetched.references,
        addresses,
    }
}

/// Download a single full message.
pub fn fetch_full_message(
    account: &Account,
    credential: &Credential,
    folder_name: &str,
    uid: &str,
) -> Result<Vec<u8>> {
    let mut session = open_imap(account, credential)?;
    session.select(folder_name, false)?;
    let raw = session.fetch_message(uid)?;
    session.logout();
    Ok(raw)
}

/// Add or remove an IMAP flag on a set of messages, in one STORE. An empty
/// set is not an empty command but a malformed one, so it is skipped.
pub fn set_flag(
    account: &Account,
    credential: &Credential,
    folder_name: &str,
    uids: &[String],
    flag: &str,
    should_add: bool,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    let mut session = open_imap(account, credential)?;
    session.select(folder_name, true)?;
    session.store_flags(&uids.join(","), flag, should_add)?;
    session.logout();
    Ok(())
}

/// Move every message of a conversation to another mailbox.
pub fn move_messages(
    account: &Account,
    credential: &Credential,
    folder_name: &str,
    uids: &[String],
    destination: &str,
) -> Result<MoveResult> {
    let mut session = open_imap(account, credential)?;
    session.select(folder_name, true)?;
    let mut result = MoveResult::default();
    for (index, uid) in uids.iter().enumerate() {
        match session.r#move(uid, destination) {
            Ok(destination_uid) => result.destination_uids.push(destination_uid),
            Err(error) => {
                result.failed_index = Some(index);
                result.error = Some(error.to_string());
                break;
            }
        }
    }
    session.logout();
    Ok(result)
}

/// Connect, log in, and hand a fully-built message to the server, then file
/// a copy in Sent. The copy is never fatal: the mail has already gone out.
pub fn send_message(
    account: &Account,
    credential: &Credential,
    from_addr: &str,
    recipients: &[String],
    raw: &[u8],
) -> Result<()> {
    let mut session =
        SmtpSession::new(&account.smtp_host, account.smtp_port, account.smtp_security);
    session.connect()?;
    session.sign_in(credential)?;
    let sent = session.send_raw(from_addr, recipients, raw);
    session.quit();
    sent?;

    if let Err(error) = append_to_sent(account, credential, raw) {
        warn!(
            "could not save a copy of the sent message to Sent on {} (account {}): {error}",
            account.imap_host, account.email
        );
    }
    Ok(())
}

fn append_to_sent(account: &Account, credential: &Credential, raw: &[u8]) -> Result<()> {
    let mut session = open_imap(account, credential)?;
    if session.has_capability(GMAIL_CAPABILITY) {
        return Ok(());
    }
    let mailboxes = session.list_folders()?;
    match folders::mailbox_with_role(mailboxes.iter().map(|m| m.name.as_str()), FolderRole::Sent) {
        Some(sent) => session.append(sent, raw)?,
        None => warn!(
            "no Sent mailbox on {} (account {})",
            account.imap_host, account.email
        ),
    }
    session.logout();
    Ok(())
}

/// Send an RFC 8058 one-click unsubscribe. Nothing authenticates this beyond
/// the token already in the URL, so a redirect off https would put that
/// token on the wire in the clear -- redirects are refused entirely.
pub fn post_unsubscribe(url: &str) -> std::result::Result<(), String> {
    if !url.to_lowercase().starts_with("https:") {
        return Err(format!("refusing to unsubscribe over {url}: not https"));
    }
    let agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(UNSUBSCRIBE_USER_AGENT)
        .build()
        .new_agent();
    let response = agent
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(ONE_CLICK_BODY.as_bytes())
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("the list answered {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_fetched_headers() {
        let header = to_message_header(FetchedHeader {
            uid: "3".into(),
            from_header: "Ada Lovelace <ADA@example.com>".into(),
            to_header: "Bob <bob@x.y>, c@x.y".into(),
            cc_header: "d@x.y".into(),
            subject: String::new(),
            date: "Wed, 16 Jul 2026 10:00:00 +0000".into(),
            is_seen: false,
            ..FetchedHeader::default()
        });
        assert_eq!(header.sender, "Ada Lovelace");
        assert_eq!(header.sender_address, "ada@example.com");
        assert_eq!(header.recipient, "Bob");
        assert_eq!(header.subject, NO_SUBJECT);
        assert_eq!(header.date, "2026-07-16T10:00:00Z");
        assert!(header.is_unread);
        assert_eq!(header.addresses.len(), 4);
    }

    #[test]
    fn unsubscribe_requires_https() {
        assert!(post_unsubscribe("http://list.example/u").is_err());
    }
}

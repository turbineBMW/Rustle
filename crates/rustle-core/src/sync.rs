//! The operations a worker thread runs: each connects, signs in, does one
//! thing and tears the session down. Nothing here touches the database.

use crate::address;
use crate::dates;
use crate::folders::{self, FolderRole};
use crate::models::NO_SUBJECT;
use crate::models::{Account, MessageHeader};
use crate::net::auth::Credential;
use crate::net::errors::NetError;
use crate::net::imap::{
    quote_mailbox, uid_set, FetchedHeader, ImapSession, MailboxInfo, GMAIL_CAPABILITY,
};
use crate::net::pool;
use crate::net::smtp::SmtpSession;
use log::warn;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, NetError>;

/// How many recent messages to pull per sync.
pub const RECENT_LIMIT: u32 = 50;

/// How many missing headers one backfill batch pulls.
pub const BACKFILL_LIMIT: u32 = 200;

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
    /// How many messages the fetched window held, \Deleted ones included:
    /// paging counts by position, and those still take one up.
    pub fetched: u32,
    /// Messages in the window marked \Deleted, left out of `messages`: any
    /// row still kept for them goes.
    pub deleted_uids: Vec<String>,
    /// Authoritative UID snapshot (\Deleted left out), only for the newest page.
    pub all_uids: Option<HashSet<String>>,
    /// Server unread counts by mailbox name, for the folders not fetched.
    pub unread_counts: HashMap<String, u32>,
}

/// One folder the backfill may fill: its mailbox name and the UIDs the
/// database already holds for it.
#[derive(Clone, Debug, Default)]
pub struct BackfillFolder {
    pub name: String,
    pub local_uids: HashSet<String>,
}

/// One batch of the whole-mailbox download.
#[derive(Clone, Debug, Default)]
pub struct BackfillResult {
    /// The folder the batch came from; None when every folder was complete.
    pub folder: Option<String>,
    pub messages: Vec<MessageHeader>,
    /// How many messages that folder still lacks after this batch.
    pub remaining: u32,
    /// Total messages in that folder on the server.
    pub exists: u32,
    /// Folders found complete (or unopenable) on the way to this batch.
    pub completed: Vec<String>,
}

/// Connect and fetch one batch of the headers the database lacks, from the
/// first of `folders` that lacks any. Newest first, so a folder fills from
/// the top the way the list shows it. Every UID is asked for by name, so
/// mail arriving or leaving between batches never shifts the window.
pub fn backfill(
    account: &Account,
    credential: &Credential,
    folders: &[BackfillFolder],
    limit: u32,
) -> Result<BackfillResult> {
    let mut session = open_imap(account, credential)?;
    let mut completed = Vec::new();
    for folder in folders {
        let exists = match session.select(&folder.name, false) {
            Ok(exists) => exists,
            // The connection, not the folder, failed.
            Err(error) if !session.is_usable() => return Err(error),
            Err(error) => {
                // A folder that won't open (a bare container, a broken
                // share) is done as far as the backfill is concerned.
                warn!("could not open {} to backfill it: {error}", folder.name);
                completed.push(folder.name.clone());
                continue;
            }
        };
        let server_uids = session.search_undeleted_uids()?;
        let missing = missing_uids(&server_uids, &folder.local_uids);
        if missing.is_empty() {
            completed.push(folder.name.clone());
            continue;
        }
        let batch = &missing[..missing.len().min(limit.max(1) as usize)];
        let raw = session.fetch_headers_by_uid(&uid_set(batch))?;
        release(account, credential, session);
        // Marked \Deleted since the search: none of these is stored yet.
        let (messages, _) = split_deleted(raw);
        return Ok(BackfillResult {
            folder: Some(folder.name.clone()),
            messages,
            remaining: (missing.len() - batch.len()) as u32,
            exists,
            completed,
        });
    }
    release(account, credential, session);
    Ok(BackfillResult {
        completed,
        ..BackfillResult::default()
    })
}

/// The server's UIDs the database doesn't hold, newest (highest) first.
fn missing_uids(server: &HashSet<String>, local: &HashSet<String>) -> Vec<u32> {
    let mut missing: Vec<u32> = server
        .difference(local)
        .filter_map(|uid| uid.parse().ok())
        .collect();
    missing.sort_unstable_by(|a, b| b.cmp(a));
    missing
}

/// Results from the commands attempted by a mailbox move: a move that fails
/// part-way reports how many succeeded.
#[derive(Clone, Debug, Default)]
pub struct MoveResult {
    pub destination_uids: Vec<Option<String>>,
    pub failed_index: Option<usize>,
    pub error: Option<String>,
}

/// A signed-in session: a parked one when the pool has it, else a new one.
/// Hand it back with `release` once the job is done.
pub(crate) fn open_imap(account: &Account, credential: &Credential) -> Result<ImapSession> {
    if let Some(session) = pool::checkout(account, credential) {
        return Ok(session);
    }
    let mut session =
        ImapSession::new(&account.imap_host, account.imap_port, account.imap_security);
    session.connect()?;
    session.sign_in(credential)?;
    Ok(session)
}

/// A job is done with its session: park it for the next one. Safe to call
/// after a command failed, too: the pool drops a session whose last failure
/// left it out of step with the server, and keeps one that was only told no.
pub(crate) fn release(account: &Account, credential: &Credential, session: ImapSession) {
    pool::checkin(account, credential, session);
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
        Some(session.search_undeleted_uids()?)
    } else {
        None
    };
    let raw = session.fetch_recent_headers(exists, limit, offset)?;
    let unread_counts = if offset == 0 {
        unread_counts(&mut session, &mailboxes, &target)
    } else {
        HashMap::new()
    };
    release(account, credential, session);
    let fetched = raw.len() as u32;
    let (messages, deleted_uids) = split_deleted(raw);

    Ok(SyncResult {
        folders: mailboxes,
        messages,
        folder: target,
        exists,
        offset,
        fetched,
        deleted_uids,
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
            Err(error) => {
                warn!(
                    "could not read the unread count of {}: {error}",
                    mailbox.name
                );
                if !session.is_usable() {
                    break;
                }
            }
        }
    }
    counts
}

/// Search the full text of `mailboxes` on the server, for Smart Search:
/// the matching UIDs per mailbox. A mailbox that fails is logged and left
/// out, so one odd folder doesn't sink the rest.
pub fn search_text(
    account: &Account,
    credential: &Credential,
    mailboxes: &[String],
    criteria: &str,
) -> Result<Vec<(String, Vec<String>)>> {
    let mut session = open_imap(account, credential)?;
    let mut found = Vec::new();
    for mailbox in mailboxes {
        let uids = session
            .select(mailbox, false)
            .and_then(|_| session.search_uids(criteria));
        match uids {
            Ok(uids) => found.push((mailbox.clone(), uids)),
            Err(error) => {
                warn!(
                    "could not search {mailbox} on {} (account {}): {error}",
                    account.imap_host, account.email
                );
                if !session.is_usable() {
                    break;
                }
            }
        }
    }
    release(account, credential, session);
    Ok(found)
}

/// Fetched headers, display-ready, apart from the UIDs of those marked
/// \Deleted: those are shown nowhere, and nothing about them is kept.
fn split_deleted(raw: Vec<FetchedHeader>) -> (Vec<MessageHeader>, Vec<String>) {
    let (deleted, kept): (Vec<_>, Vec<_>) = raw.into_iter().partition(|header| header.is_deleted);
    (
        kept.into_iter().map(to_message_header).collect(),
        deleted.into_iter().map(|header| header.uid).collect(),
    )
}

/// Turn raw wire headers into the display-ready form: the sender becomes a
/// display name, the date a timestamp, and `\Seen` inverts into `is_unread`.
pub fn to_message_header(fetched: FetchedHeader) -> MessageHeader {
    let (recipient, recipient_address) = crate::compose::first_recipient(&fetched.to_header);
    let keys = crate::threads::keys(
        &fetched.message_id,
        &fetched.references,
        &fetched.in_reply_to,
        &fetched.thread_index,
    );
    let recipients = [&fetched.to_header, &fetched.cc_header]
        .into_iter()
        .filter(|header| !header.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
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
        is_pinned: fetched.is_pinned,
        preview: fetched.preview,
        message_id: fetched.message_id,
        recipients,
        body_text: fetched.body_text,
        thread_root: keys.root,
        thread_outlook: keys.outlook,
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
    release(account, credential, session);
    Ok(raw)
}

/// Add or remove an IMAP flag on a set of messages. An empty set is not an
/// empty command but a malformed one, so it is skipped.
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
    session.store_flags(uids, flag, should_add)?;
    release(account, credential, session);
    Ok(())
}

/// Move the selected messages to another mailbox.
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
    release(account, credential, session);
    Ok(result)
}

/// Fill in the conversation keys of messages fetched before they were kept:
/// the UIDs of one mailbox, and what each one's headers say.
pub fn fetch_thread_keys(
    account: &Account,
    credential: &Credential,
    mailbox: &str,
    uids: &[u32],
) -> Result<Vec<(String, crate::threads::ThreadKeys)>> {
    let mut session = open_imap(account, credential)?;
    session.select(mailbox, false)?;
    let found = session.fetch_thread_headers(&uid_set(uids))?;
    release(account, credential, session);
    Ok(found
        .into_iter()
        .map(|found| {
            let keys = crate::threads::keys(
                &found.message_id,
                &found.references,
                &found.in_reply_to,
                &found.thread_index,
            );
            (found.uid, keys)
        })
        .collect())
}

/// A change to the account's mailbox tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailboxChange {
    Create(String),
    Rename { from: String, to: String },
    Delete(String),
}

/// Create, rename or delete a mailbox. Names are as the server spells them
/// (modified UTF-7); the caller re-syncs the folder list afterwards.
pub fn change_mailbox(
    account: &Account,
    credential: &Credential,
    change: &MailboxChange,
) -> Result<()> {
    let mut session = open_imap(account, credential)?;
    match change {
        MailboxChange::Create(name) => session.create_mailbox(name)?,
        MailboxChange::Rename { from, to } => session.rename_mailbox(from, to)?,
        MailboxChange::Delete(name) => session.delete_mailbox(name)?,
    }
    release(account, credential, session);
    Ok(())
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
    release(account, credential, session);
    Ok(())
}

/// Where a saved draft landed on the server: its mailbox, and its UID when
/// the server could find it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedDraft {
    pub mailbox: String,
    pub uid: Option<String>,
}

/// File a draft in the account's Drafts mailbox, then remove the copies an
/// earlier save left there (the same Message-ID). The new copy goes up
/// first, so a failure part-way never loses the draft. Ok(None) when the
/// server has no Drafts mailbox.
pub fn save_draft(
    account: &Account,
    credential: &Credential,
    raw: &[u8],
    message_id: &str,
) -> Result<Option<SavedDraft>> {
    let mut session = open_imap(account, credential)?;
    let Some(mailbox) = drafts_mailbox(&mut session)? else {
        warn!(
            "no Drafts mailbox on {} (account {})",
            account.imap_host, account.email
        );
        release(account, credential, session);
        return Ok(None);
    };
    session.select(&mailbox, true)?;
    let criteria = message_id_criteria(message_id);
    let earlier = match &criteria {
        Some(criteria) => session.search_uids(criteria)?,
        None => Vec::new(),
    };
    session.append_draft(&mailbox, raw)?;
    let uid = match &criteria {
        Some(criteria) => session
            .search_uids(criteria)?
            .into_iter()
            .filter(|uid| !earlier.contains(uid))
            .max_by_key(|uid| uid.parse::<u32>().unwrap_or(0)),
        None => None,
    };
    if !earlier.is_empty() {
        session.delete_uids(&earlier)?;
    }
    release(account, credential, session);
    Ok(Some(SavedDraft { mailbox, uid }))
}

/// Remove every copy of a draft from the Drafts mailbox: it was sent, or
/// thrown away.
pub fn discard_draft(account: &Account, credential: &Credential, message_id: &str) -> Result<()> {
    let Some(criteria) = message_id_criteria(message_id) else {
        return Ok(());
    };
    let mut session = open_imap(account, credential)?;
    if let Some(mailbox) = drafts_mailbox(&mut session)? {
        session.select(&mailbox, true)?;
        let uids = session.search_uids(&criteria)?;
        if !uids.is_empty() {
            session.delete_uids(&uids)?;
        }
    }
    release(account, credential, session);
    Ok(())
}

fn drafts_mailbox(session: &mut ImapSession) -> Result<Option<String>> {
    let mailboxes = session.list_folders()?;
    let names = mailboxes
        .iter()
        .filter(|mailbox| mailbox.is_selectable)
        .map(|mailbox| mailbox.name.as_str());
    Ok(folders::mailbox_with_role(names, FolderRole::Drafts).map(str::to_string))
}

/// The SEARCH for one Message-ID. None for an empty one: HEADER matches
/// substrings, and "" would match every draft in the mailbox.
fn message_id_criteria(message_id: &str) -> Option<String> {
    let message_id = message_id.trim();
    if message_id.trim_matches(|c| c == '<' || c == '>').is_empty() {
        return None;
    }
    Some(format!("HEADER Message-ID {}", quote_mailbox(message_id)))
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

    fn uids(values: &[&str]) -> HashSet<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn missing_uids_are_newest_first_and_skip_junk() {
        let server = uids(&["1", "2", "3", "10", "7", "x"]);
        let local = uids(&["2", "10"]);
        assert_eq!(missing_uids(&server, &local), vec![7, 3, 1]);
        assert!(missing_uids(&local, &server).is_empty());
    }

    #[test]
    fn deleted_headers_are_left_out() {
        let fetched = |uid: &str, is_deleted: bool| FetchedHeader {
            uid: uid.into(),
            is_deleted,
            ..FetchedHeader::default()
        };
        let (messages, deleted) = split_deleted(vec![
            fetched("1", false),
            fetched("2", true),
            fetched("3", false),
        ]);
        let uids: Vec<&str> = messages.iter().map(|m| m.uid.as_str()).collect();
        assert_eq!(uids, ["1", "3"]);
        assert_eq!(deleted, ["2"]);
    }

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
    fn draft_search_never_matches_everything() {
        assert_eq!(message_id_criteria(""), None);
        assert_eq!(message_id_criteria(" <> "), None);
        assert_eq!(
            message_id_criteria("<a\"b@x.y>").as_deref(),
            Some("HEADER Message-ID \"<a\\\"b@x.y>\"")
        );
    }

    #[test]
    fn unsubscribe_requires_https() {
        assert!(post_unsubscribe("http://list.example/u").is_err());
    }
}

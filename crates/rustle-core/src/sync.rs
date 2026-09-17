//! The operations a worker thread runs: each connects, signs in, does one
//! thing and tears the session down. Nothing here touches the database.

use crate::address;
use crate::dates;
use crate::folders::{self, FolderRole};
use crate::models::NO_SUBJECT;
use crate::models::{Account, MessageHeader};
use crate::net::auth::Credential;
use crate::net::errors::NetError;
use crate::net::imap::{FetchedHeader, ImapSession, MailboxInfo, GMAIL_CAPABILITY};
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
    /// Authoritative UID snapshot, only for the newest page.
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
            Err(error) => {
                // A folder that won't open (a bare container, a broken
                // share) is done as far as the backfill is concerned.
                warn!("could not open {} to backfill it: {error}", folder.name);
                completed.push(folder.name.clone());
                continue;
            }
        };
        let server_uids = session.search_all_uids()?;
        let missing = missing_uids(&server_uids, &folder.local_uids);
        if missing.is_empty() {
            completed.push(folder.name.clone());
            continue;
        }
        let batch = &missing[..missing.len().min(limit.max(1) as usize)];
        let raw = session.fetch_headers_by_uid(&uid_set(batch))?;
        session.logout();
        return Ok(BackfillResult {
            folder: Some(folder.name.clone()),
            messages: raw.into_iter().map(to_message_header).collect(),
            remaining: (missing.len() - batch.len()) as u32,
            exists,
            completed,
        });
    }
    session.logout();
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

/// An IMAP sequence set for a run of UIDs, with consecutive values folded
/// into ranges so a 200-message batch stays a short command line.
fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    for uid in sorted {
        match run {
            Some((start, end)) if uid == end + 1 => run = Some((start, uid)),
            Some((start, end)) => {
                parts.push(range_text(start, end));
                run = Some((uid, uid));
            }
            None => run = Some((uid, uid)),
        }
    }
    if let Some((start, end)) = run {
        parts.push(range_text(start, end));
    }
    parts.join(",")
}

fn range_text(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

/// Results from the commands attempted by a mailbox move: a move that fails
/// part-way reports how many succeeded.
#[derive(Clone, Debug, Default)]
pub struct MoveResult {
    pub destination_uids: Vec<Option<String>>,
    pub failed_index: Option<usize>,
    pub error: Option<String>,
}

pub(crate) fn open_imap(account: &Account, credential: &Credential) -> Result<ImapSession> {
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
        is_pinned: fetched.is_pinned,
        preview: fetched.preview,
        message_id: fetched.message_id,
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
    fn uid_set_folds_runs_into_ranges() {
        assert_eq!(uid_set(&[]), "");
        assert_eq!(uid_set(&[5]), "5");
        assert_eq!(uid_set(&[9, 8, 7, 3, 1, 2, 7]), "1:3,7:9");
        assert_eq!(uid_set(&[4, 2]), "2,4");
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
    fn unsubscribe_requires_https() {
        assert!(post_unsubscribe("http://list.example/u").is_err());
    }
}

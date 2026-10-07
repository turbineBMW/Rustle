//! Changes made locally that the server has yet to hear about. A flag edit or
//! a move is applied to the database at once and recorded here; `replay`
//! sends the queue to the server in order, and whatever it could not reach
//! the server with stays queued for the next attempt -- offline, a restart,
//! a dropped connection all leave the change waiting rather than lost.

use crate::models::{Account, MessageHeader};
use crate::net::auth::Credential;
use crate::net::errors::{classify, Failure, NetError};
use crate::net::imap::{ImapSession, FLAG_FLAGGED, FLAG_PINNED, FLAG_SEEN};
use crate::sync::{open_imap, release};
use std::collections::HashSet;

/// What a queued operation does to its messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Add or remove one IMAP flag.
    Flag {
        uids: Vec<String>,
        flag: String,
        add: bool,
    },
    /// Move to another mailbox of the same account. `email_ids` and `uids`
    /// are index-aligned: the local rows (destination placeholders) and the
    /// source UIDs they came from.
    Move {
        email_ids: Vec<i64>,
        uids: Vec<String>,
        dest_id: i64,
        dest: String,
    },
}

/// One change to one source mailbox. `folder_id` and `folder` name the same
/// mailbox: the id for the database, the name for the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOp {
    pub id: i64,
    pub account_id: i64,
    pub folder_id: i64,
    pub folder: String,
    pub change: Change,
}

/// What became of one operation the server answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Done. For a move, the destination UID of each message, when the
    /// server said (COPYUID); empty for a flag.
    Done(Vec<Option<String>>),
    /// The server refused it, and retrying won't change that (the mailbox
    /// is gone, the command is rejected). `moved` is how far a move got first.
    Refused {
        moved: Vec<Option<String>>,
        reason: String,
    },
}

/// The result of one pass over an account's queue. Operations after a
/// connection failure are not attempted; they have no outcome and stay queued.
#[derive(Debug, Default)]
pub struct Replay {
    pub outcomes: Vec<(i64, Outcome)>,
    /// A move the connection dropped part-way through: its id and the
    /// destination UIDs of the messages that did move. The rest stays queued.
    pub interrupted: Option<(i64, Vec<Option<String>>)>,
    /// Why the pass stopped early, if it did.
    pub failure: Option<Failure>,
}

/// Send `ops` to the server, in order, over one session.
pub fn replay(account: &Account, credential: &Credential, ops: &[PendingOp]) -> Replay {
    let mut replay = Replay::default();
    if ops.is_empty() {
        return replay;
    }
    let mut session = match open_imap(account, credential) {
        Ok(session) => session,
        Err(error) => {
            replay.failure = Some(classify(&error, &account.imap_host));
            return replay;
        }
    };
    for op in ops {
        match run_op(&mut session, op, &account.imap_host) {
            Ok(outcome) => replay.outcomes.push((op.id, outcome)),
            Err(Stop { moved, failure }) => {
                if !moved.is_empty() {
                    replay.interrupted = Some((op.id, moved));
                }
                replay.failure = Some(failure);
                return replay;
            }
        }
    }
    release(account, credential, session);
    replay
}

/// A failure that ends the pass: the connection, not the operation, is at fault.
struct Stop {
    moved: Vec<Option<String>>,
    failure: Failure,
}

fn run_op(session: &mut ImapSession, op: &PendingOp, host: &str) -> Result<Outcome, Stop> {
    // A failure that left the connection out of step says nothing about
    // what the server made of the command: keep it queued for a new one.
    let refused_or_stop = |session: &ImapSession, moved: Vec<Option<String>>, error: NetError| {
        if error.is_transient() || !session.is_usable() {
            Err(Stop {
                moved,
                failure: classify(&error, host),
            })
        } else {
            Ok(Outcome::Refused {
                moved,
                reason: error.to_string(),
            })
        }
    };
    if let Err(error) = session.select(&op.folder, true) {
        return refused_or_stop(session, Vec::new(), error);
    }
    match &op.change {
        Change::Flag { uids, flag, add } => {
            if uids.is_empty() {
                return Ok(Outcome::Done(Vec::new()));
            }
            match session.store_flags(&uids.join(","), flag, *add) {
                Ok(()) => Ok(Outcome::Done(Vec::new())),
                Err(error) => refused_or_stop(session, Vec::new(), error),
            }
        }
        Change::Move { uids, dest, .. } => {
            let mut moved = Vec::new();
            for uid in uids {
                match session.r#move(uid, dest) {
                    Ok(dest_uid) => moved.push(dest_uid),
                    Err(error) => return refused_or_stop(session, moved, error),
                }
            }
            Ok(Outcome::Done(moved))
        }
    }
}

/// Make a fetched page agree with the changes still queued for its mailbox,
/// so a sync that lands before the queue drains doesn't undo them on screen:
/// a message queued to move away is dropped, and a queued flag wins over the
/// server's (stale) one. Ops apply in queue order.
pub fn overlay(folder_id: i64, ops: &[PendingOp], headers: &mut Vec<MessageHeader>) {
    let mut leaving: HashSet<&str> = HashSet::new();
    for op in ops.iter().filter(|op| op.folder_id == folder_id) {
        match &op.change {
            Change::Move { uids, .. } => leaving.extend(uids.iter().map(String::as_str)),
            Change::Flag { uids, flag, add } => {
                let uids: HashSet<&str> = uids.iter().map(String::as_str).collect();
                for header in headers.iter_mut() {
                    if uids.contains(header.uid.as_str()) {
                        set_header_flag(header, flag, *add);
                    }
                }
            }
        }
    }
    headers.retain(|header| !leaving.contains(header.uid.as_str()));
}

/// The UIDs queued to leave `folder_id`, which a sync must not re-add.
pub fn leaving_uids(folder_id: i64, ops: &[PendingOp]) -> HashSet<String> {
    ops.iter()
        .filter(|op| op.folder_id == folder_id)
        .filter_map(|op| match &op.change {
            Change::Move { uids, .. } => Some(uids.iter().cloned()),
            Change::Flag { .. } => None,
        })
        .flatten()
        .collect()
}

fn set_header_flag(header: &mut MessageHeader, flag: &str, add: bool) {
    if flag.eq_ignore_ascii_case(FLAG_SEEN) {
        header.is_unread = !add;
    } else if flag.eq_ignore_ascii_case(FLAG_FLAGGED) {
        header.is_starred = add;
    } else if flag.eq_ignore_ascii_case(FLAG_PINNED) {
        header.is_pinned = add;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(uid: &str) -> MessageHeader {
        MessageHeader {
            uid: uid.into(),
            is_unread: true,
            ..MessageHeader::default()
        }
    }

    fn op(id: i64, folder_id: i64, change: Change) -> PendingOp {
        PendingOp {
            id,
            account_id: 1,
            folder_id,
            folder: "INBOX".into(),
            change,
        }
    }

    fn flag(uids: &[&str], flag: &str, add: bool) -> Change {
        Change::Flag {
            uids: uids.iter().map(|uid| uid.to_string()).collect(),
            flag: flag.into(),
            add,
        }
    }

    #[test]
    fn overlay_keeps_queued_flags_and_drops_leaving_mail() {
        let ops = [
            op(1, 7, flag(&["1", "2"], FLAG_SEEN, true)),
            op(2, 7, flag(&["2"], FLAG_FLAGGED, true)),
            op(3, 7, flag(&["2"], FLAG_FLAGGED, false)),
            op(
                4,
                7,
                Change::Move {
                    email_ids: vec![30],
                    uids: vec!["3".into()],
                    dest_id: 8,
                    dest: "Archive".into(),
                },
            ),
            // Another mailbox's UIDs mean nothing here.
            op(5, 9, flag(&["4"], FLAG_PINNED, true)),
        ];
        let mut headers = vec![header("1"), header("2"), header("3"), header("4")];
        overlay(7, &ops, &mut headers);
        let uids: Vec<&str> = headers.iter().map(|h| h.uid.as_str()).collect();
        assert_eq!(uids, ["1", "2", "4"]);
        assert!(!headers[0].is_unread && !headers[1].is_unread);
        assert!(!headers[1].is_starred, "the later op wins");
        assert!(headers[2].is_unread && !headers[2].is_pinned);
        assert_eq!(leaving_uids(7, &ops), HashSet::from(["3".to_string()]));
        assert!(leaving_uids(9, &ops).is_empty());
    }
}

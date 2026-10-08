//! The Outbox's bookkeeping, shared by everything that sends from it: every
//! main window drains it, and a composer with no one to hand its message to
//! sends that one itself. What they must agree on is which messages are on
//! their way, and that a message the server took never goes twice.

use crate::compose;
use crate::dates;
use crate::db::Database;
use crate::models::MessageHeader;
use crate::net::errors::Failure;
use std::collections::HashSet;

/// The Outbox messages on their way. One for the whole app, not one per
/// window: whoever claims a message first sends it, and the rest leave it be.
#[derive(Debug, Default)]
pub struct InFlight(HashSet<i64>);

impl InFlight {
    pub fn contains(&self, email_id: i64) -> bool {
        self.0.contains(&email_id)
    }

    /// Claim a message to send. False when it's already on its way.
    pub fn claim(&mut self, email_id: i64) -> bool {
        self.0.insert(email_id)
    }

    pub fn release(&mut self, email_id: i64) {
        self.0.remove(&email_id);
    }
}

/// One message to send from the Outbox.
#[derive(Clone, Debug)]
pub struct Job {
    pub email_id: i64,
    /// The envelope, Bcc included.
    pub recipients: Vec<String>,
    pub raw: Vec<u8>,
    /// For its copy in Sent once it's gone.
    pub sent_header: MessageHeader,
}

/// One attempted send. `error` is None when the server took it.
#[derive(Debug)]
pub struct Attempt {
    pub job: Job,
    pub error: Option<Failure>,
}

/// Send each job with `send`. Without a credential nothing is tried, but
/// every job still comes back, failed: each is let go, the user is told,
/// and the next drain tries again.
pub fn send_all<C>(
    jobs: Vec<Job>,
    credential: Option<C>,
    mut send: impl FnMut(&C, &Job) -> Result<(), Failure>,
) -> Vec<Attempt> {
    jobs.into_iter()
        .map(|job| {
            let error = match &credential {
                Some(credential) => send(credential, &job).err(),
                None => Some(Failure::NoCredential),
            };
            Attempt { job, error }
        })
        .collect()
}

/// The copy of a drained message filed in Sent: from the account, to the
/// first address it went to.
pub fn sent_header(account_email: &str, subject: &str, raw: &[u8]) -> MessageHeader {
    // extract_recipients keeps only the addresses.
    let recipient = compose::extract_recipients(raw)
        .into_iter()
        .next()
        .unwrap_or_default();
    MessageHeader {
        sender: account_email.to_string(),
        sender_address: account_email.to_string(),
        recipient: recipient.clone(),
        recipient_address: recipient,
        subject: subject.to_string(),
        preview: subject.to_string(),
        date: dates::now_iso(),
        is_unread: false,
        ..MessageHeader::default()
    }
}

/// How a round of sends came out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// Taken by the server and out of the Outbox.
    pub sent: usize,
    /// Still in the Outbox, to go on the next drain.
    pub errors: Vec<Failure>,
}

/// Back from the server, on the main thread. A message it took gets its copy
/// in Sent, then leaves the Outbox whether or not the copy could be filed --
/// a failed copy is only logged. It stays in the Outbox only if the database
/// won't let it go, and then it stays claimed, so this session never sends
/// it again. Everything else is let go.
pub fn settle(
    db: &Database,
    in_flight: &mut InFlight,
    account_id: i64,
    attempts: Vec<Attempt>,
) -> Settled {
    let mut settled = Settled::default();
    for Attempt { job, error } in attempts {
        if let Some(error) = error {
            in_flight.release(job.email_id);
            settled.errors.push(error);
            continue;
        }
        // The copy first, while the Outbox row still holds its id: SQLite
        // hands a freed highest id to the next row, and an Undo or Edit
        // still on screen for the sent message would find the copy.
        let filed = db.sent_folder(account_id).and_then(|sent| {
            let row = db.save_email(sent.id, &job.sent_header)?;
            db.save_raw_message(row.id, &job.raw)
        });
        if let Err(error) = filed {
            log::error!(
                "could not file the sent copy of message {} in Sent (account {account_id}): {error}",
                job.email_id
            );
        }
        if let Err(error) = db.delete_email(job.email_id) {
            log::error!(
                "message {} was sent but could not leave the Outbox (account {account_id}): {error}",
                job.email_id
            );
            continue;
        }
        in_flight.release(job.email_id);
        settled.sent += 1;
    }
    settled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::OutboxEntry;
    use crate::models::{Auth, NewAccount, Security};

    fn account() -> NewAccount {
        NewAccount {
            email: "me@example.com".into(),
            display_name: "Me".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            smtp_security: Security::StartTls,
            goa_id: String::new(),
            eds_uid: String::new(),
            eds_smtp_uid: String::new(),
            eds_root_uid: String::new(),
            imap_user: String::new(),
            imap_auth: Auth::Password,
            smtp_user: String::new(),
            smtp_auth: Auth::Password,
        }
    }

    const RAW: &[u8] =
        b"From: me@example.com\r\nTo: you@example.com\r\nSubject: Hi\r\n\r\nHello\r\n";

    /// A message waiting in `account_id`'s Outbox, as Send leaves it.
    fn queued(db: &Database, account_id: i64) -> Job {
        let outbox = db
            .get_or_create_folder(account_id, crate::folders::OUTBOX_FOLDER, "o")
            .unwrap();
        let header = sent_header("me@example.com", "Hi", RAW);
        let row = db.save_email(outbox.id, &header).unwrap();
        db.save_raw_message(row.id, RAW).unwrap();
        db.set_outbox_entry(row.id, &OutboxEntry::default())
            .unwrap();
        Job {
            email_id: row.id,
            recipients: vec!["you@example.com".into()],
            raw: RAW.to_vec(),
            sent_header: header,
        }
    }

    fn outbox_mail(db: &Database, account_id: i64) -> Vec<crate::models::Email> {
        let outbox = db
            .folder_by_name(account_id, crate::folders::OUTBOX_FOLDER)
            .unwrap()
            .unwrap();
        db.emails_in_folder(outbox.id).unwrap()
    }

    #[test]
    fn a_message_is_claimed_once() {
        let mut in_flight = InFlight::default();
        assert!(in_flight.claim(7));
        assert!(!in_flight.claim(7));
        assert!(in_flight.contains(7));
        in_flight.release(7);
        assert!(!in_flight.contains(7));
        assert!(in_flight.claim(7));
    }

    #[test]
    fn no_credential_fails_every_job() {
        let jobs = (1..=3)
            .map(|email_id| Job {
                email_id,
                recipients: Vec::new(),
                raw: Vec::new(),
                sent_header: MessageHeader::default(),
            })
            .collect();
        let mut tried = 0;
        let attempts = send_all(jobs, None::<()>, |_, _| {
            tried += 1;
            Ok(())
        });
        assert_eq!(tried, 0);
        let ids: Vec<i64> = attempts.iter().map(|a| a.job.email_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(attempts
            .iter()
            .all(|a| a.error == Some(Failure::NoCredential)));
    }

    #[test]
    fn each_job_reports_its_own_outcome() {
        let jobs = (1..=2)
            .map(|email_id| Job {
                email_id,
                recipients: Vec::new(),
                raw: Vec::new(),
                sent_header: MessageHeader::default(),
            })
            .collect();
        let attempts = send_all(jobs, Some(()), |_, job| {
            if job.email_id == 1 {
                Ok(())
            } else {
                Err(Failure::Unreachable)
            }
        });
        assert_eq!(attempts[0].error, None);
        assert_eq!(attempts[1].error, Some(Failure::Unreachable));
    }

    #[test]
    fn a_sent_message_leaves_the_outbox_for_sent() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let job = queued(&db, account.id);
        let email_id = job.email_id;
        let mut in_flight = InFlight::default();
        in_flight.claim(email_id);
        let settled = settle(
            &db,
            &mut in_flight,
            account.id,
            vec![Attempt { job, error: None }],
        );
        assert_eq!(
            settled,
            Settled {
                sent: 1,
                errors: Vec::new()
            }
        );
        assert!(!in_flight.contains(email_id));
        assert!(outbox_mail(&db, account.id).is_empty());
        assert_eq!(db.outbox_entry(email_id).unwrap(), None);
        let sent = db.sent_folder(account.id).unwrap();
        let copies = db.emails_in_folder(sent.id).unwrap();
        assert_eq!(copies.len(), 1);
        // Not the Outbox row's id handed on: Undo may still name that one.
        assert_ne!(copies[0].id, email_id);
        assert_eq!(copies[0].subject, "Hi");
        assert_eq!(copies[0].recipient_address, "you@example.com");
        assert_eq!(db.raw_message(copies[0].id).unwrap().as_deref(), Some(RAW));
    }

    #[test]
    fn a_sent_message_leaves_the_outbox_even_when_filing_fails() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let job = queued(&db, account.id);
        let email_id = job.email_id;
        let mut in_flight = InFlight::default();
        in_flight.claim(email_id);
        // No such account: its Sent folder can't be made, so the copy fails.
        let settled = settle(
            &db,
            &mut in_flight,
            account.id + 1,
            vec![Attempt { job, error: None }],
        );
        assert_eq!(settled.sent, 1);
        assert!(settled.errors.is_empty());
        assert!(!in_flight.contains(email_id));
        assert!(outbox_mail(&db, account.id).is_empty());
    }

    #[test]
    fn a_failed_message_stays_queued_and_is_let_go() {
        let db = Database::open_in_memory().unwrap();
        let account = db.save_account(&account()).unwrap();
        let job = queued(&db, account.id);
        let email_id = job.email_id;
        let mut in_flight = InFlight::default();
        in_flight.claim(email_id);
        let settled = settle(
            &db,
            &mut in_flight,
            account.id,
            vec![Attempt {
                job,
                error: Some(Failure::NoCredential),
            }],
        );
        assert_eq!(
            settled,
            Settled {
                sent: 0,
                errors: vec![Failure::NoCredential]
            }
        );
        assert!(!in_flight.contains(email_id));
        assert_eq!(outbox_mail(&db, account.id).len(), 1);
        assert!(db.outbox_entry(email_id).unwrap().is_some());
    }
}

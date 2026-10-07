//! Running the account's rules (rustle_core::rules) over mail that just
//! arrived in its inbox: marks and stars go through the change queue like
//! any flag edit, moves like any move -- so they reach the server, and wait
//! for it when it can't be reached.

use super::MainWindow;
use crate::dialogs::rules::RulesDialog;
use adw::prelude::*;
use rustle_core::models::{Account, MessageHeader};
use rustle_core::net::imap::{FLAG_FLAGGED, FLAG_SEEN};
use rustle_core::queue::{Change, PendingOp};
use rustle_core::rules;
use std::collections::{HashMap, HashSet};

impl MainWindow {
    /// Apply the account's rules to `arrived` (just saved in folder
    /// `folder_id`). Returns the UIDs a rule moved away or marked read.
    pub(super) fn apply_rules(
        &self,
        account: &Account,
        folder_id: i64,
        arrived: &[MessageHeader],
    ) -> HashSet<String> {
        if arrived.is_empty() {
            return HashSet::new();
        }
        let account_rules = self
            .db()
            .borrow()
            .rules(Some(account.id))
            .unwrap_or_default();
        if account_rules.is_empty() {
            return HashSet::new();
        }
        let mut read = Vec::new();
        let mut starred = Vec::new();
        let mut moves: HashMap<i64, Vec<String>> = HashMap::new();
        for header in arrived {
            let verdict = rules::apply(&account_rules, header);
            if verdict.mark_read && header.is_unread {
                read.push(header.uid.clone());
            }
            if verdict.star && !header.is_starred {
                starred.push(header.uid.clone());
            }
            if let Some(dest) = verdict.move_to.filter(|dest| *dest != folder_id) {
                moves.entry(dest).or_default().push(header.uid.clone());
            }
        }
        let Some(folder) = self.db().borrow().folder(folder_id).ok().flatten() else {
            return HashSet::new();
        };
        let mut handled: HashSet<String> = read.iter().cloned().collect();

        for (uids, flag) in [(&read, FLAG_SEEN), (&starred, FLAG_FLAGGED)] {
            if uids.is_empty() {
                continue;
            }
            {
                let db = self.db();
                let db = db.borrow();
                for id in db.email_ids_for_uids(folder_id, uids).unwrap_or_default() {
                    let saved = if flag == FLAG_SEEN {
                        db.set_email_unread(id, false)
                    } else {
                        db.set_email_starred(id, true)
                    };
                    if let Err(error) = saved {
                        log::error!("could not apply a rule to message {id}: {error}");
                    }
                }
            }
            self.queue_change(
                account,
                PendingOp {
                    id: 0,
                    account_id: account.id,
                    folder_id,
                    folder: folder.name.clone(),
                    change: Change::Flag {
                        uids: uids.clone(),
                        flag: flag.to_string(),
                        add: true,
                    },
                },
            );
        }

        for (dest_id, uids) in moves {
            let dest = self.db().borrow().folder(dest_id).ok().flatten();
            // A rule naming a folder that's gone, or another account's.
            let Some(dest) = dest.filter(|dest| dest.account_id == account.id) else {
                log::warn!(
                    "a rule moves mail to folder {dest_id}, which {} doesn't have",
                    account.email
                );
                continue;
            };
            // Paired one by one, so the two lists stay index-aligned.
            let mut email_ids = Vec::new();
            let mut moved_uids = Vec::new();
            for uid in &uids {
                let ids = self
                    .db()
                    .borrow()
                    .email_ids_for_uids(folder_id, std::slice::from_ref(uid))
                    .unwrap_or_default();
                if let Some(id) = ids.first() {
                    email_ids.push(*id);
                    moved_uids.push(uid.clone());
                }
            }
            if email_ids.is_empty() {
                continue;
            }
            if let Err(error) = self.db().borrow_mut().move_emails(&email_ids, dest.id) {
                log::error!(
                    "could not move {} message(s) by a rule: {error}",
                    email_ids.len()
                );
                continue;
            }
            log::debug!(
                "a rule moved {} new message(s) of {} to {}",
                moved_uids.len(),
                account.email,
                dest.name
            );
            handled.extend(moved_uids.iter().cloned());
            self.queue_change(
                account,
                PendingOp {
                    id: 0,
                    account_id: account.id,
                    folder_id,
                    folder: folder.name.clone(),
                    change: Change::Move {
                        email_ids,
                        uids: moved_uids,
                        dest_id: dest.id,
                        dest: dest.name,
                    },
                },
            );
        }
        handled
    }

    pub(super) fn on_manage_rules(&self) {
        let accounts: Vec<Account> = {
            let state = self.state();
            let mut accounts: Vec<Account> = state.accounts.values().cloned().collect();
            accounts.sort_by_key(|account| account.id);
            accounts
        };
        RulesDialog::new(self.db(), accounts).present(Some(self));
    }
}

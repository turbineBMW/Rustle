//! The change queue's GTK half. A flag edit or a move is written to the
//! database and to `pending_ops` at once; `flush_queue` sends an account's
//! queue to the server and settles the database with what it answered. A
//! change the server couldn't be reached for stays queued and goes out on the
//! next flush: the next sync, the network coming back, the next launch.

use super::MainWindow;
use crate::i18n::{self, gettext};
use crate::workers;
use rustle_core::db::Database;
use rustle_core::models::{Account, MessageHeader};
use rustle_core::net::errors::Failure;
use rustle_core::queue::{self, Change, Outcome, PendingOp, Replay};
use rustle_core::secrets;
use std::collections::HashSet;

use gtk::glib;

impl MainWindow {
    /// Record a change made locally, then try to send it straight away.
    pub(super) fn queue_change(&self, account: &Account, op: PendingOp) {
        if let Err(error) = self.db().borrow().enqueue_op(&op) {
            log::error!(
                "could not queue a change to {} (account {}): {error}",
                op.folder,
                account.email
            );
            return;
        }
        self.flush_queue(account);
    }

    /// Send an account's queued changes, one pass at a time: a flush asked
    /// for while one runs waits for it, then goes again.
    pub(super) fn flush_queue(&self, account: &Account) {
        if !self.state().is_online {
            return;
        }
        {
            let mut state = self.state_mut();
            if !state.flushing_account_ids.insert(account.id) {
                state.flush_again.insert(account.id);
                return;
            }
        }
        let ops = match self.db().borrow().pending_ops(account.id) {
            Ok(ops) => ops,
            Err(error) => {
                log::error!(
                    "could not read the queued changes of {}: {error}",
                    account.email
                );
                Vec::new()
            }
        };
        if ops.is_empty() {
            self.state_mut().flushing_account_ids.remove(&account.id);
            return;
        }
        let job_account = account.clone();
        let job_ops = ops.clone();
        workers::run(
            move || {
                let Some(credential) = secrets::credential_for(&job_account) else {
                    return Replay {
                        failure: Some(Failure::NoCredential),
                        ..Replay::default()
                    };
                };
                queue::replay(&job_account, &credential, &job_ops)
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                account,
                move |replay: Replay| window.on_replayed(&account, &ops, replay)
            ),
        );
    }

    fn on_replayed(&self, account: &Account, ops: &[PendingOp], replay: Replay) {
        self.state_mut().flushing_account_ids.remove(&account.id);
        if self.is_stale(account) {
            self.state_mut().flush_again.remove(&account.id);
            return;
        }
        let op = |id: i64| ops.iter().find(|op| op.id == id);
        let mut refusals: Vec<String> = Vec::new();
        // Mailboxes whose server state should come back over a refused flag.
        let mut resync: HashSet<String> = HashSet::new();
        for (id, outcome) in &replay.outcomes {
            let Some(op) = op(*id) else {
                continue;
            };
            match outcome {
                Outcome::Done(moved) => {
                    if matches!(op.change, Change::Move { .. }) {
                        self.settle_move(op, moved, true);
                    }
                }
                Outcome::Refused { moved, reason } => {
                    log::error!(
                        "{} refused a queued {} in {} (account {}): {reason}",
                        account.imap_host,
                        op_kind(op),
                        op.folder,
                        account.email
                    );
                    refusals.push(reason.clone());
                    match op.change {
                        Change::Move { .. } => self.settle_move(op, moved, true),
                        Change::Flag { .. } => {
                            resync.insert(op.folder.clone());
                        }
                    }
                }
            }
            if let Err(error) = self.db().borrow().finish_op(op.id) {
                log::error!("could not clear queued change {}: {error}", op.id);
            }
        }
        if let Some((id, moved)) = &replay.interrupted {
            if let Some(op) = op(*id) {
                self.settle_move(op, moved, false);
                if let Err(error) = self.db().borrow().trim_move_op(op.id, moved.len()) {
                    log::error!("could not trim queued move {}: {error}", op.id);
                }
            }
        }
        if let Some(failure) = &replay.failure {
            let waiting = ops.len() - replay.outcomes.len();
            log::warn!(
                "{waiting} change(s) for {} stay queued until the server can be reached: {failure:?}",
                account.email
            );
        }

        let is_changed = !replay.outcomes.is_empty() || replay.interrupted.is_some();
        if is_changed {
            let keep_id = self.selected_email().map(|c| c.id());
            self.reload_folders();
            self.refresh_emails(keep_id);
        }
        for folder in resync {
            self.start_sync(account, true, Some(&folder), 0);
        }
        if let Some(reason) = refusals.first() {
            self.toast(&i18n::format(
                &gettext("Action failed: {msg}"),
                &[("msg", reason)],
            ));
        }
        let again = self.state_mut().flush_again.remove(&account.id);
        if again && replay.failure.is_none() {
            self.flush_queue(account);
        }
    }

    /// Settle the rows of a move the server answered: the first
    /// `moved.len()` reached the destination (under the UIDs it gave), and
    /// when `restore_rest` the others go back to the source. A move cut short
    /// by the connection keeps its rest queued instead.
    fn settle_move(&self, op: &PendingOp, moved: &[Option<String>], restore_rest: bool) {
        let Change::Move {
            email_ids,
            uids,
            dest_id,
            ..
        } = &op.change
        else {
            return;
        };
        let done = moved.len().min(email_ids.len());
        let arrived: Vec<(i64, i64, Option<String>)> = email_ids
            .iter()
            .zip(moved)
            .map(|(id, uid)| (*id, *dest_id, uid.clone()))
            .collect();
        let returned: Vec<(i64, i64, Option<String>)> = if restore_rest {
            email_ids
                .iter()
                .zip(uids)
                .skip(done)
                .map(|(id, uid)| (*id, op.folder_id, Some(uid.clone())))
                .collect()
        } else {
            Vec::new()
        };
        {
            let db = self.db();
            let mut db = db.borrow_mut();
            for (rows, what) in [(&arrived, "moved"), (&returned, "unmoved")] {
                if rows.is_empty() {
                    continue;
                }
                if let Err(error) = db.reconcile_moved_emails(rows) {
                    log::error!("could not settle {} {what} message(s): {error}", rows.len());
                }
            }
        }
        let mut state = self.state_mut();
        for uid in uids.iter().take(done) {
            // Moved: the source keeps the UID until a sync confirms it gone.
            let entry = state
                .move_tombstones
                .entry((op.folder_id, uid.clone()))
                .or_default();
            entry.active = (entry.active - 1).max(0);
            entry.awaiting += 1;
        }
        if restore_rest {
            for uid in uids.iter().skip(done) {
                let key = (op.folder_id, uid.clone());
                let Some(entry) = state.move_tombstones.get_mut(&key) else {
                    continue;
                };
                entry.active -= 1;
                if entry.active <= 0 && entry.awaiting <= 0 {
                    state.move_tombstones.remove(&key);
                }
            }
        }
    }

    /// Bring a page fetched from `folder_id` in line with what's still
    /// queued or in its undo window, so a sync that lands first doesn't put
    /// moved mail back or flip a flag the user just changed.
    pub(super) fn guard_fetched(
        &self,
        db: &Database,
        folder_id: i64,
        headers: &mut Vec<MessageHeader>,
    ) {
        match db.pending_ops_for_folder(folder_id) {
            Ok(ops) => queue::overlay(folder_id, &ops, headers),
            Err(error) => log::error!("could not read the queued changes of a folder: {error}"),
        }
        let state = self.state();
        headers.retain(|header| {
            !state
                .move_tombstones
                .contains_key(&(folder_id, header.uid.clone()))
        });
    }
}

fn op_kind(op: &PendingOp) -> &'static str {
    match op.change {
        Change::Flag { .. } => "flag change",
        Change::Move { .. } => "move",
    }
}

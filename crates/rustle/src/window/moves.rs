//! Archive, trash and move, with an undo window: the change is applied
//! locally at once and the IMAP MOVE runs a few seconds later unless undone.

use super::{MainWindow, MOVE_UNDO_MS};
use crate::i18n::{self, gettext};
use crate::objects::EmailObject;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::folders::{self, FolderRole};
use rustle_core::models::{Account, Folder};
use rustle_core::net::errors::classify;
use rustle_core::sync::MoveResult;
use rustle_core::{secrets, sync};
use std::collections::HashMap;
use std::time::Duration;

/// A move applied locally but not yet run on the server. `email_ids`, `uids`,
/// `originals` and `tombstones` are index-aligned: a move that fails part-way
/// reports how many succeeded, and the tail is restored by slicing all four.
/// The account is carried, not read at commit time: the undo window outlives
/// an account switch, and these UIDs only mean anything on their own server.
#[derive(Clone)]
pub struct PendingMove {
    pub account: Account,
    pub email_ids: Vec<i64>,
    pub uids: Vec<String>,
    pub originals: Vec<(i64, i64, Option<String>)>,
    pub source: Folder,
    pub dest: Folder,
    pub tombstones: Vec<(i64, String)>,
}

impl MainWindow {
    /// Archive moves to the archive folder -- except while reading the
    /// archive itself, where the same button unarchives to the inbox.
    fn archive_role(&self) -> FolderRole {
        match self.current_folder() {
            Some(folder) if folders::role_for_folder(&folder.name) == FolderRole::Archive => {
                FolderRole::Inbox
            }
            _ => FolderRole::Archive,
        }
    }

    pub(super) fn archive_label(&self) -> String {
        if self.archive_role() == FolderRole::Inbox {
            gettext("Unarchive")
        } else {
            gettext("Archive")
        }
    }

    pub(super) fn update_archive_button(&self) {
        let imp = self.imp();
        let is_unarchive = self.archive_role() == FolderRole::Inbox;
        let label = self.archive_label();
        imp.archive_button_content.set_label(&label);
        imp.archive_button_content.set_icon_name(if is_unarchive {
            "mail-unread-symbolic"
        } else {
            "mail-archive-symbolic"
        });
        imp.archive_button.set_tooltip_text(Some(&label));
    }

    pub(super) fn on_archive(&self) {
        self.start_move_by_role(self.archive_role());
    }

    pub(super) fn on_trash(&self) {
        self.start_move_by_role(FolderRole::Trash);
    }

    /// Move the selection to a folder picked from the menu, by folder id.
    /// Only emails of that folder's account can go there.
    pub(super) fn on_move(&self, folder_id: i64) {
        let Some((account, dest)) = self.account_for_folder(folder_id) else {
            return;
        };
        let emails: Vec<EmailObject> = self
            .selected_emails()
            .into_iter()
            .filter(|c| {
                self.account_for_folder(c.with(|c| c.folder_id))
                    .is_some_and(|(a, _)| a.id == account.id)
            })
            .collect();
        if emails.is_empty() {
            return;
        }
        let count = emails.len();
        let name = folders::display_name_for_folder(&dest.name, dest.display_delimiter());
        let title = i18n::plural(
            "Moved to {name}",
            "Moved {n} emails to {name}",
            count as u64,
            &[("name", &name)],
        );
        let groups = self.group_moves(emails, |_, _| Some(dest.clone()));
        self.start_move(groups, &title);
    }

    fn start_move_by_role(&self, role: FolderRole) {
        let emails = self.selected_emails();
        if emails.is_empty() {
            return;
        }
        let count = emails.len() as u64;
        let title = match role {
            FolderRole::Archive => i18n::plural("Archived", "Archived {n} emails", count, &[]),
            FolderRole::Inbox => i18n::plural("Unarchived", "Unarchived {n} emails", count, &[]),
            _ => i18n::plural("Deleted", "Deleted {n} emails", count, &[]),
        };
        let groups = self.group_moves(emails, |account_id, source| {
            self.folder_with_role(account_id, role, source.id)
        });
        if groups.is_empty() {
            self.toast(&i18n::format(
                &gettext("No {role} folder found."),
                &[("role", &format!("{role:?}").to_lowercase())],
            ));
            return;
        }
        self.start_move(groups, &title);
    }

    /// Bucket a selection by source folder and resolve each bucket's
    /// destination -- in the unified inbox one selection spans accounts,
    /// and each has its own Archive and Trash.
    fn group_moves(
        &self,
        emails: Vec<EmailObject>,
        dest_for: impl Fn(i64, &Folder) -> Option<Folder>,
    ) -> Vec<(Account, Folder, Folder, Vec<EmailObject>)> {
        let mut by_source: HashMap<i64, Vec<EmailObject>> = HashMap::new();
        for email in emails {
            by_source
                .entry(email.with(|c| c.folder_id))
                .or_default()
                .push(email);
        }
        let mut groups = Vec::new();
        for (folder_id, group) in by_source {
            let Some((account, source)) = self.account_for_folder(folder_id) else {
                continue;
            };
            let Some(dest) = dest_for(account.id, &source) else {
                continue;
            };
            if dest.id == source.id {
                continue;
            }
            groups.push((account, source, dest, group));
        }
        groups
    }

    /// The folder this account uses for a role, other than `exclude_id`.
    /// When an account has both "Archive" and Gmail's "All Mail", a folder
    /// actually named Archive is the one the user means.
    fn folder_with_role(
        &self,
        account_id: i64,
        role: FolderRole,
        exclude_id: i64,
    ) -> Option<Folder> {
        let matches: Vec<Folder> = self
            .db()
            .borrow()
            .folders_for_account(account_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|folder| {
                folder.id != exclude_id && folders::role_for_folder(&folder.name) == role
            })
            .collect();
        if role == FolderRole::Archive {
            if let Some(folder) = matches
                .iter()
                .find(|folder| folder.name.to_lowercase().contains("archive"))
            {
                return Some(folder.clone());
            }
        }
        matches.into_iter().next()
    }

    /// Move emails optimistically: update the DB and drop them from
    /// the list now, then run the real IMAP MOVE after the undo window.
    fn start_move(&self, groups: Vec<(Account, Folder, Folder, Vec<EmailObject>)>, verb: &str) {
        self.commit_pending_moves();
        let mut pending_moves = Vec::new();
        for (account, source, dest, emails) in groups {
            // Pair each mail with its UID in one pass so the "has a UID"
            // narrowing survives into the index-aligned vectors. A locally
            // saved copy has no UID yet.
            let mut email_ids = Vec::new();
            let mut uids = Vec::new();
            for email in &emails {
                email.with(|mail| {
                    if let Some(uid) = &mail.server_id {
                        email_ids.push(mail.id);
                        uids.push(uid.clone());
                    }
                });
            }
            if email_ids.is_empty() {
                continue;
            }
            let originals = email_ids
                .iter()
                .zip(&uids)
                .map(|(id, uid)| (*id, source.id, Some(uid.clone())))
                .collect();
            let tombstones: Vec<(i64, String)> =
                uids.iter().map(|uid| (source.id, uid.clone())).collect();
            if let Err(error) = self.db().borrow_mut().move_emails(&email_ids, dest.id) {
                log::error!(
                    "could not move {} message(s) locally: {error}",
                    email_ids.len()
                );
                continue;
            }
            {
                let mut state = self.state_mut();
                for tombstone in &tombstones {
                    state
                        .move_tombstones
                        .entry(tombstone.clone())
                        .or_default()
                        .active += 1;
                }
            }
            pending_moves.push(PendingMove {
                account,
                email_ids,
                uids,
                originals,
                source,
                dest,
                tombstones,
            });
        }
        if pending_moves.is_empty() {
            return;
        }

        self.reload_folders();
        self.refresh_emails(None);

        let toast = adw::Toast::builder()
            .title(verb)
            .button_label(gettext("Undo"))
            .build();
        toast.connect_button_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_undo_move()
        ));
        let timeout = glib::timeout_add_local_once(
            Duration::from_millis(MOVE_UNDO_MS),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || window.on_move_timeout()
            ),
        );
        {
            let mut state = self.state_mut();
            state.pending_toast = Some(toast.clone());
            state.pending_moves = pending_moves;
            state.pending_timeout = Some(timeout);
        }
        self.imp().toast_overlay.add_toast(toast);
    }

    fn take_pending(&self) -> Vec<PendingMove> {
        let mut state = self.state_mut();
        if let Some(timeout) = state.pending_timeout.take() {
            timeout.remove();
        }
        state.pending_toast = None;
        std::mem::take(&mut state.pending_moves)
    }

    fn on_undo_move(&self) {
        for pending in self.take_pending() {
            self.restore_move(&pending);
        }
    }

    /// The undo window elapsed -- actually send the moves to the server.
    fn on_move_timeout(&self) {
        let mut state = self.state_mut();
        state.pending_timeout = None;
        state.pending_toast = None;
        let pending_moves = std::mem::take(&mut state.pending_moves);
        drop(state);
        for pending in pending_moves {
            self.run_move_worker(pending);
        }
    }

    /// A newer action arrived: send the previous pending moves now instead
    /// of waiting for their timer.
    fn commit_pending_moves(&self) {
        let toast = self.state().pending_toast.clone();
        let pending_moves = self.take_pending();
        if let Some(toast) = toast {
            toast.dismiss();
        }
        for pending in pending_moves {
            self.run_move_worker(pending);
        }
    }

    fn restore_move(&self, pending: &PendingMove) {
        if let Err(error) = self
            .db()
            .borrow_mut()
            .reconcile_moved_emails(&pending.originals)
        {
            log::error!(
                "could not restore {} moved message(s): {error}",
                pending.originals.len()
            );
        }
        self.clear_move_tombstones(pending, 0);
        self.reload_folders();
        self.refresh_emails(None);
    }

    fn clear_move_tombstones(&self, pending: &PendingMove, start: usize) {
        let mut state = self.state_mut();
        for tombstone in pending.tombstones.iter().skip(start) {
            let Some(entry) = state.move_tombstones.get_mut(tombstone) else {
                continue;
            };
            entry.active -= 1;
            if entry.active <= 0 && entry.awaiting <= 0 {
                state.move_tombstones.remove(tombstone);
            }
        }
    }

    fn await_move_tombstones(&self, pending: &PendingMove, completed: usize) {
        let mut state = self.state_mut();
        for tombstone in pending.tombstones.iter().take(completed) {
            if let Some(entry) = state.move_tombstones.get_mut(tombstone) {
                entry.active -= 1;
                entry.awaiting += 1;
            }
        }
    }

    /// A newest-page sync says which UIDs the source still holds: any
    /// tombstone whose UID is gone has done its job.
    pub(super) fn confirm_move_tombstones(
        &self,
        folder_id: i64,
        all_uids: &std::collections::HashSet<String>,
    ) {
        let mut state = self.state_mut();
        let keys: Vec<(i64, String)> = state.move_tombstones.keys().cloned().collect();
        for key in keys {
            if key.0 != folder_id || all_uids.contains(&key.1) {
                continue;
            }
            let entry = state.move_tombstones.entry(key.clone()).or_default();
            entry.awaiting = 0;
            if entry.active <= 0 {
                state.move_tombstones.remove(&key);
            }
        }
    }

    fn run_move_worker(&self, pending: PendingMove) {
        let job = pending.clone();
        workers::run(
            move || -> Result<MoveResult, Option<String>> {
                let Some(credential) = secrets::credential_for(&job.account) else {
                    log::warn!("could not sign in to account {}", job.account.email);
                    return Err(None);
                };
                sync::move_messages(
                    &job.account,
                    &credential,
                    &job.source.name,
                    &job.uids,
                    &job.dest.name,
                )
                .map_err(|error| {
                    log::error!(
                        "could not move {} message(s) from {} to {} (account {}): {error}",
                        job.uids.len(),
                        job.source.name,
                        job.dest.name,
                        job.account.email
                    );
                    Some(i18n::failure_message(&classify(
                        &error,
                        &job.account.imap_host,
                    )))
                })
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result: Result<MoveResult, Option<String>>| match result {
                    Ok(result) => window.on_move_result(&pending, result),
                    Err(None) => {
                        // The messages were already moved locally, so they have to come back.
                        window.restore_move(&pending);
                        window.toast(&gettext("Could not sign in to this account."));
                    }
                    Err(Some(message)) => {
                        window.restore_move(&pending);
                        window.toast(&i18n::format(
                            &gettext("Move failed: {msg}"),
                            &[("msg", &message)],
                        ));
                    }
                }
            ),
        );
    }

    fn on_move_result(&self, pending: &PendingMove, result: MoveResult) {
        let completed = result.destination_uids.len();
        self.await_move_tombstones(pending, completed);
        let completed_moves: Vec<(i64, i64, Option<String>)> = pending
            .email_ids
            .iter()
            .take(completed)
            .zip(&result.destination_uids)
            .map(|(id, uid)| (*id, pending.dest.id, uid.clone()))
            .collect();
        {
            let db = self.db();
            let mut db = db.borrow_mut();
            if !completed_moves.is_empty() {
                if let Err(error) = db.reconcile_moved_emails(&completed_moves) {
                    log::error!(
                        "could not reconcile {} moved message(s): {error}",
                        completed_moves.len()
                    );
                }
            }
            let failed_index = result.failed_index.or(if completed < pending.uids.len() {
                Some(completed)
            } else {
                None
            });
            if let Some(failed_index) = failed_index {
                if let Err(error) = db.reconcile_moved_emails(&pending.originals[failed_index..]) {
                    log::error!("could not restore the unmoved tail: {error}");
                }
                drop(db);
                self.clear_move_tombstones(pending, failed_index);
            }
        }
        self.reload_folders();
        self.refresh_emails(None);
        if let Some(error) = result.error {
            log::warn!(
                "move stopped after {completed} of {} message(s) from {} to {}: {error}",
                pending.uids.len(),
                pending.source.name,
                pending.dest.name
            );
            self.toast(&i18n::format(
                &gettext("Move failed: {msg}"),
                &[("msg", &error)],
            ));
        }
    }

    /// A menu of every folder of the selection's account except the source,
    /// each targeting `<prefix>.move` with the folder id. In the unified inbox
    /// a selection spanning accounts gets no menu: there is no one place to
    /// move it to.
    pub(super) fn build_move_menu(&self, action_prefix: &str) -> gio::Menu {
        let menu = gio::Menu::new();
        let selected = self.selected_emails();
        let source_ids: std::collections::HashSet<i64> =
            selected.iter().map(|c| c.with(|c| c.folder_id)).collect();
        let account_id = match self.current_folder() {
            Some(folder) => Some(folder.account_id),
            None => {
                let ids: std::collections::HashSet<i64> = source_ids
                    .iter()
                    .filter_map(|id| self.account_for_folder(*id).map(|(a, _)| a.id))
                    .collect();
                if ids.len() == 1 {
                    ids.into_iter().next()
                } else {
                    None
                }
            }
        };
        let Some(account_id) = account_id else {
            return menu;
        };
        let exclude = self.current_folder().map(|f| f.id);
        for folder in self
            .db()
            .borrow()
            .folders_for_account(account_id)
            .unwrap_or_default()
        {
            if Some(folder.id) == exclude || (exclude.is_none() && source_ids.contains(&folder.id))
            {
                continue;
            }
            let label = folders::display_name_for_folder(&folder.name, folder.display_delimiter());
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(
                Some(&format!("{action_prefix}.move")),
                Some(&folder.id.to_variant()),
            );
            menu.append_item(&item);
        }
        menu
    }

    pub(super) fn update_move_menu(&self) {
        self.imp()
            .move_button
            .set_menu_model(Some(&self.build_move_menu("win")));
    }
}

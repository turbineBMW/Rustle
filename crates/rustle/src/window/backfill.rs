//! The whole-mailbox download. A newest-page sync shows the top of a folder;
//! this sweep fills in everything below it, folder by folder, batch by
//! batch, until the database holds every header the server does. It runs
//! beside the ordinary syncs rather than through them: it never takes the
//! account's sync lock, so polls, folder clicks and the spinner behave as
//! if it weren't there.

use super::MainWindow;
use crate::settings as keys;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::folders::{self, FolderRole};
use rustle_core::models::Account;
use rustle_core::net::errors::{classify, Failure};
use rustle_core::secrets;
use rustle_core::sync::{self, BackfillFolder, BackfillResult, BACKFILL_LIMIT};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;

/// Breathing room between batches, so the server and the main loop see a
/// steady trickle rather than a flood.
const BATCH_PAUSE: Duration = Duration::from_millis(500);

/// One account's sweep: the folders still to finish, front one in progress.
#[derive(Debug, Default)]
pub struct Backfill {
    queue: VecDeque<i64>,
}

impl Backfill {
    pub fn has_queued(&self, folder_id: i64) -> bool {
        self.queue.contains(&folder_id)
    }

    /// Folders deleted under the sweep leave it, so a reused row id can't
    /// pull a stranger's mail into a new folder.
    pub fn retain_folders(&mut self, live_ids: &HashSet<i64>) {
        self.queue.retain(|id| live_ids.contains(id));
    }

    fn remove(&mut self, folder_id: i64) {
        self.queue.retain(|id| *id != folder_id);
    }
}

impl MainWindow {
    fn downloads_all_mail(&self) -> bool {
        self.settings().boolean(keys::DOWNLOAD_ALL_MAIL)
    }

    /// The preference flipped: start every account, or let the running
    /// sweeps find their entry gone when their batch returns.
    pub(super) fn start_all_backfills(&self) {
        if !self.downloads_all_mail() {
            self.state_mut().backfills.clear();
            return;
        }
        let accounts: Vec<Account> = self.state().accounts.values().cloned().collect();
        for account in accounts {
            self.start_backfill(&account, None);
        }
    }

    /// Begin a sweep of the account unless one is already running. `first`
    /// is the folder to fill before the inbox: the one the user is looking at.
    pub(super) fn start_backfill(&self, account: &Account, first: Option<i64>) {
        if !self.downloads_all_mail() || !self.state().is_online || self.imp().is_closing.get() {
            return;
        }
        if self.state().backfills.contains_key(&account.id) {
            return;
        }
        let folders = self
            .db()
            .borrow()
            .folders_for_account(account.id)
            .unwrap_or_default();
        let mut queue: VecDeque<i64> = VecDeque::new();
        let mut rest: Vec<(u8, i64)> = Vec::new();
        for folder in folders {
            if folder.name == folders::OUTBOX_FOLDER
                || folders::NAMESPACE_ROOTS.contains(&folder.name.as_str())
            {
                continue;
            }
            if Some(folder.id) == first {
                queue.push_front(folder.id);
                continue;
            }
            let rank = match folders::role_for_folder(&folder.name) {
                FolderRole::Inbox => 0,
                FolderRole::Other => 2,
                _ => 1,
            };
            rest.push((rank, folder.id));
        }
        rest.sort();
        queue.extend(rest.into_iter().map(|(_, id)| id));
        if queue.is_empty() {
            return;
        }
        self.state_mut()
            .backfills
            .insert(account.id, Backfill { queue });
        self.run_backfill_batch(account);
    }

    fn run_backfill_batch(&self, account: &Account) {
        let queue: Vec<i64> = match self.state().backfills.get(&account.id) {
            Some(sweep) => sweep.queue.iter().copied().collect(),
            None => return,
        };
        let folders: Vec<BackfillFolder> = {
            let db = self.db();
            let db = db.borrow();
            queue
                .iter()
                .filter_map(|id| {
                    let folder = db.folder(*id).ok().flatten()?;
                    let local_uids = db.uids_in_folder(*id).unwrap_or_default();
                    Some(BackfillFolder {
                        name: folder.name,
                        local_uids,
                    })
                })
                .collect()
        };
        if folders.is_empty() {
            self.state_mut().backfills.remove(&account.id);
            return;
        }
        let job_account = account.clone();
        workers::run(
            move || backfill_job(&job_account, &folders),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                account,
                move |result: Result<BackfillResult, Failure>| {
                    window.on_backfill_done(&account, result)
                }
            ),
        );
    }

    fn on_backfill_done(&self, account: &Account, result: Result<BackfillResult, Failure>) {
        if self.is_stale(account) || !self.state().backfills.contains_key(&account.id) {
            self.state_mut().backfills.remove(&account.id);
            return;
        }
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                // Logged at the worker boundary. The next ordinary sync
                // starts the sweep again, so nothing is lost by stopping.
                self.state_mut().backfills.remove(&account.id);
                return;
            }
        };
        let keep_id = self.selected_email().map(|c| c.id());
        let mut fetched_folder: Option<i64> = None;
        let mut saved = 0usize;
        {
            let db = self.db();
            let mut db = db.borrow_mut();
            let folder_id = |db: &rustle_core::db::Database, name: &str| {
                db.folder_by_name(account.id, name).ok().flatten()
            };
            let completed: Vec<i64> = result
                .completed
                .iter()
                .filter_map(|name| folder_id(&db, name).map(|f| f.id))
                .collect();
            let target = result
                .folder
                .as_deref()
                .and_then(|name| folder_id(&db, name));
            {
                let mut state = self.state_mut();
                if let Some(sweep) = state.backfills.get_mut(&account.id) {
                    for id in completed {
                        sweep.remove(id);
                    }
                }
            }
            if let Some(target) = target {
                fetched_folder = Some(target.id);
                let tombstoned: HashSet<String> = {
                    let state = self.state();
                    state
                        .move_tombstones
                        .keys()
                        .filter(|(folder_id, _)| *folder_id == target.id)
                        .map(|(_, uid)| uid.clone())
                        .collect()
                };
                for message in &result.messages {
                    if tombstoned.contains(&message.uid) {
                        continue;
                    }
                    match db.save_incoming_email(target.id, message) {
                        Ok(_) => saved += 1,
                        Err(error) => log::error!(
                            "could not store message {} in {}: {error}",
                            message.uid,
                            target.name
                        ),
                    }
                }
                let addresses: Vec<(String, String)> = result
                    .messages
                    .iter()
                    .flat_map(|m| m.addresses.clone())
                    .collect();
                if let Err(error) = db.save_contacts(&addresses) {
                    log::warn!("could not store contacts: {error}");
                }
                log::debug!(
                    "backfilled {saved} message(s) of {} for {}; {} to go",
                    target.name,
                    account.email,
                    result.remaining
                );
                let mut state = self.state_mut();
                // The batches take the newest missing UIDs first, so what's
                // local stays the top of the folder: the scroll path can
                // carry on from here if the sweep stops.
                let loaded = result.exists.saturating_sub(result.remaining);
                state.loaded_counts.insert(target.id, loaded);
                state
                    .folders_with_more_mail
                    .insert(target.id, result.remaining > 0);
                if result.remaining == 0 {
                    if let Some(sweep) = state.backfills.get_mut(&account.id) {
                        sweep.remove(target.id);
                    }
                }
            }
        }
        if saved > 0 {
            if fetched_folder.is_some_and(|id| self.current_folder_ids().contains(&id)) {
                self.refresh_emails(keep_id);
            }
            self.reload_folders();
        }

        let finished = self
            .state()
            .backfills
            .get(&account.id)
            .is_none_or(|sweep| sweep.queue.is_empty());
        if finished {
            self.state_mut().backfills.remove(&account.id);
            log::info!("every folder of {} is downloaded", account.email);
            return;
        }
        if !self.downloads_all_mail() || !self.state().is_online || self.imp().is_closing.get() {
            self.state_mut().backfills.remove(&account.id);
            return;
        }
        glib::timeout_add_local_once(
            BATCH_PAUSE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                account,
                move || {
                    if !window.is_stale(&account) {
                        window.run_backfill_batch(&account);
                    }
                }
            ),
        );
    }
}

/// Runs on the worker thread: network only, no widgets, no database.
fn backfill_job(account: &Account, folders: &[BackfillFolder]) -> Result<BackfillResult, Failure> {
    let Some(credential) = secrets::credential_for(account) else {
        log::warn!("could not sign in to account {}", account.email);
        return Err(Failure::NoCredential);
    };
    sync::backfill(account, &credential, folders, BACKFILL_LIMIT).map_err(|error| {
        log::error!(
            "backfill failed for {} on {} (from folder {}): {error}",
            account.email,
            account.imap_host,
            folders.first().map(|f| f.name.as_str()).unwrap_or("?")
        );
        classify(&error, &account.imap_host)
    })
}

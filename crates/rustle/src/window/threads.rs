//! Filling in conversation keys for mail synced before they were kept. A
//! sweep per account reads just the conversation headers (Message-ID,
//! References, In-Reply-To, Thread-Index) of a few hundred messages at a
//! time, a couple of hundred bytes each, until every message has its keys.
//! New mail arrives with them, so a sweep runs out and stays done.

use super::MainWindow;
use crate::workers;
use gtk::glib;
use rustle_core::models::{Account, Folder};
use rustle_core::secrets;
use rustle_core::sync;
use rustle_core::threads::ThreadKeys;
use std::time::Duration;

/// Messages per batch, and the pause between batches.
const BATCH: u32 = 500;
const PAUSE: Duration = Duration::from_secs(2);

impl MainWindow {
    /// Start the account's sweep unless one is running or offline.
    pub(super) fn sweep_thread_keys(&self, account: &Account) {
        if !self.state().is_online || !self.state_mut().thread_sweeps.insert(account.id) {
            return;
        }
        self.sweep_next(account.clone());
    }

    fn sweep_next(&self, account: Account) {
        let batch = match self.db().borrow().unthreaded_batch(account.id, BATCH) {
            Ok(batch) => batch,
            Err(error) => {
                log::error!(
                    "could not find mail to thread for {}: {error}",
                    account.email
                );
                None
            }
        };
        let Some((folder, uids)) = batch.filter(|(_, uids)| !uids.is_empty()) else {
            self.state_mut().thread_sweeps.remove(&account.id);
            return;
        };
        let job_account = account.clone();
        let job_folder = folder.name.clone();
        let job_uids = uids.clone();
        workers::run(
            move || -> Option<Vec<(String, ThreadKeys)>> {
                let credential = secrets::credential_for(&job_account)?;
                sync::fetch_thread_keys(&job_account, &credential, &job_folder, &job_uids)
                    .map_err(|error| {
                        log::warn!(
                            "could not read the conversation headers of {} message(s) in {} (account {}): {error}",
                            job_uids.len(),
                            job_folder,
                            job_account.email
                        );
                    })
                    .ok()
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |found| window.on_swept(account, folder, uids, found)
            ),
        );
    }

    fn on_swept(
        &self,
        account: Account,
        folder: Folder,
        uids: Vec<u32>,
        found: Option<Vec<(String, ThreadKeys)>>,
    ) {
        // A failed batch ends the sweep; the next sync starts it again.
        let Some(found) = found.filter(|_| !self.is_stale(&account)) else {
            self.state_mut().thread_sweeps.remove(&account.id);
            return;
        };
        if let Err(error) = self
            .db()
            .borrow_mut()
            .set_thread_keys(folder.id, &uids, &found)
        {
            log::error!(
                "could not store conversation keys for {}: {error}",
                folder.name
            );
            self.state_mut().thread_sweeps.remove(&account.id);
            return;
        }
        log::debug!(
            "read the conversation headers of {} message(s) in {} for {}",
            found.len(),
            folder.name,
            account.email
        );
        glib::timeout_add_local_once(
            PAUSE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || window.sweep_next(account)
            ),
        );
    }
}

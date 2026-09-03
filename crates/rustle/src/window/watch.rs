//! Push: one IDLE thread per account, reconciled against the account list.
//! Unlike `workers::run`, the thread lives as long as the account does and
//! reports through a `SendWeakRef` on the window, so it never holds a
//! widget or the database. The poll timer keeps running underneath it.

use super::MainWindow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::models::Account;
use rustle_core::secrets;
use rustle_core::watch::{self, InboxWatch, WatchEnd};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Wait before reconnecting after a failure, doubling up to the cap.
const FIRST_RETRY: Duration = Duration::from_secs(30);
const LONGEST_RETRY: Duration = Duration::from_secs(10 * 60);
/// A connection that held this long earns a fresh backoff.
const STABLE_AFTER: Duration = Duration::from_secs(5 * 60);
/// The server announces one arrival as a burst of lines; collect them.
const SETTLE: Duration = Duration::from_millis(1500);

impl MainWindow {
    /// Start a watch for every account missing one, cancel those whose
    /// account is gone or changed, and all of them offline. Idempotent, so
    /// it runs after every account reload and network flip.
    pub(super) fn sync_inbox_watchers(&self) {
        let wanted: HashMap<i64, Account> = {
            let state = self.state();
            if state.is_online && !self.imp().is_closing.get() {
                state.accounts.clone()
            } else {
                HashMap::new()
            }
        };
        let stale: Vec<Arc<InboxWatch>> = {
            let mut state = self.state_mut();
            let mut stale = Vec::new();
            state.inbox_watchers.retain(|id, (account, watch)| {
                let keep = wanted.get(id).is_some_and(|a| a == account);
                if !keep {
                    stale.push(watch.clone());
                }
                keep
            });
            stale
        };
        for watch in stale {
            watch.cancel();
        }
        let missing: Vec<Account> = {
            let state = self.state();
            wanted
                .into_values()
                .filter(|a| !state.inbox_watchers.contains_key(&a.id))
                .collect()
        };
        for account in missing {
            self.spawn_inbox_watcher(account);
        }
    }

    pub(super) fn stop_inbox_watchers(&self) {
        let watchers: Vec<Arc<InboxWatch>> = self
            .state_mut()
            .inbox_watchers
            .drain()
            .map(|(_, (_, watch))| watch)
            .collect();
        for watch in watchers {
            watch.cancel();
        }
    }

    fn spawn_inbox_watcher(&self, account: Account) {
        let watch = InboxWatch::new();
        self.state_mut()
            .inbox_watchers
            .insert(account.id, (account.clone(), watch.clone()));
        let window: glib::SendWeakRef<MainWindow> = self.downgrade().into();
        let (id, email) = (account.id, account.email.clone());
        let spawned = thread::Builder::new()
            .name(format!("idle-{id}"))
            .spawn(move || watch_thread(&account, &watch, &window));
        if let Err(error) = spawned {
            log::error!("could not start the inbox watch for {email}: {error}");
            self.state_mut().inbox_watchers.remove(&id);
        }
    }

    /// Called on the main loop for every change the server reported.
    fn on_inbox_changed(&self, account_id: i64) {
        if self.imp().is_closing.get() || !self.state().is_online {
            return;
        }
        if let Some(pending) = self.state_mut().inbox_settle.remove(&account_id) {
            pending.remove();
        }
        let source = glib::timeout_add_local_once(
            SETTLE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.state_mut().inbox_settle.remove(&account_id);
                    window.sync_inbox(account_id);
                }
            ),
        );
        self.state_mut().inbox_settle.insert(account_id, source);
    }

    /// Fetch the inbox, or note that it needs fetching again once the sync
    /// already running for the account is done: that one may have listed
    /// the mailbox before the new message landed.
    pub(super) fn sync_inbox(&self, account_id: i64) {
        let account = {
            let mut state = self.state_mut();
            if state.syncing_account_ids.contains(&account_id) {
                state.inbox_resync_pending.insert(account_id);
                return;
            }
            state.accounts.get(&account_id).cloned()
        };
        if let Some(account) = account {
            log::debug!(
                "the server reported a change in the inbox of {}; fetching",
                account.email
            );
            self.start_sync(&account, true, None, 0);
        }
    }
}

/// The whole life of one watch: connect, idle, and on failure wait and try
/// again, until cancelled. Credentials are resolved here, fresh on every
/// connect, so an expired OAuth token is renewed on the way back in.
fn watch_thread(account: &Account, watch: &InboxWatch, window: &glib::SendWeakRef<MainWindow>) {
    let account_id = account.id;
    let mut retry = FIRST_RETRY;
    while !watch.is_cancelled() {
        let started = Instant::now();
        let mut notify = || {
            let window = window.clone();
            glib::MainContext::default().invoke(move || {
                if let Some(window) = window.upgrade() {
                    window.on_inbox_changed(account_id);
                }
            });
        };
        let result = match secrets::credential_for(account) {
            Some(credential) => watch::watch_inbox(account, &credential, watch, &mut notify),
            None => Err(rustle_core::net::errors::NetError::Protocol(
                "no credential".into(),
            )),
        };
        match result {
            Ok(WatchEnd::Cancelled) => break,
            Ok(WatchEnd::Unsupported) => {
                log::info!(
                    "{} has no IDLE; {} relies on the sync timer",
                    account.imap_host,
                    account.email
                );
                break;
            }
            Err(_) if watch.is_cancelled() => break,
            Err(error) => {
                if started.elapsed() > STABLE_AFTER {
                    retry = FIRST_RETRY;
                }
                log::warn!(
                    "inbox watch for {} on {} dropped: {error}; reconnecting in {}s",
                    account.email,
                    account.imap_host,
                    retry.as_secs()
                );
                if watch.wait_cancelled(retry) {
                    break;
                }
                retry = (retry * 2).min(LONGEST_RETRY);
            }
        }
    }
    log::debug!("inbox watch for {} ended", account.email);
}

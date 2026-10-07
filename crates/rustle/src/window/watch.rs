//! Push: one IDLE thread per account on its inbox, reconciled against the
//! account list, and one more on the folder the user has open. Unlike
//! `workers::run`, a thread lives as long as what it watches and reports
//! through a `SendWeakRef` on the window, so it never holds a widget or the
//! database. The poll timer keeps running underneath. After a suspend every
//! socket is dead, so a resume restarts the lot.

use super::MainWindow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::folders::{self, FolderRole};
use rustle_core::models::{Account, Folder};
use rustle_core::net::pool;
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
        self.sync_folder_watcher();
    }

    /// Watch the open folder too, unless it's the inbox (already watched),
    /// the local Outbox, or more than one folder (the unified inbox).
    pub(super) fn sync_folder_watcher(&self) {
        let wanted: Option<(Folder, Account)> = {
            let folder = self.current_folder();
            let state = self.state();
            folder
                .filter(|_| state.is_online && !self.imp().is_closing.get())
                .filter(|folder| {
                    folder.name != folders::OUTBOX_FOLDER
                        && folders::role_for_folder(&folder.name) != FolderRole::Inbox
                })
                .and_then(|folder| {
                    let account = state.accounts.get(&folder.account_id)?.clone();
                    Some((folder, account))
                })
        };
        let current = self.state_mut().folder_watcher.take();
        if let Some((folder, account, watch)) = current {
            let is_same = wanted
                .as_ref()
                .is_some_and(|(f, a)| f.id == folder.id && *a == account);
            if is_same {
                self.state_mut().folder_watcher = Some((folder, account, watch));
                return;
            }
            watch.cancel();
        }
        let Some((folder, account)) = wanted else {
            return;
        };
        let watch = InboxWatch::new();
        self.state_mut().folder_watcher = Some((folder.clone(), account.clone(), watch.clone()));
        let window: glib::SendWeakRef<MainWindow> = self.downgrade().into();
        let folder_id = folder.id;
        let notify = move || {
            let window = window.clone();
            glib::MainContext::default().invoke(move || {
                if let Some(window) = window.upgrade() {
                    window.on_folder_changed(folder_id);
                }
            });
        };
        let name = folder.name.clone();
        let spawned = thread::Builder::new()
            .name(format!("idle-folder-{folder_id}"))
            .spawn(move || watch_thread(&account, Some(&name), &watch, notify));
        if let Err(error) = spawned {
            log::error!("could not start the watch on {}: {error}", folder.name);
            self.state_mut().folder_watcher = None;
        }
    }

    /// The open folder changed on the server: fetch it once the burst
    /// settles, if it's still the one on screen.
    fn on_folder_changed(&self, folder_id: i64) {
        if self.imp().is_closing.get() || !self.state().is_online {
            return;
        }
        if let Some(pending) = self.state_mut().folder_settle.take() {
            pending.remove();
        }
        let source = glib::timeout_add_local_once(
            SETTLE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.state_mut().folder_settle = None;
                    window.sync_open_folder(folder_id);
                }
            ),
        );
        self.state_mut().folder_settle = Some(source);
    }

    /// Fetch the open folder, or come back to it once the account's running
    /// sync is done (see `sync_inbox`).
    pub(super) fn sync_open_folder(&self, folder_id: i64) {
        let Some(folder) = self.current_folder().filter(|f| f.id == folder_id) else {
            return;
        };
        let account = {
            let mut state = self.state_mut();
            if state.syncing_account_ids.contains(&folder.account_id) {
                state.folder_resync_pending = Some(folder_id);
                return;
            }
            state.accounts.get(&folder.account_id).cloned()
        };
        if let Some(account) = account {
            log::debug!("the server reported a change in {}; fetching", folder.name);
            self.start_sync(&account, true, Some(&folder.name), 0);
        }
    }

    /// Listen for logind's resume. A failure here only costs the fast
    /// reconnect: the watches still notice their dead sockets at the next
    /// timeout.
    pub(super) fn watch_for_resume(&self) {
        gio::bus_get(
            gio::BusType::System,
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |bus| {
                    let bus = match bus {
                        Ok(bus) => bus,
                        Err(error) => {
                            log::info!("no system bus, so no reconnect on resume: {error}");
                            return;
                        }
                    };
                    let weak = window.downgrade();
                    let subscription = bus.subscribe_to_signal(
                        Some("org.freedesktop.login1"),
                        Some("org.freedesktop.login1.Manager"),
                        Some("PrepareForSleep"),
                        Some("/org/freedesktop/login1"),
                        None,
                        gio::DBusSignalFlags::NONE,
                        move |signal| {
                            let is_going_to_sleep = signal.parameters.get::<(bool,)>();
                            if let (Some(window), Some((false,))) =
                                (weak.upgrade(), is_going_to_sleep)
                            {
                                window.on_resumed();
                            }
                        },
                    );
                    window.state_mut().resume_subscription = Some(subscription);
                }
            ),
        );
    }

    /// Back from suspend: every connection is dead, so drop the parked ones,
    /// restart the watches and fetch what arrived while asleep.
    fn on_resumed(&self) {
        log::debug!("resumed from suspend; reconnecting");
        pool::forget_all();
        self.stop_inbox_watchers();
        self.sync_inbox_watchers();
        let has_accounts = !self.state().accounts.is_empty();
        if has_accounts && self.state().is_online {
            self.sync_all(true);
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
        if let Some((_, _, watch)) = self.state_mut().folder_watcher.take() {
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
        let notify = move || {
            let window = window.clone();
            glib::MainContext::default().invoke(move || {
                if let Some(window) = window.upgrade() {
                    window.on_inbox_changed(id);
                }
            });
        };
        let spawned = thread::Builder::new()
            .name(format!("idle-{id}"))
            .spawn(move || watch_thread(&account, None, &watch, notify));
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

/// The whole life of one watch on `mailbox` (the inbox when None): connect,
/// idle, and on failure wait and try again, until cancelled. Credentials are
/// resolved here, fresh on every connect, so an expired OAuth token is
/// renewed on the way back in. Every connect after the first reports a
/// change straight away, for whatever arrived while it was down.
fn watch_thread(
    account: &Account,
    mailbox: Option<&str>,
    watch: &InboxWatch,
    mut notify: impl FnMut() + Send,
) {
    let mut retry = FIRST_RETRY;
    let mut is_reconnect = false;
    while !watch.is_cancelled() {
        let started = Instant::now();
        let result = match secrets::credential_for(account) {
            Some(credential) => watch::watch_mailbox(
                account,
                &credential,
                mailbox,
                is_reconnect,
                watch,
                &mut notify,
            ),
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
                is_reconnect = true;
                if started.elapsed() > STABLE_AFTER {
                    retry = FIRST_RETRY;
                }
                log::warn!(
                    "watch on {} for {} on {} dropped: {error}; reconnecting in {}s",
                    mailbox.unwrap_or("the inbox"),
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
    log::debug!(
        "watch on {} for {} ended",
        mailbox.unwrap_or("the inbox"),
        account.email
    );
}

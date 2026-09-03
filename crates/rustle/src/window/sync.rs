//! Syncing, the Outbox, and the connection banner.

use super::{MainWindow, PAGE_EMPTY, PAGE_LOADING};
use crate::config::APP_ID;
use crate::i18n::{self, gettext};
use crate::settings as keys;
use crate::sound;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::compose;
use rustle_core::dates;
use rustle_core::folders;
use rustle_core::models::{Account, MessageHeader};
use rustle_core::net::errors::{classify, linkify, Failure};
use rustle_core::secrets;
use rustle_core::sounds::NotificationSound;
use rustle_core::sync::{self, SyncResult, RECENT_LIMIT};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// One attempted send from the Outbox. `error` is None when it went out.
struct OutboxResult {
    email_id: i64,
    subject: String,
    raw: Vec<u8>,
    error: Option<Failure>,
}

impl MainWindow {
    pub(super) fn drain_outbox(&self, account: &Account) {
        let jobs: Vec<(i64, String, Vec<String>, Vec<u8>)> = {
            let db = self.db();
            let db = db.borrow();
            let Some(outbox) = db
                .folder_by_name(account.id, folders::OUTBOX_FOLDER)
                .ok()
                .flatten()
            else {
                return;
            };
            db.emails_in_folder(outbox.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|mail| {
                    let raw = db.raw_message(mail.id).ok().flatten()?;
                    Some((
                        mail.id,
                        mail.subject,
                        compose::extract_recipients(&raw),
                        raw,
                    ))
                })
                .collect()
        };
        if jobs.is_empty() {
            return;
        }
        let job_account = account.clone();
        workers::run(
            move || outbox_job(&job_account, jobs),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                account,
                move |results: Vec<OutboxResult>| window.on_outbox_drained(&account, results)
            ),
        );
    }

    /// Back on the main thread. Files under the account that sent, not the
    /// open one; dropping a stale one would leave the mail in the Outbox to
    /// go out twice.
    fn on_outbox_drained(&self, account: &Account, results: Vec<OutboxResult>) {
        let mut sent_count = 0u64;
        let mut errors = Vec::new();
        {
            let db = self.db();
            let db = db.borrow();
            for result in results {
                if let Some(error) = result.error {
                    errors.push(error);
                    continue;
                }
                let filed = db.sent_folder(account.id).and_then(|sent| {
                    // extract_recipients keeps only the addresses.
                    let recipient = compose::extract_recipients(&result.raw)
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                    let row = db.save_email(
                        sent.id,
                        &MessageHeader {
                            sender: account.email.clone(),
                            sender_address: account.email.clone(),
                            recipient: recipient.clone(),
                            recipient_address: recipient,
                            subject: result.subject.clone(),
                            preview: result.subject.clone(),
                            date: dates::now_iso(),
                            is_unread: false,
                            ..MessageHeader::default()
                        },
                    )?;
                    db.save_raw_message(row.id, &result.raw)?;
                    db.delete_email(result.email_id)
                });
                match filed {
                    Ok(()) => sent_count += 1,
                    Err(error) => {
                        log::error!("could not file sent message {}: {error}", result.email_id)
                    }
                }
            }
        }
        if sent_count > 0 {
            self.reload_folders();
            self.refresh_conversations(None);
            self.toast(&i18n::plural(
                "Sent {n} queued message.",
                "Sent {n} queued messages.",
                sent_count,
                &[],
            ));
        }
        // Queued mail that could not be sent is still in the Outbox, so say so.
        if let Some(first) = errors.first() {
            let message = i18n::plural(
                "Couldn't send a queued message. {reason}",
                "Couldn't send {n} queued messages. {reason}",
                errors.len() as u64,
                &[("reason", &i18n::failure_message(first))],
            );
            self.show_connection_banner(&message, &retry_button_label(first.is_auth()));
        }
    }

    pub(super) fn start_sync(
        &self,
        account: &Account,
        in_background: bool,
        folder_name: Option<&str>,
        offset: u32,
    ) {
        // Don't pile background syncs (folder clicks, the poll timer) on top
        // of one already running for the same account.
        if in_background && self.state().syncing_account_ids.contains(&account.id) {
            return;
        }
        self.set_syncing(account.id, true);
        let stack = &self.imp().conversation_stack;
        if stack.visible_child_name().as_deref() == Some(PAGE_EMPTY) {
            stack.set_visible_child_name(PAGE_LOADING);
        }
        let job_account = account.clone();
        let job_folder = folder_name.map(str::to_string);
        workers::run(
            move || sync_job(&job_account, job_folder.as_deref(), offset),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                account,
                move |result: Result<SyncResult, Failure>| match result {
                    Ok(result) => window.on_sync_done(&account, result),
                    Err(failure) => window.on_sync_error(&account, &failure),
                }
            ),
        );
    }

    /// Send and fetch for every account. The open folder is the one folder
    /// worth naming; the rest get their inbox.
    pub(super) fn sync_all(&self, in_background: bool) {
        let open_folder = self.current_folder();
        let accounts: Vec<Account> = {
            let state = self.state();
            let mut accounts: Vec<Account> = state.accounts.values().cloned().collect();
            accounts.sort_by_key(|a| a.id);
            accounts
        };
        for account in accounts {
            self.drain_outbox(&account);
            let folder_name = open_folder
                .as_ref()
                .filter(|f| f.account_id == account.id)
                .map(|f| f.name.clone());
            self.start_sync(&account, in_background, folder_name.as_deref(), 0);
        }
    }

    /// Refresh on a timer using the configured interval (0 = manual only).
    pub(super) fn reschedule_sync(&self) {
        if let Some(timer) = self.state_mut().sync_timer.take() {
            timer.remove();
        }
        let minutes = self.settings().int(keys::SYNC_INTERVAL);
        if minutes <= 0 {
            return;
        }
        let timer = glib::timeout_add_seconds_local(
            minutes as u32 * 60,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if !window.state().accounts.is_empty() && window.state().is_online {
                        window.sync_all(true);
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );
        self.state_mut().sync_timer = Some(timer);
    }

    /// A sync still in flight when its account was deleted: filing its mail
    /// would recreate the folders that went with it.
    fn is_stale(&self, account: &Account) -> bool {
        !self.state().accounts.contains_key(&account.id)
    }

    fn on_sync_done(&self, account: &Account, result: SyncResult) {
        // Before the staleness check: a dropped callback still has to release
        // the spinner and the Refresh button.
        self.set_syncing(account.id, false);
        if self.is_stale(account) {
            return;
        }
        // Remember the open conversation so a background poll doesn't yank it.
        let keep_id = self.selected_conversation().map(|c| c.id());

        let mut new_messages: Vec<MessageHeader> = Vec::new();
        let target_id;
        {
            let db = self.db();
            let mut db = db.borrow_mut();
            let mut mailboxes: Vec<_> = result
                .folders
                .iter()
                .filter(|m| !folders::NAMESPACE_ROOTS.contains(&m.name.as_str()))
                .collect();
            // Shortest name first: a parent's name is a prefix of its
            // children's, so every parent is stored before a child looks it up.
            mailboxes.sort_by_key(|m| m.name.len());
            for mailbox in &mailboxes {
                let icon = if mailbox.is_selectable {
                    folders::icon_for_folder(&mailbox.name)
                } else {
                    "folder-symbolic"
                };
                let folder = match db.get_or_create_folder(account.id, &mailbox.name, icon) {
                    Ok(folder) => folder,
                    Err(error) => {
                        log::error!("could not store folder {}: {error}", mailbox.name);
                        continue;
                    }
                };
                let parent_name = folders::parent_mailbox_name(&mailbox.name, &mailbox.delimiter);
                let parent = if parent_name.is_empty() {
                    None
                } else {
                    db.folder_by_name(account.id, &parent_name).ok().flatten()
                };
                if let Err(error) =
                    db.set_folder_parent(folder.id, parent.map(|p| p.id), &mailbox.delimiter)
                {
                    log::error!("could not nest folder {}: {error}", mailbox.name);
                }
            }
            if !mailboxes.is_empty() {
                // Mirror the server's folder list, keeping only the local Outbox.
                let mut names: HashSet<String> = mailboxes.iter().map(|m| m.name.clone()).collect();
                names.insert(folders::OUTBOX_FOLDER.to_string());
                if let Err(error) = db.prune_folders(account.id, &names) {
                    log::error!("could not prune folders of {}: {error}", account.email);
                }
            }

            let target = match db.get_or_create_folder(
                account.id,
                &result.folder,
                folders::icon_for_folder(&result.folder),
            ) {
                Ok(folder) => folder,
                Err(error) => {
                    log::error!("could not store folder {}: {error}", result.folder);
                    return;
                }
            };
            target_id = target.id;
            let notify_folder = folders::notifies_on_arrival(&target.name);
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
                    Ok(true) if message.is_unread && notify_folder => {
                        new_messages.push(message.clone())
                    }
                    Ok(_) => {}
                    Err(error) => log::error!(
                        "could not store message {} in {}: {error}",
                        message.uid,
                        target.name
                    ),
                }
            }
            if let Some(all_uids) = &result.all_uids {
                if let Err(error) = db.prune_stale_emails(target.id, all_uids) {
                    log::error!("could not prune {}: {error}", target.name);
                }
            }
            if let Err(error) = db.reassign_conversations(target.id) {
                log::error!("could not thread {}: {error}", target.name);
            }
            // From every fetched header, not just the newly added ones, so an
            // existing install fills its contacts on the next sync.
            let addresses: Vec<(String, String)> = result
                .messages
                .iter()
                .flat_map(|m| m.addresses.clone())
                .collect();
            if let Err(error) = db.save_contacts(&addresses) {
                log::warn!("could not store contacts: {error}");
            }
        }
        if let Some(all_uids) = &result.all_uids {
            // After filtering the fetched headers: an older snapshot can still
            // contain a UID that the authoritative set says has left.
            self.confirm_move_tombstones(target_id, all_uids);
        }

        // Update paging state: track the deepest page loaded (max, so a
        // newest-page poll never forgets how far the user scrolled back),
        // and offer "more" only while messages remain beyond it.
        {
            let mut state = self.state_mut();
            let reached = result.offset + result.messages.len() as u32;
            let loaded = result.exists.min(
                state
                    .loaded_counts
                    .get(&target_id)
                    .copied()
                    .unwrap_or(0)
                    .max(reached),
            );
            state.loaded_counts.insert(target_id, loaded);
            state
                .folders_with_more_mail
                .insert(target_id, result.exists > loaded);
            state.folder_sync_times.insert(target_id, Instant::now());
        }
        let arrived_elsewhere = self.apply_unread_counts(account, &result.unread_counts);

        self.reload_folders();
        self.refresh_conversations(keep_id);
        self.imp().connection_banner.set_revealed(false);
        self.notify_arrivals(account.id, &new_messages, target_id, &arrived_elsewhere);
    }

    /// The app icon, set explicitly so notification daemons that don't
    /// resolve the desktop file's icon still show the envelope.
    fn app_icon() -> gio::ThemedIcon {
        gio::ThemedIcon::new(APP_ID)
    }

    /// Only nag about new mail when the user isn't already looking.
    fn notify_arrivals(
        &self,
        account_id: i64,
        messages: &[MessageHeader],
        folder_id: i64,
        arrived_elsewhere: &HashMap<String, u32>,
    ) {
        if self.is_active() || !self.settings().boolean(keys::NOTIFICATIONS) {
            return;
        }
        if messages.is_empty() && arrived_elsewhere.is_empty() {
            return;
        }
        if !messages.is_empty() {
            self.notify_new_mail(account_id, messages, folder_id);
        }
        if !arrived_elsewhere.is_empty() {
            self.notify_unread_elsewhere(account_id, arrived_elsewhere);
        }
        self.play_new_mail_sound(account_id);
    }

    /// The account's sound, or the app default when it hasn't picked one.
    /// One note per account per sync, alongside its notifications.
    fn play_new_mail_sound(&self, account_id: i64) {
        let account_choice = self
            .db()
            .borrow()
            .account(account_id)
            .ok()
            .flatten()
            .map(|account| NotificationSound::parse(&account.notification_sound))
            .unwrap_or(NotificationSound::Inherit);
        let default = sound::default_sound(&self.settings());
        sound::play(&NotificationSound::resolve(&account_choice, &default));
    }

    /// Store the server's unread counts; return how many arrived per folder.
    /// A folder we have no earlier count for is only recorded -- on the first
    /// sync every count would otherwise read as new mail.
    fn apply_unread_counts(
        &self,
        account: &Account,
        counts: &HashMap<String, u32>,
    ) -> HashMap<String, u32> {
        let mut arrived = HashMap::new();
        let db = self.db();
        let db = db.borrow();
        let mut state = self.state_mut();
        for (name, count) in counts {
            let Some(folder) = db.folder_by_name(account.id, name).ok().flatten() else {
                continue;
            };
            if let Some(previous) = state.remote_unread_counts.get(&folder.id) {
                if count > previous && folders::notifies_on_arrival(name) {
                    arrived.insert(
                        folders::display_name_for_folder(name, None),
                        count - previous,
                    );
                }
            }
            state.remote_unread_counts.insert(folder.id, *count);
        }
        arrived
    }

    /// New mail in a folder we didn't fetch, so there are no headers to name
    /// -- only the count and where it landed.
    fn notify_unread_elsewhere(&self, account_id: i64, arrived: &HashMap<String, u32>) {
        if !self.settings().boolean(keys::NOTIFICATIONS) {
            return;
        }
        let Some(app) = self.application() else {
            return;
        };
        let total: u32 = arrived.values().sum();
        let notification = gio::Notification::new(&i18n::plural(
            "{n} new message",
            "{n} new messages",
            total as u64,
            &[],
        ));
        let mut names: Vec<&String> = arrived.keys().collect();
        names.sort();
        notification.set_body(Some(
            &names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ));
        notification.set_default_action("app.focus-mail");
        notification.set_icon(&Self::app_icon());
        // Notification ids carry the account: every account syncs on the
        // same tick, and a repeated id replaces the one already on screen.
        app.send_notification(
            Some(&format!("new-mail-elsewhere-{account_id}")),
            &notification,
        );
    }

    fn notify_new_mail(&self, account_id: i64, messages: &[MessageHeader], folder_id: i64) {
        if !self.settings().boolean(keys::NOTIFICATIONS) {
            return;
        }
        let Some(app) = self.application() else {
            return;
        };
        let notification = if messages.len() == 1 {
            let notification = gio::Notification::new(&messages[0].sender);
            notification.set_body(Some(&messages[0].subject));
            // Clicking it opens that thread, which is what marks it read.
            notification.set_default_action_and_target_value(
                "app.open-mail",
                Some(&(folder_id, messages[0].uid.clone()).to_variant()),
            );
            notification
        } else {
            let mut senders: Vec<&str> = Vec::new();
            for message in messages {
                if !senders.contains(&message.sender.as_str()) {
                    senders.push(&message.sender);
                }
            }
            let notification = gio::Notification::new(&i18n::plural(
                "{n} new message",
                "{n} new messages",
                messages.len() as u64,
                &[],
            ));
            notification.set_body(Some(&senders.join(", ")));
            notification.set_default_action("app.focus-mail");
            notification
        };
        notification.set_icon(&Self::app_icon());
        app.send_notification(Some(&format!("new-mail-{account_id}")), &notification);
    }

    fn on_sync_error(&self, account: &Account, failure: &Failure) {
        self.set_syncing(account.id, false);
        // Another account's failure can leave the open folder on the spinner.
        self.show_list_or_placeholder();
        if self.is_stale(account) {
            return;
        }
        self.show_connection_banner(
            &i18n::failure_message(failure),
            &retry_button_label(failure.is_auth()),
        );
    }

    fn show_connection_banner(&self, title: &str, button_label: &str) {
        let banner = &self.imp().connection_banner;
        banner.set_title(&linkify(title));
        banner.set_button_label(if button_label.is_empty() {
            None
        } else {
            Some(button_label)
        });
        banner.set_revealed(true);
    }

    pub(super) fn show_offline_banner(&self) {
        self.show_connection_banner(
            &gettext("You're offline. Rustle will reconnect when your connection returns."),
            "",
        );
    }

    pub(super) fn on_banner_retry(&self) {
        self.imp().connection_banner.set_revealed(false);
        if !self.state().accounts.is_empty() {
            self.sync_all(false);
        }
    }

    /// network-changed fires on any change; act only on real online/offline flips.
    pub(super) fn on_network_changed(&self, is_available: bool) {
        if is_available == self.state().is_online {
            return;
        }
        self.state_mut().is_online = is_available;
        self.sync_inbox_watchers();
        if !is_available {
            self.show_offline_banner();
            return;
        }
        self.imp().connection_banner.set_revealed(false);
        if !self.state().accounts.is_empty() {
            self.sync_all(false);
        }
    }

    /// Once per install, not once per close: the notice explains why the app
    /// is still around the first time it happens, and is noise after that.
    pub(super) fn notify_background(&self) {
        let settings = self.settings();
        if settings.boolean(keys::BACKGROUND_NOTICE_SHOWN) {
            return;
        }
        let _ = settings.set_boolean(keys::BACKGROUND_NOTICE_SHOWN, true);
        let Some(app) = self.application() else {
            return;
        };
        let notification = gio::Notification::new(&gettext("Rustle is running in the background"));
        notification.set_body(Some(&gettext(
            "It will keep checking for new mail. Quit to stop.",
        )));
        notification.set_default_action("app.focus-mail");
        notification.set_icon(&Self::app_icon());
        app.send_notification(Some("running-background"), &notification);
    }

    fn set_syncing(&self, account_id: i64, is_syncing: bool) {
        let (row, any_syncing) = {
            let mut state = self.state_mut();
            if is_syncing {
                state.syncing_account_ids.insert(account_id);
            } else {
                state.syncing_account_ids.remove(&account_id);
            }
            (
                state.account_rows.get(&account_id).cloned(),
                !state.syncing_account_ids.is_empty(),
            )
        };
        if let Some(row) = row {
            row.set_syncing(is_syncing);
        }
        self.imp().refresh_button.set_sensitive(!any_syncing);
        if !is_syncing && self.state_mut().inbox_resync_pending.remove(&account_id) {
            self.sync_inbox(account_id);
        }
    }

    /// Each account row spins on its own, but the conversation list only
    /// waits on the accounts whose folders are open.
    pub(super) fn is_current_account_syncing(&self) -> bool {
        let folders = self.current_folders();
        let state = self.state();
        folders
            .iter()
            .any(|folder| state.syncing_account_ids.contains(&folder.account_id))
    }
}

fn retry_button_label(is_auth_failure: bool) -> String {
    // Auth failures aren't worth a Retry button (same password); everything
    // else is a transient connection problem the user can retry.
    if is_auth_failure {
        String::new()
    } else {
        gettext("Retry")
    }
}

/// Runs on the worker thread: network only, no widgets, no database.
fn sync_job(
    account: &Account,
    folder_name: Option<&str>,
    offset: u32,
) -> Result<SyncResult, Failure> {
    let Some(credential) = secrets::credential_for(account) else {
        log::warn!("could not sign in to account {}", account.email);
        return Err(Failure::NoCredential);
    };
    sync::fetch_mailbox(account, &credential, folder_name, RECENT_LIMIT, offset).map_err(|error| {
        log::error!(
            "sync failed for {} on {} (folder {}, offset {offset}): {error}",
            account.email,
            account.imap_host,
            folder_name.unwrap_or("inbox")
        );
        classify(&error, &account.imap_host)
    })
}

/// Runs on the worker thread. Failures travel back as the classified error:
/// the mail stays in the Outbox, so the user has to be told why.
fn outbox_job(
    account: &Account,
    jobs: Vec<(i64, String, Vec<String>, Vec<u8>)>,
) -> Vec<OutboxResult> {
    let Some(credential) = secrets::credential_for(account) else {
        log::warn!(
            "could not sign in to {}; the Outbox stays queued",
            account.email
        );
        return Vec::new();
    };
    jobs.into_iter()
        .map(|(email_id, subject, recipients, raw)| {
            let error = sync::send_message(account, &credential, &account.email, &recipients, &raw)
                .err()
                .map(|error| {
                    log::error!(
                    "could not send queued message {email_id} ({subject:?}) to {} via {}: {error}",
                    recipients.join(", "),
                    account.smtp_host
                );
                    classify(&error, &account.smtp_host)
                });
            OutboxResult {
                email_id,
                subject,
                raw,
                error,
            }
        })
        .collect()
}

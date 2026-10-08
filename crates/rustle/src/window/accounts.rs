//! Accounts, and what the composer opens with (window/composer.rs hosts it).

use super::{MainWindow, PAGE_NO_ACCOUNT};
use crate::composer::Draft;
use crate::dialogs::accounts::AccountsDialog;
use crate::dialogs::add_account::AddAccountDialog;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::address;
use rustle_core::compose;
use rustle_core::eds;
use rustle_core::mime::{self, ParsedMessage};
use rustle_core::models::Account;

/// How long registry signals are left to settle before accounts are re-read:
/// one account arrives as several sources.
const EDS_SETTLE_MS: u64 = 500;

impl MainWindow {
    /// Re-read accounts from Evolution Data Server whenever its registry
    /// changes, and once now.
    pub(super) fn watch_eds(&self) {
        let bus = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
            Ok(bus) => bus,
            Err(error) => {
                log::warn!("no session bus to read accounts from: {error}");
                return;
            }
        };
        let window = self.downgrade();
        let subscription = bus.subscribe_to_signal(
            Some(eds::BUS_NAME),
            None,
            None,
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                // Sources announce themselves, vanish, and rewrite their
                // key files (Data); status chatter is not an account change.
                let is_change = match signal.signal_name {
                    "InterfacesAdded" | "InterfacesRemoved" => true,
                    "PropertiesChanged" => {
                        signal.parameters.n_children() > 1
                            && glib::VariantDict::new(Some(&signal.parameters.child_value(1)))
                                .contains("Data")
                    }
                    _ => false,
                };
                if let (true, Some(window)) = (is_change, window.upgrade()) {
                    window.schedule_eds_refresh();
                }
            },
        );
        self.imp().eds_subscription.replace(Some(subscription));
        self.refresh_accounts_from_eds();
    }

    fn schedule_eds_refresh(&self) {
        if let Some(pending) = self.imp().eds_refresh.take() {
            pending.remove();
        }
        let window = self.downgrade();
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(EDS_SETTLE_MS),
            move || {
                if let Some(window) = window.upgrade() {
                    window.imp().eds_refresh.take();
                    window.refresh_accounts_from_eds();
                }
            },
        );
        self.imp().eds_refresh.replace(Some(source));
    }

    /// Bring the accounts in line with EDS. Accounts Rustle still kept
    /// itself are moved there first, password and all.
    pub(super) fn refresh_accounts_from_eds(&self) {
        let outside = self
            .db()
            .borrow()
            .accounts_outside_eds()
            .unwrap_or_default();
        workers::run(
            move || {
                let mut accounts = eds::mail_accounts().map_err(|error| error.to_string())?;
                for account in outside {
                    let is_in_eds = accounts.iter().any(|found| {
                        (!account.goa_id.is_empty() && found.goa_id == account.goa_id)
                            || (found.email.eq_ignore_ascii_case(&account.email)
                                && found.imap.host.eq_ignore_ascii_case(&account.imap_host))
                    });
                    // Online Accounts ones are EDS's to create.
                    if is_in_eds || !account.goa_id.is_empty() {
                        continue;
                    }
                    match move_to_eds(&account) {
                        Ok(updated) => accounts = updated,
                        Err(error) => log::error!(
                            "could not move account {} to Evolution Data Server: {error}",
                            account.email
                        ),
                    }
                }
                Ok::<_, String>(accounts)
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result: Result<Vec<eds::MailAccount>, String>| {
                    let accounts = match result {
                        Ok(accounts) => accounts,
                        Err(error) => {
                            log::warn!(
                                "could not read accounts from Evolution Data Server: {error}"
                            );
                            return;
                        }
                    };
                    let reconciled = match window.db().borrow_mut().reconcile_eds(&accounts) {
                        Ok(reconciled) => reconciled,
                        Err(error) => {
                            log::error!(
                                "could not update accounts from Evolution Data Server: {error}"
                            );
                            return;
                        }
                    };
                    if !reconciled.is_changed {
                        return;
                    }
                    let had_view = window.state().view.is_some();
                    window.reload_accounts();
                    // A first view syncs everything itself.
                    if had_view {
                        let added: Vec<Account> = reconciled
                            .added
                            .iter()
                            .filter_map(|id| window.db().borrow().account(*id).ok().flatten())
                            .collect();
                        for account in added {
                            window.start_sync(&account, true, None, 0);
                        }
                    }
                }
            ),
        );
    }

    pub(super) fn on_manage_accounts(&self) {
        let dialog = AccountsDialog::new(self.db(), &self.settings());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_account_added()
        ));
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.reload_accounts()
        ));
        dialog.present(Some(self));
    }

    /// Re-read accounts after they change. `reload_folders` picks a new
    /// folder if the open one went with a deleted account.
    pub fn reload_accounts(&self) {
        let has_accounts = self
            .db()
            .borrow()
            .accounts()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if !has_accounts {
            {
                let mut state = self.state_mut();
                state.accounts.clear();
                state.view = None;
            }
            self.sync_inbox_watchers();
            self.imp()
                .main_stack
                .set_visible_child_name(PAGE_NO_ACCOUNT);
            return;
        }
        if self.state().view.is_none() {
            self.load_mail_view();
            return;
        }
        self.reload_folders();
    }

    /// The welcome page's button; with accounts, adding goes through
    /// Manage Accounts.
    pub(super) fn on_add_account_clicked(&self) {
        let dialog = AddAccountDialog::new(self.db());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_account_added()
        ));
        dialog.present(Some(self));
    }

    fn on_account_added(&self) {
        // The first account has no mail view yet; a later one only adds a
        // branch to the sidebar, so the open folder is left alone.
        if self.state().view.is_none() {
            self.load_mail_view();
            return;
        }
        self.reload_folders();
        // A new account, or one shown again, has no folders yet.
        let fresh: Vec<Account> = {
            let db = self.db();
            let db = db.borrow();
            db.accounts()
                .unwrap_or_default()
                .into_iter()
                .filter(|account| {
                    db.folders_for_account(account.id)
                        .is_ok_and(|folders| folders.is_empty())
                })
                .collect()
        };
        for account in fresh {
            self.start_sync(&account, true, None, 0);
        }
    }

    /// The account a new message is written from: the one owning the
    /// selected email, else the open folder's, else the first.
    pub(super) fn compose_account(&self) -> Option<Account> {
        if let Some(selected) = self.selected_email() {
            if let Some((account, _)) = self.account_for_folder(selected.with(|c| c.folder_id)) {
                return Some(account);
            }
        }
        if let Some(folder) = self.current_folder() {
            let account = self.state().accounts.get(&folder.account_id).cloned();
            if account.is_some() {
                return account;
            }
        }
        let state = self.state();
        let mut ids: Vec<&i64> = state.accounts.keys().collect();
        ids.sort();
        ids.first().and_then(|id| state.accounts.get(id).cloned())
    }

    pub(super) fn on_compose_clicked(&self) {
        let Some(account) = self.compose_account() else {
            return;
        };
        let signature = account.signature_html();
        let body_html = if signature.is_empty() {
            String::new()
        } else {
            compose::signature_block(&signature)
        };
        self.open_composer(
            &account,
            Draft {
                body_html,
                ..Draft::default()
            },
        );
    }

    pub(super) fn open_reply(&self, should_reply_all: bool) {
        let Some((view, parsed)) = self.active_parsed() else {
            return;
        };
        let _ = view;
        let Some(account) = self.compose_account() else {
            return;
        };
        // Reply-To wins over From: it is how a sender asks for replies elsewhere.
        let reply_target = if parsed.reply_to_header.trim().is_empty() {
            &parsed.from_header
        } else {
            &parsed.reply_to_header
        };
        let to = address::first_address(reply_target);
        // ponytail: quotes are flattened to text; inlining the original's real
        // HTML would need a sanitizer, since the composer runs with JavaScript
        // enabled.
        let body_html = compose::quote_reply_body(
            &parsed.from_header,
            &parsed.date_header,
            &mime::readable_text(&parsed),
            &account.signature_html(),
        );
        let cc = if should_reply_all {
            compose::reply_all_cc(
                &parsed.to.join(", "),
                &parsed.cc.join(", "),
                &account.email,
                &to,
            )
        } else {
            String::new()
        };
        self.open_composer(
            &account,
            Draft {
                to,
                cc,
                subject: compose::reply_subject(&parsed.subject),
                body_html,
                original_people: [&parsed.from_header, &parsed.reply_to_header]
                    .into_iter()
                    .chain(&parsed.to)
                    .chain(&parsed.cc)
                    .flat_map(|text| rustle_core::address::parse_list(text))
                    .map(|mailbox| mailbox.address)
                    .collect(),
                original_attachments: parsed.attachments.clone(),
                ..Draft::default()
            },
        );
    }

    pub(super) fn open_forward(&self) {
        let Some((_, parsed)) = self.active_parsed() else {
            return;
        };
        let Some(account) = self.compose_account() else {
            return;
        };
        let body_html = compose::forward_body(
            &parsed.from_header,
            &parsed.date_header,
            &parsed.subject,
            &mime::readable_text(&parsed),
            &account.signature_html(),
        );
        self.open_composer(
            &account,
            Draft {
                subject: compose::forward_subject(&parsed.subject),
                body_html,
                // A forward passes the whole message on, files included.
                attachments: parsed.attachments.clone(),
                ..Draft::default()
            },
        );
    }

    /// The rendered selected email, if it
    /// has finished loading.
    fn active_parsed(&self) -> Option<(crate::widgets::message_view::MessageView, ParsedMessage)> {
        if self.selected_emails().len() != 1 {
            return None;
        }
        let view = self.state().message_view.clone()?;
        let parsed = view.parsed()?;
        Some((view, parsed))
    }
}

/// Move an account Rustle kept itself into EDS, with its password. Runs on a
/// worker; returns the registry's accounts once the new one is listed.
fn move_to_eds(account: &Account) -> Result<Vec<eds::MailAccount>, String> {
    let password = rustle_core::secrets::legacy_password(account.id)
        .map_err(|error| format!("could not read its password: {error}"))?
        .ok_or("it has no password in the keyring")?;
    let accounts =
        eds::create_password_account(&eds::NewMailAccount::from_account(account), &password)?;
    if let Err(error) = rustle_core::secrets::clear_legacy_password(account.id) {
        log::warn!(
            "could not remove the old keyring entry of {}: {error}",
            account.email
        );
    }
    log::info!("moved account {} to Evolution Data Server", account.email);
    Ok(accounts)
}

//! Accounts, and opening the composer.

use super::{MainWindow, PAGE_NO_ACCOUNT};
use crate::composer::{ComposerWindow, Draft};
use crate::dialogs::account::AccountDialog;
use crate::dialogs::accounts::AccountsDialog;
use crate::dialogs::online_accounts::OnlineAccountsDialog;
use crate::settings;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::address;
use rustle_core::compose;
use rustle_core::mime::ParsedMessage;
use rustle_core::models::Account;

/// ponytail: quotes are flattened to text; inlining the original's real HTML
/// would need a sanitizer, since the composer runs with JavaScript enabled.
fn original_text(parsed: Option<&ParsedMessage>) -> String {
    match parsed {
        None => String::new(),
        Some(parsed) => parsed.text_body.clone().unwrap_or_else(|| {
            rustle_core::html::html_to_text(parsed.html_body.as_deref().unwrap_or(""))
        }),
    }
}

impl MainWindow {
    pub(super) fn on_manage_accounts(&self) {
        let dialog = AccountsDialog::new(self.db());
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

    pub(super) fn on_add_account_clicked(&self) {
        let dialog = AccountDialog::new(self.db());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_account_added()
        ));
        dialog.present(Some(self));
    }

    pub(super) fn on_online_accounts_clicked(&self) {
        let dialog = OnlineAccountsDialog::new(self.db());
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
        // Highest id sorts last, so this is the one just added.
        let newest = self
            .db()
            .borrow()
            .accounts()
            .ok()
            .and_then(|a| a.last().cloned());
        if let Some(account) = newest {
            self.start_sync(&account, true, None, 0);
        }
    }

    fn signature_text(&self) -> String {
        settings::signature_text(&self.settings())
    }

    /// The account a new message is written from: the one owning the
    /// selected conversation, else the open folder's, else the first.
    fn compose_account(&self) -> Option<Account> {
        if let Some(selected) = self.selected_conversation() {
            if let Some((account, _)) = self.account_for_folder(selected.with(|c| c.folder_id())) {
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
        let signature = self.signature_text();
        let body_html = if signature.is_empty() {
            String::new()
        } else {
            compose::signature_block(&signature)
        };
        self.open_composer(Draft {
            body_html,
            ..Draft::default()
        });
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
        let body_html = compose::quote_reply_body(
            &parsed.from_header,
            &parsed.date_header,
            &original_text(Some(&parsed)),
            &self.signature_text(),
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
        self.open_composer(Draft {
            to,
            cc,
            subject: compose::reply_subject(&parsed.subject),
            body_html,
            ..Draft::default()
        });
    }

    pub(super) fn open_forward(&self) {
        let Some((_, parsed)) = self.active_parsed() else {
            return;
        };
        let body_html = compose::forward_body(
            &parsed.from_header,
            &parsed.date_header,
            &parsed.subject,
            &original_text(Some(&parsed)),
            &self.signature_text(),
        );
        self.open_composer(Draft {
            subject: compose::forward_subject(&parsed.subject),
            body_html,
            ..Draft::default()
        });
    }

    /// The rendered newest message of the one selected conversation, if it
    /// has finished loading.
    fn active_parsed(&self) -> Option<(crate::widgets::message_view::MessageView, ParsedMessage)> {
        if self.selected_conversations().len() != 1 {
            return None;
        }
        let view = self.state().active_view.clone()?;
        let parsed = view.parsed()?;
        Some((view, parsed))
    }

    /// Open the composer for a mailto: link handed to us by the desktop.
    pub fn open_mailto(&self, uri: &str) {
        let Some(account) = self.compose_account() else {
            return;
        };
        let composer = ComposerWindow::for_mailto(
            self.application().as_ref(),
            self.db(),
            &account,
            &self.settings(),
            uri,
        );
        composer.connect_finished(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_composer_finished()
        ));
        composer.present();
    }

    fn open_composer(&self, draft: Draft) {
        let Some(account) = self.compose_account() else {
            return;
        };
        let composer = ComposerWindow::new(self.application().as_ref(), self.db(), &account, draft);
        composer.connect_finished(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_composer_finished()
        ));
        composer.present();
    }

    fn on_composer_finished(&self) {
        let keep_id = self.selected_conversation().map(|c| c.id());
        self.reload_folders();
        self.refresh_conversations(keep_id);
        let accounts: Vec<Account> = self.state().accounts.values().cloned().collect();
        for account in accounts {
            self.drain_outbox(&account);
        }
    }
}

//! The composer in the reader pane. It takes the pane's place until it is
//! sent or cancelled; its pop-out button moves it to a window of its own,
//! and so does picking something else to read. On a phone it always opens
//! in a window, as it did before.

use super::{MainWindow, PAGE_COMPOSER, PAGE_READER};
use crate::composer::{self, Composer, Draft, ResumedDraft};
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::folders::{self, FolderRole};
use rustle_core::models::Account;
use rustle_core::{compose, html, mime};
use std::collections::HashMap;

/// Window actions whose accelerators an editor needs for itself (Ctrl+I is
/// italic, Ctrl+Delete deletes a word...). Application accelerators run in
/// the capture phase, ahead of the focused web view, so these are disabled
/// while the inline composer has the focus; a disabled action lets the key
/// through.
const HELD_ACTIONS: [&str; 9] = [
    "toggle-read",
    "toggle-star",
    "toggle-pin",
    "archive",
    "trash",
    "reply",
    "reply-all",
    "forward",
    "search",
];

impl InlineComposer {
    pub fn widget(&self) -> gtk::Widget {
        self.composer.clone().upcast()
    }
}

pub struct InlineComposer {
    composer: Composer,
    /// The emails selected when it opened; a different selection means
    /// there is something else to read.
    selection: Vec<i64>,
    /// Whether the collapsed split showed the reader before, so closing
    /// goes back to the list if that is where it opened from.
    showed_content: bool,
}

impl MainWindow {
    pub(super) fn setup_inline_composer(&self) {
        self.connect_focus_widget_notify(|window| window.on_focus_moved());
        // Enter or a double-click on a draft opens it to finish.
        self.imp().email_list.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| window.edit_draft()
        ));
    }

    /// Whether a folder holds the account's drafts.
    pub(super) fn is_in_drafts(&self, folder_id: i64) -> bool {
        let folder = self.db().borrow().folder(folder_id).ok().flatten();
        folder.is_some_and(|folder| folders::role_for_folder(&folder.name) == FolderRole::Drafts)
    }

    /// Open the selected draft in the composer, to finish. Saving it again
    /// replaces it; sending or deleting it removes it.
    pub(super) fn edit_draft(&self) {
        let Some(email) = self.selected_email().map(|email| email.get()) else {
            return;
        };
        if self.selected_emails().len() != 1 || !self.is_in_drafts(email.folder_id) {
            return;
        }
        let Some((account, _)) = self.account_for_folder(email.folder_id) else {
            return;
        };
        // The reader's copy, or the cached one when it hasn't loaded yet
        // (a double-click lands before the first click's render).
        let rendered = self
            .state()
            .message_view
            .as_ref()
            .filter(|_| self.state().rendered_id == Some(email.id))
            .and_then(|view| view.raw());
        let raw = rendered.or_else(|| self.db().borrow().raw_message(email.id).ok().flatten());
        let Some(raw) = raw else {
            return;
        };
        let parsed = mime::parse_message(&raw);
        let body_html = match &parsed.html_body {
            Some(body) => compose::editable_body(body),
            None => html::to_html(parsed.text_body.as_deref().unwrap_or("").trim_end()),
        };
        let message_id = if email.message_id.is_empty() {
            parsed.message_id.clone()
        } else {
            email.message_id.clone()
        };
        self.open_composer(
            &account,
            Draft {
                to: parsed.to.join(", "),
                cc: parsed.cc.join(", "),
                bcc: parsed.bcc.join(", "),
                subject: parsed.subject,
                body_html,
                attachments: parsed.attachments,
                resumed: Some(ResumedDraft {
                    email_id: email.id,
                    account_id: account.id,
                    message_id,
                }),
                ..Draft::default()
            },
        );
    }

    pub(super) fn open_composer(&self, account: &Account, draft: Draft) {
        self.host_composer(Composer::new(self.db(), account, draft));
    }

    /// Open the composer for a mailto: link handed to us by the desktop.
    pub fn open_mailto(&self, uri: &str) {
        let Some(account) = self.compose_account() else {
            return;
        };
        self.host_composer(Composer::for_mailto(self.db(), &account, uri));
    }

    fn host_composer(&self, composer: Composer) {
        composer.set_queue_handler(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |queued| window.on_send_queued(queued)
        ));
        composer.connect_finished(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_composer_finished()
        ));
        composer.connect_notice(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |text| window.toast(text)
        ));
        if cfg!(feature = "phone") {
            composer::present_in_window(self.application().as_ref(), &composer);
            return;
        }
        // One inline at a time: the one already there makes way.
        self.displace_inline_composer();
        composer.set_inline(true);
        composer.connect_closed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |composer| window.close_inline_composer(composer)
        ));
        composer.connect_pop_out(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.displace_inline_composer()
        ));
        let imp = self.imp();
        let showed_content = imp.inner_split.shows_content();
        imp.composer_slot.set_child(Some(&composer));
        imp.reader_pane.set_visible_child_name(PAGE_COMPOSER);
        imp.inner_split.set_show_content(true);
        let selection = self.selected_ids();
        self.state_mut().inline_composer = Some(InlineComposer {
            composer: composer.clone(),
            selection,
            showed_content,
        });
        composer.focus_first_field();
    }

    fn on_composer_finished(&self) {
        let keep_id = self.selected_email().map(|c| c.id());
        self.reload_folders();
        self.refresh_emails(keep_id);
        let accounts: Vec<Account> = self.state().accounts.values().cloned().collect();
        for account in accounts {
            self.drain_outbox(&account);
        }
    }

    /// Sent or cancelled: the reader comes back. A composer that has since
    /// popped out closes its own window instead.
    fn close_inline_composer(&self, composer: &Composer) {
        let is_inline = self
            .state()
            .inline_composer
            .as_ref()
            .is_some_and(|inline| inline.composer == *composer);
        if is_inline {
            self.take_inline_composer(true);
        }
    }

    /// The selection changed under the inline composer: the reader follows
    /// it, and the composer carries on in a window.
    pub(super) fn on_selection_moved(&self) {
        let opened_on = self
            .state()
            .inline_composer
            .as_ref()
            .map(|inline| inline.selection.clone());
        if opened_on.is_some_and(|ids| ids != self.selected_ids()) {
            let Some(inline) = self.take_inline_composer(false) else {
                return;
            };
            self.pop_out(inline.composer);
        }
    }

    /// Move the inline composer to a window, which also brings the reader
    /// back.
    fn displace_inline_composer(&self) {
        if let Some(inline) = self.take_inline_composer(true) {
            self.pop_out(inline.composer);
        }
    }

    fn pop_out(&self, composer: Composer) {
        // Nothing typed yet: nothing to carry over.
        if !composer.is_blank() {
            composer::present_in_window(self.application().as_ref(), &composer);
        }
    }

    /// Take the inline composer out of the reader pane. `restore_split`
    /// takes a collapsed window back to the list if it opened from there.
    fn take_inline_composer(&self, restore_split: bool) -> Option<InlineComposer> {
        let inline = self.state_mut().inline_composer.take()?;
        self.hold_editor_shortcuts(false);
        let imp = self.imp();
        imp.composer_slot.set_child(None::<&gtk::Widget>);
        imp.reader_pane.set_visible_child_name(PAGE_READER);
        if restore_split && !inline.showed_content {
            imp.inner_split.set_show_content(false);
        }
        Some(inline)
    }

    fn selected_ids(&self) -> Vec<i64> {
        self.selected_emails()
            .iter()
            .map(|email| email.id())
            .collect()
    }

    fn on_focus_moved(&self) {
        let composer = self
            .state()
            .inline_composer
            .as_ref()
            .map(|inline| inline.composer.clone());
        let is_composing = match (composer, GtkWindowExt::focus(self)) {
            (Some(composer), Some(focus)) => focus.is_ancestor(&composer),
            _ => false,
        };
        self.hold_editor_shortcuts(is_composing);
    }

    /// Disable `HELD_ACTIONS` while the inline composer has the focus, and
    /// restore them after. `set_actions_enabled` writes to the saved state
    /// meanwhile, so what the selection allows is what comes back.
    fn hold_editor_shortcuts(&self, should_hold: bool) {
        let is_held = !self.state().held_actions.is_empty();
        if should_hold == is_held {
            return;
        }
        let actions = HELD_ACTIONS.into_iter().filter_map(|name| {
            let action = self
                .lookup_action(name)
                .and_downcast::<gtk::gio::SimpleAction>()?;
            Some((name, action))
        });
        if should_hold {
            let mut held = HashMap::new();
            for (name, action) in actions {
                held.insert(name, action.is_enabled());
                action.set_enabled(false);
            }
            self.state_mut().held_actions = held;
        } else {
            let held = std::mem::take(&mut self.state_mut().held_actions);
            for (name, action) in actions {
                action.set_enabled(held.get(name).copied().unwrap_or(true));
            }
        }
    }
}

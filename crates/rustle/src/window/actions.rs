//! Read/unread, star, and the row context menu.

use super::{MainWindow, MAIL_ACTIONS, REPLY_FORWARD_ACTIONS};
use crate::i18n::gettext;
use crate::objects::EmailObject;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use rustle_core::folders::FolderRole;
use rustle_core::net::imap::{FLAG_FLAGGED, FLAG_PINNED, FLAG_SEEN};
use rustle_core::queue::{Change, PendingOp};
use std::collections::{HashMap, HashSet};

/// A window method bound to an action name.
type ActionHandler = fn(&MainWindow);

#[derive(Clone, Copy)]
enum FlagField {
    Unread,
    Starred,
    Pinned,
}

fn register(
    target: &impl IsA<gio::ActionMap>,
    name: &str,
    parameter_type: Option<&glib::VariantTy>,
    handler: impl Fn(Option<&glib::Variant>) + 'static,
) {
    let action = gio::SimpleAction::new(name, parameter_type);
    action.connect_activate(move |_, parameter| handler(parameter));
    target.add_action(&action);
}

impl MainWindow {
    pub(super) fn setup_actions(&self) {
        let plain: [(&str, ActionHandler); 20] = [
            ("toggle-read", |w| w.on_toggle_read()),
            ("toggle-star", |w| w.on_toggle_star()),
            ("toggle-pin", |w| w.on_toggle_pin()),
            ("archive", |w| w.on_archive()),
            ("trash", |w| w.on_trash()),
            ("compose", |w| w.on_compose_clicked()),
            ("reply", |w| w.open_reply(false)),
            ("reply-all", |w| w.open_reply(true)),
            ("forward", |w| w.open_forward()),
            ("edit-draft", |w| w.edit_draft()),
            ("refresh", |w| w.on_refresh_clicked()),
            ("search", |w| w.on_search_action()),
            ("manage-accounts", |w| w.on_manage_accounts()),
            ("manage-rules", |w| w.on_manage_rules()),
            ("print", |w| w.print_message()),
            ("save-message", |w| w.save_message()),
            ("show-source", |w| w.show_source()),
            ("zoom-in", |w| w.step_zoom(1)),
            ("zoom-out", |w| w.step_zoom(-1)),
            ("zoom-reset", |w| w.step_zoom(0)),
        ];
        for (name, handler) in plain {
            let window = self.downgrade();
            register(self, name, None, move |_| {
                if let Some(window) = window.upgrade() {
                    handler(&window);
                }
            });
        }
        // Stateful, so the header's toggle button tracks the sidebar.
        let toggle_sidebar =
            gio::PropertyAction::new("toggle-sidebar", &*self.imp().outer_split, "show-sidebar");
        self.add_action(&toggle_sidebar);
        // Move is the one action carrying a parameter: the destination folder id.
        let window = self.downgrade();
        register(
            self,
            "move",
            Some(glib::VariantTy::INT64),
            move |parameter| {
                if let (Some(window), Some(folder_id)) =
                    (window.upgrade(), parameter.and_then(|p| p.get::<i64>()))
                {
                    window.on_move(folder_id);
                }
            },
        );
    }

    fn set_actions_enabled(&self, names: &[&str], is_enabled: bool) {
        for name in names {
            // Held off for the inline composer: it gets this state back.
            let is_held = {
                let mut state = self.state_mut();
                let wanted = state.held_actions.get_mut(*name);
                wanted.map(|wanted| *wanted = is_enabled).is_some()
            };
            if is_held {
                continue;
            }
            if let Some(action) = self.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                action.set_enabled(is_enabled);
            }
        }
    }

    pub(super) fn set_mail_actions_enabled(&self, is_enabled: bool) {
        self.set_actions_enabled(&MAIL_ACTIONS, is_enabled);
        self.imp().move_button.set_sensitive(is_enabled);
    }

    pub(super) fn set_reply_forward_enabled(&self, is_enabled: bool) {
        self.set_actions_enabled(&REPLY_FORWARD_ACTIONS, is_enabled);
        let imp = self.imp();
        for button in [
            &imp.reply_button,
            &imp.reply_all_button,
            &imp.forward_button,
        ] {
            button.set_sensitive(is_enabled);
        }
    }

    /// Every selected email. Before a mail view is loaded there is
    /// nothing selected rather than an error; every mail action funnels
    /// through here, which is why the guard belongs here.
    pub(super) fn selected_emails(&self) -> Vec<EmailObject> {
        if self.state().view.is_none() {
            return Vec::new();
        }
        let selection = self.selection();
        let model = self.email_model();
        let positions = selection.selection();
        (0..positions.size())
            .filter_map(|index| {
                model
                    .item(positions.nth(index as u32))
                    .and_downcast::<EmailObject>()
            })
            .collect()
    }

    /// The ids of every selected email, in list order.
    pub(super) fn selected_ids(&self) -> Vec<i64> {
        self.selected_emails()
            .iter()
            .map(|email| email.id())
            .collect()
    }

    pub(super) fn selected_email(&self) -> Option<EmailObject> {
        let selected = self.selected_emails();
        if selected.len() == 1 {
            selected.into_iter().next()
        } else {
            None
        }
    }

    fn on_toggle_read(&self) {
        let emails = self.selected_emails();
        if !emails.is_empty() {
            self.toggle_flag(&emails, FlagField::Unread);
        }
    }

    fn on_toggle_star(&self) {
        let emails = self.selected_emails();
        if !emails.is_empty() {
            self.toggle_flag(&emails, FlagField::Starred);
        }
    }

    fn on_toggle_pin(&self) {
        let emails = self.selected_emails();
        if !emails.is_empty() {
            self.toggle_flag(&emails, FlagField::Pinned);
        }
    }

    /// A button pressed on a new-mail notification: mark that message read,
    /// archive it or delete it, the same way the window's buttons do.
    pub fn act_on_notified(&self, folder_id: i64, uid: &str, action: &str) {
        let email = {
            let db = self.db();
            let db = db.borrow();
            db.email_ids_for_uids(folder_id, &[uid.to_string()])
                .ok()
                .and_then(|ids| ids.first().copied())
                .and_then(|id| db.email(id).ok().flatten())
        };
        let Some(email) = email else {
            log::warn!("a notification named message {uid} of folder {folder_id}, which is gone");
            return;
        };
        // The list's own object when it's showing, so its row updates in place.
        let object = self
            .list_emails_with_ids(&HashSet::from([email.id]))
            .pop()
            .unwrap_or_else(|| EmailObject::new(email));
        match action {
            "read" => self.mark_email_read(&object),
            "archive" => self.move_emails_by_role(vec![object], FolderRole::Archive),
            "trash" => self.move_emails_by_role(vec![object], FolderRole::Trash),
            other => log::warn!("unknown notification action {other}"),
        }
        let account = self
            .account_for_folder(folder_id)
            .map(|(account, _)| account);
        if let (Some(app), Some(account)) = (self.application(), account) {
            app.withdraw_notification(&format!("new-mail-{}", account.id));
        }
    }

    /// Clear the unread flag for an email: locally, in the
    /// badges and list, and on the server. Guarded so an already-read email
    /// isn't flipped back to unread.
    pub(super) fn mark_email_read(&self, email: &EmailObject) {
        if email.with(|c| c.is_unread) {
            self.toggle_flag(std::slice::from_ref(email), FlagField::Unread);
        }
    }

    /// Flip one boolean flag across a selection, locally and on the server.
    /// A mixed selection follows the aggregate command shown in the menu: if
    /// anything is unread, the whole selection is marked read.
    fn toggle_flag(&self, emails: &[EmailObject], field: FlagField) {
        let read = |mail: &rustle_core::models::Email| match field {
            FlagField::Unread => mail.is_unread,
            FlagField::Starred => mail.is_starred,
            FlagField::Pinned => mail.is_pinned,
        };
        let value = toggled_value(emails.iter().map(|email| email.with(read)));
        self.set_flag(emails, field, value);
    }

    /// Shift+I and Shift+U: set read or unread outright, whatever the
    /// selection's mix. Only the emails not already so are touched.
    pub(super) fn set_unread(&self, emails: &[EmailObject], unread: bool) {
        let changing = differing(emails, |email| email.with(|c| c.is_unread), unread);
        if !changing.is_empty() {
            self.set_flag(&changing, FlagField::Unread, unread);
        }
    }

    /// Set one boolean flag across emails, locally and on the server.
    fn set_flag(&self, emails: &[EmailObject], field: FlagField, value: bool) {
        {
            let db = self.db();
            let db = db.borrow();
            for email in emails {
                email.update(|mail| {
                    let saved = match field {
                        FlagField::Unread => {
                            mail.is_unread = value;
                            db.set_email_unread(mail.id, value)
                        }
                        FlagField::Starred => {
                            mail.is_starred = value;
                            db.set_email_starred(mail.id, value)
                        }
                        FlagField::Pinned => {
                            mail.is_pinned = value;
                            db.set_email_pinned(mail.id, value)
                        }
                    };
                    if let Err(error) = saved {
                        log::error!("could not save the flag of message {}: {error}", mail.id);
                    }
                });
            }
        }
        self.after_flag_change();

        // One STORE per mailbox rather than one per message: in the unified
        // inbox a selection can span several accounts. The queue sends it,
        // and keeps it while the server can't be reached.
        let mut by_folder: HashMap<i64, Vec<String>> = HashMap::new();
        for email in emails {
            email.with(|c| {
                by_folder
                    .entry(c.folder_id)
                    .or_default()
                    .extend(c.server_id.iter().cloned())
            });
        }
        let (flag, add) = match field {
            FlagField::Unread => (FLAG_SEEN, !value),
            FlagField::Starred => (FLAG_FLAGGED, value),
            FlagField::Pinned => (FLAG_PINNED, value),
        };
        for (folder_id, uids) in by_folder {
            if uids.is_empty() {
                continue;
            }
            let Some((account, folder)) = self.account_for_folder(folder_id) else {
                continue;
            };
            let op = PendingOp {
                id: 0,
                account_id: account.id,
                folder_id,
                folder: folder.name,
                change: Change::Flag {
                    uids,
                    flag: flag.to_string(),
                    add,
                },
            };
            self.queue_change(&account, op);
        }
    }

    /// Update badges and the list after a flag change, keeping the
    /// selection as it was so the reader doesn't reload. Not the flagged
    /// emails: a notification's Mark Read flags one nobody selected.
    fn after_flag_change(&self) {
        self.reload_folders();
        self.refresh_keeping_selection();
    }

    /// Select an unselected right-clicked row, then pop up its actions menu.
    pub(super) fn on_row_right_click(
        &self,
        gesture: &gtk::GestureClick,
        x: f64,
        y: f64,
        item: &gtk::ListItem,
    ) {
        let position = item.position();
        if position == gtk::INVALID_LIST_POSITION {
            return;
        }
        // Selecting the row opens it in the reader, which marks it read and
        // would rebuild the list under the menu about to open on this row.
        self.state_mut().is_row_menu_open = true;
        let selection = self.selection();
        if !selection.is_selected(position) {
            self.state_mut().is_selection_update_in_progress = true;
            selection.unselect_all();
            selection.select_item(position, true);
            self.state_mut().is_selection_update_in_progress = false;
            self.update_reader();
        }
        let Some(email) = self
            .email_model()
            .item(position)
            .and_downcast::<EmailObject>()
        else {
            self.end_row_menu();
            return;
        };
        let Some(row_widget) = gesture.widget().filter(|row| row.root().is_some()) else {
            self.end_row_menu();
            return;
        };

        let popover = gtk::PopoverMenu::from_model(Some(&self.context_menu(&email)));
        popover.insert_action_group("context", Some(&self.context_actions()));
        popover.set_parent(&row_widget);
        popover.set_has_arrow(false);
        // GtkModelButton activates its action after closing the popover, so
        // keep the action hierarchy alive until activation has finished.
        popover.connect_closed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |popover| {
                let popover = popover.clone();
                glib::idle_add_local_once(move || {
                    popover.unparent();
                    window.end_row_menu();
                });
            }
        ));
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    }

    /// The row menu has gone: the list catches up on any refresh it held off.
    fn end_row_menu(&self) {
        let is_deferred = {
            let mut state = self.state_mut();
            state.is_row_menu_open = false;
            std::mem::take(&mut state.is_refresh_deferred)
        };
        if is_deferred {
            self.refresh_keeping_selection();
        }
    }

    /// The subset of the window's actions the row context menu offers.
    fn context_actions(&self) -> gio::SimpleActionGroup {
        let group = gio::SimpleActionGroup::new();
        let handlers: [(&str, ActionHandler); 5] = [
            ("toggle-read", |w| w.on_toggle_read()),
            ("toggle-star", |w| w.on_toggle_star()),
            ("toggle-pin", |w| w.on_toggle_pin()),
            ("archive", |w| w.on_archive()),
            ("trash", |w| w.on_trash()),
        ];
        for (name, handler) in handlers {
            let window = self.downgrade();
            register(&group, name, None, move |_| {
                if let Some(window) = window.upgrade() {
                    handler(&window);
                }
            });
        }
        let window = self.downgrade();
        register(
            &group,
            "move",
            Some(glib::VariantTy::INT64),
            move |parameter| {
                if let (Some(window), Some(folder_id)) =
                    (window.upgrade(), parameter.and_then(|p| p.get::<i64>()))
                {
                    window.on_move(folder_id);
                }
            },
        );
        group
    }

    fn context_menu(&self, email: &EmailObject) -> gio::Menu {
        let menu = gio::Menu::new();
        let mut selected = self.selected_emails();
        if selected.is_empty() {
            selected.push(email.clone());
        }
        let any_unread = selected.iter().any(|c| c.with(|c| c.is_unread));
        let any_starred = selected.iter().any(|c| c.with(|c| c.is_starred));
        let any_pinned = selected.iter().any(|c| c.with(|c| c.is_pinned));

        let flags = gio::Menu::new();
        flags.append(
            Some(&if any_unread {
                gettext("Mark Read")
            } else {
                gettext("Mark Unread")
            }),
            Some("context.toggle-read"),
        );
        flags.append(
            Some(&if any_starred {
                gettext("Unstar")
            } else {
                gettext("Star")
            }),
            Some("context.toggle-star"),
        );
        flags.append(
            Some(&if any_pinned {
                gettext("Unpin")
            } else {
                gettext("Pin")
            }),
            Some("context.toggle-pin"),
        );
        menu.append_section(None, &flags);

        let actions = gio::Menu::new();
        actions.append(Some(&self.archive_label()), Some("context.archive"));
        actions.append(Some(&gettext("Delete")), Some("context.trash"));
        actions.append_submenu(Some(&gettext("Move to")), &self.build_move_menu("context"));
        menu.append_section(None, &actions);
        menu
    }
}

/// What a toggle sets a flag to across a selection: on, unless any of it
/// has it already. A mixed selection is cleared, as the menu says ("Mark
/// Read" when anything is unread).
fn toggled_value(current: impl IntoIterator<Item = bool>) -> bool {
    !current.into_iter().any(|is_set| is_set)
}

/// The items whose flag isn't `wanted` yet.
fn differing<T: Clone>(items: &[T], current: impl Fn(&T) -> bool, wanted: bool) -> Vec<T> {
    items
        .iter()
        .filter(|item| current(item) != wanted)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toggle_clears_a_mixed_selection() {
        assert!(!toggled_value([true, false]));
        assert!(!toggled_value([true]));
        assert!(toggled_value([false, false]));
    }

    #[test]
    fn marking_touches_only_what_differs() {
        // (id, is unread)
        let mixed = [(1, true), (2, false), (3, true)];
        let unread = |item: &(i32, bool)| item.1;
        // Shift+U on a mixed selection leaves it all unread, not read.
        assert_eq!(differing(&mixed, unread, true), vec![(2, false)]);
        assert_eq!(differing(&mixed, unread, false), vec![(1, true), (3, true)]);
        assert!(differing(&[(4, true)], unread, true).is_empty());
    }
}

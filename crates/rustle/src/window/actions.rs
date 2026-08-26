//! Read/unread, star, and the row context menu.

use super::{MainWindow, MAIL_ACTIONS, REPLY_FORWARD_ACTIONS};
use crate::i18n::{self, gettext};
use crate::objects::ConversationObject;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use rustle_core::models::Account;
use rustle_core::net::errors::classify;
use rustle_core::net::imap::{FLAG_FLAGGED, FLAG_SEEN};
use rustle_core::{secrets, sync};
use std::collections::HashMap;
use std::rc::Rc;

/// A window method bound to an action name.
type ActionHandler = fn(&MainWindow);

#[derive(Clone, Copy)]
enum FlagField {
    Unread,
    Starred,
}

/// One IMAP flag edit to apply to a set of messages in one mailbox. A frozen
/// snapshot for the worker, never a live object the main thread mutates.
#[derive(Clone)]
struct FlagChange {
    account: Account,
    folder_name: String,
    uids: Vec<String>,
    flag: &'static str,
    should_add: bool,
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
        let plain: [(&str, ActionHandler); 13] = [
            ("toggle-read", |w| w.on_toggle_read()),
            ("toggle-star", |w| w.on_toggle_star()),
            ("archive", |w| w.on_archive()),
            ("trash", |w| w.on_trash()),
            ("compose", |w| w.on_compose_clicked()),
            ("reply", |w| w.open_reply(false)),
            ("reply-all", |w| w.open_reply(true)),
            ("forward", |w| w.open_forward()),
            ("refresh", |w| w.on_refresh_clicked()),
            ("search", |w| w.on_search_action()),
            ("add-account", |w| w.on_add_account_clicked()),
            ("online-accounts", |w| w.on_online_accounts_clicked()),
            ("manage-accounts", |w| w.on_manage_accounts()),
        ];
        for (name, handler) in plain {
            let window = self.downgrade();
            register(self, name, None, move |_| {
                if let Some(window) = window.upgrade() {
                    handler(&window);
                }
            });
        }
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

    /// Every selected conversation. Before a mail view is loaded there is
    /// nothing selected rather than an error; every mail action funnels
    /// through here, which is why the guard belongs here.
    pub(super) fn selected_conversations(&self) -> Vec<ConversationObject> {
        if self.state().view.is_none() {
            return Vec::new();
        }
        let selection = self.selection();
        let model = self.conversation_model();
        let positions = selection.selection();
        (0..positions.size())
            .filter_map(|index| {
                model
                    .item(positions.nth(index as u32))
                    .and_downcast::<ConversationObject>()
            })
            .collect()
    }

    pub(super) fn selected_conversation(&self) -> Option<ConversationObject> {
        let selected = self.selected_conversations();
        if selected.len() == 1 {
            selected.into_iter().next()
        } else {
            None
        }
    }

    fn on_toggle_read(&self) {
        let conversations = self.selected_conversations();
        if !conversations.is_empty() {
            self.toggle_flag(&conversations, FlagField::Unread);
        }
    }

    fn on_toggle_star(&self) {
        let conversations = self.selected_conversations();
        if !conversations.is_empty() {
            self.toggle_flag(&conversations, FlagField::Starred);
        }
    }

    /// Clear the unread flag for a whole conversation: locally, in the
    /// badges and list, and on the server. Guarded so an already-read thread
    /// isn't flipped back to unread.
    pub(super) fn mark_conversation_read(&self, conversation: &ConversationObject) {
        if conversation.with(|c| c.is_unread()) {
            self.toggle_flag(std::slice::from_ref(conversation), FlagField::Unread);
        }
    }

    /// Flip one boolean flag across a selection, locally and on the server.
    /// A mixed selection follows the aggregate command shown in the menu: if
    /// anything is unread, the whole selection is marked read.
    fn toggle_flag(&self, conversations: &[ConversationObject], field: FlagField) {
        let read = |mail: &rustle_core::models::Email| match field {
            FlagField::Unread => mail.is_unread,
            FlagField::Starred => mail.is_starred,
        };
        let mut originals: HashMap<i64, bool> = HashMap::new();
        let mut any_set = false;
        for conversation in conversations {
            conversation.with(|c| {
                for mail in &c.emails {
                    originals.insert(mail.id, read(mail));
                    any_set |= read(mail);
                }
            });
        }
        let value = !any_set;
        let keep_id = if conversations.len() == 1 {
            Some(conversations[0].id())
        } else {
            None
        };

        let objects: Vec<ConversationObject> = conversations.to_vec();
        let db = self.db();
        let write = move |values: &HashMap<i64, bool>| {
            let db = db.borrow();
            for conversation in &objects {
                conversation.update(|c| {
                    for mail in &mut c.emails {
                        let new_value = values.get(&mail.id).copied().unwrap_or(false);
                        let saved = match field {
                            FlagField::Unread => {
                                mail.is_unread = new_value;
                                db.set_email_unread(mail.id, new_value)
                            }
                            FlagField::Starred => {
                                mail.is_starred = new_value;
                                db.set_email_starred(mail.id, new_value)
                            }
                        };
                        if let Err(error) = saved {
                            log::error!("could not save the flag of message {}: {error}", mail.id);
                        }
                    }
                });
            }
        };
        let write = Rc::new(write);

        let all_new: HashMap<i64, bool> = originals.keys().map(|id| (*id, value)).collect();
        write(&all_new);
        self.after_flag_change(keep_id);

        let revert_write = write.clone();
        let window = self.downgrade();
        let revert: Rc<dyn Fn()> = Rc::new(move || {
            revert_write(&originals);
            if let Some(window) = window.upgrade() {
                window.after_flag_change(keep_id);
            }
        });

        // One STORE per mailbox rather than one per message: in the unified
        // inbox a selection can span several accounts.
        let mut by_folder: HashMap<i64, Vec<String>> = HashMap::new();
        for conversation in conversations {
            conversation.with(|c| {
                by_folder
                    .entry(c.folder_id())
                    .or_default()
                    .extend(c.server_uids())
            });
        }
        let (flag, should_add) = match field {
            FlagField::Unread => (FLAG_SEEN, !value),
            FlagField::Starred => (FLAG_FLAGGED, value),
        };
        for (folder_id, uids) in by_folder {
            let Some((account, folder)) = self.account_for_folder(folder_id) else {
                continue;
            };
            self.run_flag_worker(
                FlagChange {
                    account,
                    folder_name: folder.name,
                    uids,
                    flag,
                    should_add,
                },
                revert.clone(),
            );
        }
    }

    /// Update badges and the list after a flag change, keeping the
    /// conversation selected so the reader doesn't reload.
    fn after_flag_change(&self, keep_id: Option<i64>) {
        self.reload_folders();
        self.refresh_conversations(keep_id);
    }

    fn run_flag_worker(&self, change: FlagChange, revert: Rc<dyn Fn()>) {
        let job = change.clone();
        workers::run(
            move || -> Result<(), Option<String>> {
                let Some(credential) = secrets::credential_for(&job.account) else {
                    log::warn!("could not sign in to account {}", job.account.email);
                    return Err(None);
                };
                sync::set_flag(
                    &job.account,
                    &credential,
                    &job.folder_name,
                    &job.uids,
                    job.flag,
                    job.should_add,
                )
                .map_err(|error| {
                    log::error!(
                        "could not set {} on {} message(s) in {} (account {}): {error}",
                        job.flag,
                        job.uids.len(),
                        job.folder_name,
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
                move |result: Result<(), Option<String>>| {
                    // The row was already updated optimistically, so leaving
                    // it would show a state the server never got.
                    match result {
                        Ok(()) => {}
                        Err(None) => {
                            revert();
                            window.toast(&gettext("Could not sign in to this account."));
                        }
                        Err(Some(message)) => {
                            revert();
                            window.toast(&i18n::format(
                                &gettext("Action failed: {msg}"),
                                &[("msg", &message)],
                            ));
                        }
                    }
                }
            ),
        );
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
        let selection = self.selection();
        if !selection.is_selected(position) {
            self.state_mut().is_selection_update_in_progress = true;
            selection.unselect_all();
            selection.select_item(position, true);
            self.state_mut().is_selection_update_in_progress = false;
            self.update_reader();
        }
        let Some(conversation) = self
            .conversation_model()
            .item(position)
            .and_downcast::<ConversationObject>()
        else {
            return;
        };
        let Some(row_widget) = gesture.widget() else {
            return;
        };

        let popover = gtk::PopoverMenu::from_model(Some(&self.context_menu(&conversation)));
        popover.insert_action_group("context", Some(&self.context_actions()));
        popover.set_parent(&row_widget);
        popover.set_has_arrow(false);
        // GtkModelButton activates its action after closing the popover, so
        // keep the action hierarchy alive until activation has finished.
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    }

    /// The subset of the window's actions the row context menu offers.
    fn context_actions(&self) -> gio::SimpleActionGroup {
        let group = gio::SimpleActionGroup::new();
        let handlers: [(&str, ActionHandler); 4] = [
            ("toggle-read", |w| w.on_toggle_read()),
            ("toggle-star", |w| w.on_toggle_star()),
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

    fn context_menu(&self, conversation: &ConversationObject) -> gio::Menu {
        let menu = gio::Menu::new();
        let mut selected = self.selected_conversations();
        if selected.is_empty() {
            selected.push(conversation.clone());
        }
        let any_unread = selected.iter().any(|c| c.with(|c| c.is_unread()));
        let any_starred = selected.iter().any(|c| c.with(|c| c.is_starred()));

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
        menu.append_section(None, &flags);

        let actions = gio::Menu::new();
        actions.append(Some(&self.archive_label()), Some("context.archive"));
        actions.append(Some(&gettext("Delete")), Some("context.trash"));
        actions.append_submenu(Some(&gettext("Move to")), &self.build_move_menu("context"));
        menu.append_section(None, &actions);
        menu
    }
}

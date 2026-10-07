//! Changing the mailbox tree from the sidebar: a right-click on an account
//! makes a folder, on a folder makes a subfolder, renames or deletes it. And
//! a folder row takes messages dropped on it from the list.

use super::MainWindow;
use crate::i18n::{self, gettext};
use crate::objects::{EmailObject, SidebarKind};
use crate::widgets::folder_row::FolderRow;
use crate::workers;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use rustle_core::folders::{self, FolderRole};
use rustle_core::models::{Account, Folder};
use rustle_core::net::errors::classify;
use rustle_core::secrets;
use rustle_core::sync::{self, MailboxChange};
use std::collections::HashSet;

/// What a drag from the list carries: this prefix, then email ids. Plain
/// text dropped from elsewhere never starts with it.
pub(super) const DRAG_PREFIX: &str = "rustle-emails:";

impl MainWindow {
    pub(super) fn on_folder_right_click(
        &self,
        gesture: &gtk::GestureClick,
        x: f64,
        y: f64,
        item: &gtk::ListItem,
    ) {
        let Some((_, entry)) = FolderRow::item_of(item) else {
            return;
        };
        let (account, folder) = match entry.kind() {
            SidebarKind::Account(account) => (*account, None),
            SidebarKind::Folder(folder) => {
                let Some((account, _)) = self.account_for_folder(folder.id) else {
                    return;
                };
                (account, Some(folder))
            }
            SidebarKind::UnifiedInbox => return,
        };
        let menu = gio::Menu::new();
        let group = gio::SimpleActionGroup::new();
        let window = self.downgrade();

        let label = if folder.is_some() {
            gettext("New Subfolder…")
        } else {
            gettext("New Folder…")
        };
        menu.append(Some(&label), Some("folder.new"));
        let action = gio::SimpleAction::new("new", None);
        action.connect_activate(glib::clone!(
            #[strong]
            account,
            #[strong]
            folder,
            #[strong]
            window,
            move |_, _| {
                if let Some(window) = window.upgrade() {
                    window.new_folder(&account, folder.as_ref());
                }
            }
        ));
        group.add_action(&action);

        if let Some(folder) = folder.filter(is_user_folder) {
            let section = gio::Menu::new();
            section.append(Some(&gettext("Rename…")), Some("folder.rename"));
            section.append(Some(&gettext("Delete…")), Some("folder.delete"));
            menu.append_section(None, &section);
            for (name, delete) in [("rename", false), ("delete", true)] {
                let action = gio::SimpleAction::new(name, None);
                action.connect_activate(glib::clone!(
                    #[strong]
                    account,
                    #[strong]
                    folder,
                    #[strong]
                    window,
                    move |_, _| {
                        let Some(window) = window.upgrade() else {
                            return;
                        };
                        if delete {
                            window.delete_folder(&account, &folder);
                        } else {
                            window.rename_folder(&account, &folder);
                        }
                    }
                ));
                group.add_action(&action);
            }
        }

        let Some(widget) = gesture.widget() else {
            return;
        };
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.insert_action_group("folder", Some(&group));
        popover.set_parent(&widget);
        popover.set_has_arrow(false);
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    }

    fn new_folder(&self, account: &Account, parent: Option<&Folder>) {
        let delimiter = self.delimiter_of(account, parent);
        let heading = match parent {
            Some(parent) => i18n::format(
                &gettext("New Folder in “{name}”"),
                &[("name", &folder_label(parent))],
            ),
            None => gettext("New Folder"),
        };
        let parent_name = parent.map(|parent| parent.name.clone());
        let account = account.clone();
        let separator = delimiter.clone();
        self.ask_folder_name(
            &heading,
            "",
            &gettext("Create"),
            &separator,
            move |window, leaf| {
                let leaf = folders::encode_mailbox_name(leaf);
                let name = match &parent_name {
                    Some(parent) => format!("{parent}{delimiter}{leaf}"),
                    None => leaf,
                };
                window.change_mailbox(&account, MailboxChange::Create(name.clone()), Some(name));
            },
        );
    }

    fn rename_folder(&self, account: &Account, folder: &Folder) {
        let delimiter = self.delimiter_of(account, Some(folder));
        let parent = folders::parent_mailbox_name(&folder.name, &delimiter);
        let from = folder.name.clone();
        let account = account.clone();
        self.ask_folder_name(
            &i18n::format(
                &gettext("Rename “{name}”"),
                &[("name", &folder_label(folder))],
            ),
            &folder_label(folder),
            &gettext("Rename"),
            &delimiter.clone(),
            move |window, leaf| {
                let leaf = folders::encode_mailbox_name(leaf);
                let to = if parent.is_empty() {
                    leaf
                } else {
                    format!("{parent}{delimiter}{leaf}")
                };
                if to != from {
                    let change = MailboxChange::Rename {
                        from: from.clone(),
                        to: to.clone(),
                    };
                    window.change_mailbox(&account, change, Some(to));
                }
            },
        );
    }

    fn delete_folder(&self, account: &Account, folder: &Folder) {
        let dialog = adw::AlertDialog::new(
            Some(&i18n::format(
                &gettext("Delete “{name}”?"),
                &[("name", &folder_label(folder))],
            )),
            Some(&gettext(
                "The folder and every message in it are deleted from the server. This can't be undone.",
            )),
        );
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("delete", &gettext("Delete")),
        ]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let account = account.clone();
        let name = folder.name.clone();
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    if response == "delete" {
                        window.change_mailbox(&account, MailboxChange::Delete(name.clone()), None);
                    }
                }
            ),
        );
        dialog.present(Some(self));
    }

    /// Ask for one folder name; `on_name` gets it trimmed, never empty and
    /// never holding the hierarchy delimiter (that would make two levels).
    fn ask_folder_name(
        &self,
        heading: &str,
        initial: &str,
        confirm: &str,
        delimiter: &str,
        on_name: impl Fn(&MainWindow, &str) + 'static,
    ) {
        let entry = gtk::Entry::builder()
            .text(initial)
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::new(Some(heading), None);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", &gettext("Cancel")), ("ok", confirm)]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        let is_valid = {
            let delimiter = delimiter.to_string();
            move |text: &str| {
                let text = text.trim();
                !text.is_empty() && (delimiter.is_empty() || !text.contains(delimiter.as_str()))
            }
        };
        dialog.set_response_enabled("ok", is_valid(initial));
        entry.connect_changed(glib::clone!(
            #[weak]
            dialog,
            #[strong]
            is_valid,
            move |entry| dialog.set_response_enabled("ok", is_valid(&entry.text()))
        ));
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                entry,
                move |_, response| {
                    let text = entry.text();
                    if response == "ok" && is_valid(&text) {
                        on_name(&window, text.trim());
                    }
                }
            ),
        );
        dialog.present(Some(self));
        entry.grab_focus();
    }

    /// The hierarchy delimiter in use: the folder's own, else any of the
    /// account's, else "/".
    fn delimiter_of(&self, account: &Account, folder: Option<&Folder>) -> String {
        if let Some(folder) = folder.filter(|f| !f.delimiter.is_empty()) {
            return folder.delimiter.clone();
        }
        self.db()
            .borrow()
            .folders_for_account(account.id)
            .unwrap_or_default()
            .into_iter()
            .map(|folder| folder.delimiter)
            .find(|delimiter| !delimiter.is_empty())
            .unwrap_or_else(|| "/".to_string())
    }

    /// Run a mailbox change on the server, then re-read the folder list --
    /// opening `then_open` when given, so a new or renamed folder is shown.
    fn change_mailbox(&self, account: &Account, change: MailboxChange, then_open: Option<String>) {
        let job_account = account.clone();
        let job_change = change.clone();
        workers::run(
            move || -> Result<(), String> {
                let Some(credential) = secrets::credential_for(&job_account) else {
                    return Err(gettext("Could not sign in to this account."));
                };
                sync::change_mailbox(&job_account, &credential, &job_change).map_err(|error| {
                    log::error!(
                        "could not change mailbox ({job_change:?}) on {} (account {}): {error}",
                        job_account.imap_host,
                        job_account.email
                    );
                    i18n::failure_message(&classify(&error, &job_account.imap_host))
                })
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[strong]
                account,
                move |result: Result<(), String>| match result {
                    Ok(()) => {
                        window.start_sync(&account, false, then_open.as_deref(), 0);
                    }
                    Err(message) => window.toast(&i18n::format(
                        &gettext("Couldn't change the folder: {msg}"),
                        &[("msg", &message)],
                    )),
                }
            ),
        );
    }

    /// A folder row takes messages dragged from the list.
    pub(super) fn add_folder_drop_target(&self, row: &impl IsA<gtk::Widget>, item: &gtk::ListItem) {
        let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
        let weak_item = item.downgrade();
        target.connect_enter(glib::clone!(
            #[strong]
            weak_item,
            move |_, _, _| {
                let is_folder = weak_item
                    .upgrade()
                    .and_then(|item| FolderRow::item_of(&item))
                    .is_some_and(|(_, entry)| matches!(entry.kind(), SidebarKind::Folder(_)));
                if is_folder {
                    gdk::DragAction::MOVE
                } else {
                    gdk::DragAction::empty()
                }
            }
        ));
        target.connect_drop(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let Some(ids) = value
                    .get::<String>()
                    .ok()
                    .and_then(|text| dragged_ids(&text))
                else {
                    return false;
                };
                let Some(SidebarKind::Folder(folder)) = weak_item
                    .upgrade()
                    .and_then(|item| FolderRow::item_of(&item))
                    .map(|(_, entry)| entry.kind())
                else {
                    return false;
                };
                let emails = window.list_emails_with_ids(&ids);
                let is_elsewhere = emails
                    .iter()
                    .any(|email| email.with(|e| e.folder_id) != folder.id);
                if is_elsewhere {
                    window.move_emails_to(emails, folder.id);
                }
                true
            }
        ));
        row.add_controller(target);
    }

    /// The list's email objects among `ids`.
    pub(super) fn list_emails_with_ids(&self, ids: &HashSet<i64>) -> Vec<EmailObject> {
        let model = self.email_model();
        (0..model.n_items())
            .filter_map(|index| model.item(index).and_downcast::<EmailObject>())
            .filter(|email| ids.contains(&email.id()))
            .collect()
    }

    /// What a drag from a list row carries: the whole selection when the row
    /// is part of it, else that row alone.
    pub(super) fn drag_payload(&self, item: &gtk::ListItem) -> Option<String> {
        let ids: Vec<i64> = if item.is_selected() {
            self.selected_emails().iter().map(EmailObject::id).collect()
        } else {
            vec![item.item().and_downcast::<EmailObject>()?.id()]
        };
        if ids.is_empty() {
            return None;
        }
        let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
        Some(format!("{DRAG_PREFIX}{}", ids.join(",")))
    }
}

/// The ids in a drag payload; None for anything that isn't one.
fn dragged_ids(text: &str) -> Option<HashSet<i64>> {
    let ids: HashSet<i64> = text
        .strip_prefix(DRAG_PREFIX)?
        .split(',')
        .filter_map(|id| id.parse().ok())
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// Folders the user made, which they may rename or delete. Inbox, Sent and
/// the other special folders belong to the server; the Outbox to Rustle.
fn is_user_folder(folder: &Folder) -> bool {
    folder.name != folders::OUTBOX_FOLDER
        && folders::role_for_folder(&folder.name) == FolderRole::Other
}

fn folder_label(folder: &Folder) -> String {
    folders::display_name_for_folder(&folder.name, Some(&folder.delimiter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_payloads_are_dropped() {
        assert_eq!(
            dragged_ids("rustle-emails:3,1,x"),
            Some(HashSet::from([1, 3]))
        );
        assert_eq!(dragged_ids("rustle-emails:"), None);
        assert_eq!(dragged_ids("3,1"), None);
    }
}

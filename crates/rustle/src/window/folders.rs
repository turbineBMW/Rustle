//! The folder sidebar: a tree of every account's mailboxes, with the
//! unified inbox on top.

use super::{MainWindow, View, FOLDER_SYNC_COOLDOWN_SECS};
use crate::objects::{SidebarItem, SidebarKind};
use crate::widgets::folder_row::FolderRow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::folders::{self, FolderRole};
use rustle_core::models::{Account, Folder};
use std::collections::HashMap;
use std::time::{Duration, Instant};

impl MainWindow {
    /// Everything the mail view runs on. Built once, before we know whether
    /// there are any accounts to put in it.
    pub(super) fn build_mail_models(&self) {
        let imp = self.imp();
        let root_store = gio::ListStore::new::<SidebarItem>();
        let window = self.downgrade();
        let tree_model = gtk::TreeListModel::new(root_store.clone(), false, true, move |item| {
            window
                .upgrade()
                .and_then(|window| window.folder_children(item))
        });
        let folder_selection = gtk::SingleSelection::new(Some(tree_model.clone()));
        folder_selection.connect_selection_changed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _, _| window.on_folder_selected()
        ));
        let _ = imp.folder_root_store.set(root_store);
        let _ = imp.folder_tree_model.set(tree_model);
        let _ = imp.folder_selection.set(folder_selection);

        let conversation_sections = gio::ListStore::new::<gio::ListStore>();
        let conversation_model = gtk::FlattenListModel::new(Some(conversation_sections.clone()));
        let selection = gtk::MultiSelection::new(Some(conversation_model.clone()));
        // The sticky day label follows the model as well as the scroll.
        conversation_model.connect_items_changed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _, _, _| window.update_sticky_day()
        ));
        let _ = imp.conversation_sections.set(conversation_sections);
        let _ = imp.conversation_model.set(conversation_model);
        let _ = imp.selection.set(selection);

        self.setup_folder_sidebar();
        self.setup_conversation_list();
    }

    /// Two kinds of branch: an account row holds its top-level mailboxes, a
    /// folder row holds its subfolders. The unified inbox has none.
    fn folder_children(&self, item: &glib::Object) -> Option<gio::ListModel> {
        let item = item.downcast_ref::<SidebarItem>()?;
        let state = self.state();
        let children = match item.kind() {
            SidebarKind::UnifiedInbox => return None,
            SidebarKind::Account(account) => state.account_roots.get(&account.id)?,
            SidebarKind::Folder(folder) => state.folder_children.get(&folder.id)?,
        };
        if children.is_empty() {
            return None;
        }
        let store = gio::ListStore::new::<SidebarItem>();
        for child in children {
            store.append(&SidebarItem::new(SidebarKind::Folder(child.clone())));
        }
        Some(store.upcast())
    }

    fn setup_folder_sidebar(&self) {
        let imp = self.imp();
        imp.folder_list
            .set_model(Some(imp.folder_selection.get().expect("built")));
        let factory = gtk::SignalListItemFactory::new();
        // setup: build one empty widget, reused for many folders as the list
        // scrolls. The expander draws the indent and the expand/collapse arrow.
        factory.connect_setup(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let expander = gtk::TreeExpander::new();
            expander.set_child(Some(&FolderRow::default()));
            item.set_child(Some(&expander));
        });
        factory.connect_bind(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, item| {
                if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                    window.on_folder_row_bind(item);
                }
            }
        ));
        factory.connect_unbind(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, item| {
                if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                    window.on_folder_row_unbind(item);
                }
            }
        ));
        imp.folder_list.set_factory(Some(&factory));
    }

    /// bind: fill an existing widget from its item. Runs on every scroll, so
    /// it only copies fields across.
    fn on_folder_row_bind(&self, item: &gtk::ListItem) {
        let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        let Some((tree_row, entry)) = FolderRow::item_of(item) else {
            return;
        };
        expander.set_list_row(Some(&tree_row));
        let Some(row) = expander.child().and_downcast::<FolderRow>() else {
            return;
        };

        match entry.kind() {
            SidebarKind::UnifiedInbox => {
                item.set_selectable(true);
                row.bind_unified_inbox(self.unified_badge());
                self.state_mut().unified_row = Some(row);
            }
            // An account row is a heading over its folders, not somewhere to click.
            SidebarKind::Account(account) => {
                item.set_selectable(false);
                let is_syncing = self.state().syncing_account_ids.contains(&account.id);
                row.bind_account(&account, &tree_row, is_syncing);
                self.state_mut().account_rows.insert(account.id, row);
            }
            SidebarKind::Folder(folder) => {
                item.set_selectable(true);
                row.bind_folder(&folder, self.unread_badge(&folder));
                self.state_mut().folder_rows.insert(folder.id, row);
            }
        }
    }

    fn on_folder_row_unbind(&self, item: &gtk::ListItem) {
        if let Some((_, entry)) = FolderRow::item_of(item) {
            let mut state = self.state_mut();
            match entry.kind() {
                SidebarKind::UnifiedInbox => state.unified_row = None,
                SidebarKind::Account(account) => {
                    state.account_rows.remove(&account.id);
                }
                SidebarKind::Folder(folder) => {
                    state.folder_rows.remove(&folder.id);
                }
            }
        }
        if let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() {
            expander.set_list_row(None);
        }
    }

    /// What the sidebar shows next to a folder. Only the open folders'
    /// messages are synced, so their local count is the accurate one -- and
    /// it drops the moment a message is read. Every other folder shows what
    /// the server last reported.
    pub(super) fn unread_badge(&self, folder: &Folder) -> i64 {
        let is_open = self.current_folder_ids().contains(&folder.id);
        let remote = self.state().remote_unread_counts.get(&folder.id).copied();
        match remote {
            Some(count) if !is_open => count as i64,
            _ => self
                .db()
                .borrow()
                .unread_count_in_folder(folder.id)
                .unwrap_or(0),
        }
    }

    fn unified_badge(&self) -> i64 {
        self.inbox_folders()
            .iter()
            .map(|folder| self.unread_badge(folder))
            .sum()
    }

    /// Select the unified inbox with several accounts, or the one account's
    /// inbox (or its first folder if we can't spot one).
    fn select_default_row(&self) {
        let account_count = self.state().accounts.len();
        let mut target: Option<u32> = None;
        if account_count > 1 {
            target = self
                .sidebar_positions()
                .into_iter()
                .find(|(_, item)| item.is_unified_inbox())
                .map(|(p, _)| p);
        }
        if target.is_none() {
            for (position, item) in self.sidebar_positions() {
                let Some(folder) = item.folder() else {
                    continue;
                };
                if target.is_none() {
                    target = Some(position);
                }
                if folders::role_for_folder(&folder.name) == FolderRole::Inbox {
                    target = Some(position);
                    break;
                }
            }
        }
        let Some(target) = target else { return };
        // Row 0 is autoselected when the tree is built, so set_selected() may
        // emit nothing. Load the view directly instead.
        {
            let mut state = self.state_mut();
            state.is_folder_refresh_suppressed = true;
            state.view = None;
        }
        self.imp()
            .folder_selection
            .get()
            .expect("built")
            .set_selected(target);
        self.state_mut().is_folder_refresh_suppressed = false;
        self.on_folder_selected();
    }

    /// Every row in the flattened tree, with its position.
    fn sidebar_positions(&self) -> Vec<(u32, SidebarItem)> {
        let Some(model) = self.imp().folder_tree_model.get() else {
            return Vec::new();
        };
        (0..model.n_items())
            .filter_map(|position| self.sidebar_item_at(position).map(|item| (position, item)))
            .collect()
    }

    fn on_folder_selected(&self) {
        let selection = self.imp().folder_selection.get().expect("built");
        let Some(item) = selection
            .selected_item()
            .and_downcast::<gtk::TreeListRow>()
            .and_then(|row| row.item())
            .and_downcast::<SidebarItem>()
        else {
            return;
        };
        let new_view = match item.kind() {
            // SingleSelection autoselects row 0 when it is an account heading.
            SidebarKind::Account(_) => return,
            SidebarKind::UnifiedInbox => View::UnifiedInbox,
            SidebarKind::Folder(folder) => View::Folder(folder),
        };
        let previous = self.state_mut().view.replace(new_view.clone());
        self.update_move_menu();
        self.update_archive_button();
        if self.state().is_folder_refresh_suppressed {
            return;
        }
        self.refresh_conversations(None);

        // Only sync on a real view change -- rebuilding the sidebar re-emits
        // selection-changed for the same folder, which would loop. A folder
        // synced moments ago is left alone.
        let changed = match (&previous, &new_view) {
            (None, _) => true,
            (Some(View::UnifiedInbox), View::UnifiedInbox) => false,
            (Some(View::Folder(a)), View::Folder(b)) => a.id != b.id,
            _ => true,
        };
        if !changed || !self.state().is_online {
            return;
        }
        let cooldown = Duration::from_secs(FOLDER_SYNC_COOLDOWN_SECS);
        let now = Instant::now();
        for folder in self.current_folders() {
            let last = self.state().folder_sync_times.get(&folder.id).copied();
            if last.is_some_and(|last| now.duration_since(last) < cooldown) {
                continue;
            }
            // Bound first: a `Ref` in an `if let` condition lives through the
            // block, and start_sync needs the state mutably.
            let account = self.state().accounts.get(&folder.account_id).cloned();
            if let Some(account) = account {
                self.start_sync(&account, true, Some(&folder.name), 0);
            }
        }
    }

    /// Rebuilding the tree destroys every row, which resets the user's
    /// expand/collapse state, so only rebuild when the accounts, the folders
    /// or their nesting actually changed. A plain badge/icon update refreshes
    /// the rows in place.
    pub(super) fn reload_folders(&self) {
        let (accounts, folders) = {
            let db = self.db();
            let db = db.borrow();
            let accounts = db.accounts().unwrap_or_default();
            let folders: Vec<Folder> = accounts
                .iter()
                .flat_map(|a| db.folders_for_account(a.id).unwrap_or_default())
                .collect();
            (accounts, folders)
        };
        let shape = (
            accounts.iter().map(|a| a.id).collect::<Vec<_>>(),
            folders
                .iter()
                .map(|f| (f.id, f.parent_id))
                .collect::<Vec<_>>(),
        );
        crate::account_colors::apply(&accounts);
        self.avatars().set_accounts(&accounts);
        let needs_rebuild = {
            let mut state = self.state_mut();
            state.accounts = accounts.iter().map(|a| (a.id, a.clone())).collect();
            let mut account_roots: HashMap<i64, Vec<Folder>> = HashMap::new();
            let mut folder_children: HashMap<i64, Vec<Folder>> = HashMap::new();
            for folder in &folders {
                match folder.parent_id {
                    None => account_roots
                        .entry(folder.account_id)
                        .or_default()
                        .push(folder.clone()),
                    Some(parent) => folder_children
                        .entry(parent)
                        .or_default()
                        .push(folder.clone()),
                }
            }
            state.account_roots = account_roots;
            state.folder_children = folder_children;
            if shape != state.folder_shape {
                state.folder_shape = shape;
                true
            } else {
                false
            }
        };
        if needs_rebuild {
            self.rebuild_folder_tree(&accounts);
        }

        // SQLite reuses the rowid of a deleted folder, so anything keyed by
        // folder id has to go when the folder does.
        let live_ids: std::collections::HashSet<i64> = folders.iter().map(|f| f.id).collect();
        {
            let mut state = self.state_mut();
            state.loaded_counts.retain(|id, _| live_ids.contains(id));
            state
                .folders_with_more_mail
                .retain(|id, _| live_ids.contains(id));
            state
                .folder_sync_times
                .retain(|id, _| live_ids.contains(id));
            state
                .remote_unread_counts
                .retain(|id, _| live_ids.contains(id));
        }

        // Nothing is selected on a first run, or after the open folder was
        // pruned along with its account.
        let view = self.state().view.clone();
        let view_is_gone = match view {
            None => true,
            Some(View::Folder(folder)) => !live_ids.contains(&folder.id),
            Some(View::UnifiedInbox) => false,
        };
        if view_is_gone {
            self.state_mut().view = None;
            self.select_default_row();
        }

        let rows: Vec<(Folder, FolderRow)> = {
            let state = self.state();
            folders
                .iter()
                .filter_map(|f| {
                    state
                        .folder_rows
                        .get(&f.id)
                        .map(|row| (f.clone(), row.clone()))
                })
                .collect()
        };
        for (folder, row) in rows {
            row.bind_folder(&folder, self.unread_badge(&folder));
        }
        let unified_row = self.state().unified_row.clone();
        if let Some(row) = unified_row {
            row.bind_unified_inbox(self.unified_badge());
        }
    }

    fn rebuild_folder_tree(&self, accounts: &[Account]) {
        // Preserve the selection by identity, not row index -- pruning stale
        // folders shifts the indices. Re-selecting is suppressed so it doesn't
        // rebuild the conversation list; callers refresh that explicitly.
        let keep = self.state().view.clone();
        {
            let mut state = self.state_mut();
            state.is_folder_refresh_suppressed = true;
            state.folder_rows.clear();
            state.account_rows.clear();
            state.unified_row = None;
        }
        let root_store = self.imp().folder_root_store.get().expect("built");
        root_store.remove_all();
        if !accounts.is_empty() {
            root_store.append(&SidebarItem::new(SidebarKind::UnifiedInbox));
        }
        for account in accounts {
            root_store.append(&SidebarItem::new(SidebarKind::Account(account.clone())));
        }
        match keep {
            Some(View::Folder(folder)) => self.select_folder_by_id(folder.id),
            Some(View::UnifiedInbox) => {
                if let Some((position, _)) = self
                    .sidebar_positions()
                    .into_iter()
                    .find(|(_, item)| item.is_unified_inbox())
                {
                    self.imp()
                        .folder_selection
                        .get()
                        .expect("built")
                        .set_selected(position);
                }
            }
            None => {}
        }
        self.state_mut().is_folder_refresh_suppressed = false;
    }

    pub(super) fn select_folder_by_id(&self, folder_id: i64) {
        for (position, item) in self.sidebar_positions() {
            if item.folder().is_some_and(|folder| folder.id == folder_id) {
                self.imp()
                    .folder_selection
                    .get()
                    .expect("built")
                    .set_selected(position);
                return;
            }
        }
    }
}

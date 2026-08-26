//! The conversation list: contents, search, and paging.

use super::{MainWindow, PAGE_EMPTY, PAGE_LIST, PAGE_LOADING, SEARCH_DEBOUNCE_MS};
use crate::objects::ConversationObject;
use crate::widgets::conversation_row::ConversationRow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::glib;
use rustle_core::folders;
use rustle_core::models::Conversation;
use std::time::Duration;

impl MainWindow {
    /// Rebuild the conversation list from the current view, applying the
    /// search query if one is typed. `keep_id` re-selects that conversation
    /// if it's still in the list, so a mail action can refresh without
    /// reloading the reader.
    pub(super) fn refresh_conversations(&self, keep_id: Option<i64>) {
        if self.state().view.is_none() {
            return;
        }
        let scroller = &self.imp().conversation_scroller;
        let vadjustment = scroller.vadjustment();
        let scroll_position = vadjustment.value();

        let matches = self.matching_conversations(keep_id);
        self.replace_conversations(matches, keep_id);
        self.show_list_or_placeholder();
        self.update_reader();
        restore_scroll(&vadjustment, scroll_position);
    }

    /// The view's conversations, narrowed by the search box and filter.
    fn matching_conversations(&self, keep_id: Option<i64>) -> Vec<Conversation> {
        let imp = self.imp();
        let folder_ids = self.current_folder_ids();
        let query = imp.search_entry.text().trim().to_string();
        let matches = {
            let db = self.db();
            let db = db.borrow();
            let result = if query.is_empty() {
                db.conversations_in_folders(&folder_ids)
            } else {
                db.search_conversations(&folder_ids, &query)
            };
            result.unwrap_or_else(|error| {
                log::error!("could not load conversations: {error}");
                Vec::new()
            })
        };
        if !imp.unread_button.is_active() {
            return matches;
        }
        // Keep the conversation being read even once it's marked read, so
        // opening a mail here doesn't make it vanish under you.
        matches
            .into_iter()
            .filter(|c| c.is_unread() || Some(c.id()) == keep_id)
            .collect()
    }

    /// Swap in the new list, keeping `keep_id` selected if it survived.
    /// MultiSelection tracks positions while keep_id tracks the conversation
    /// itself, so the selection is cleared before the store is spliced and
    /// restored by identity afterwards.
    fn replace_conversations(&self, matches: Vec<Conversation>, keep_id: Option<i64>) {
        let target = keep_id.and_then(|id| matches.iter().position(|c| c.id() == id));
        let objects: Vec<ConversationObject> =
            matches.into_iter().map(ConversationObject::new).collect();
        let store = self.conversation_store();
        let selection = self.selection();
        self.state_mut().is_selection_update_in_progress = true;
        selection.unselect_all();
        store.splice(0, store.n_items(), &objects);
        match target {
            Some(index) => selection.select_item(index as u32, true),
            None => selection.unselect_all(),
        };
        self.state_mut().is_selection_update_in_progress = false;
    }

    pub(super) fn show_list_or_placeholder(&self) {
        let page = if self.conversation_store().n_items() > 0 {
            PAGE_LIST
        } else if self.is_current_account_syncing() {
            PAGE_LOADING
        } else {
            PAGE_EMPTY
        };
        self.imp().conversation_stack.set_visible_child_name(page);
    }

    /// Debounce keystrokes: query the database ~200ms after typing stops.
    pub(super) fn on_search_changed(&self) {
        if let Some(previous) = self.state_mut().search_timeout.take() {
            previous.remove();
        }
        let source = glib::timeout_add_local_once(
            Duration::from_millis(SEARCH_DEBOUNCE_MS),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.state_mut().search_timeout = None;
                    window.refresh_conversations(None);
                }
            ),
        );
        self.state_mut().search_timeout = Some(source);
    }

    /// Potentially thousands of rows, so this uses the scalable GTK4 pattern:
    /// a ListStore of data, a MultiSelection wrapper, and a factory that
    /// recycles a handful of ConversationRow widgets as you scroll.
    pub(super) fn setup_conversation_list(&self) {
        let imp = self.imp();
        self.selection().connect_selection_changed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _, _| {
                if !window.state().is_selection_update_in_progress {
                    window.update_reader();
                }
            }
        ));
        imp.conversation_list.set_model(Some(&self.selection()));

        let factory = gtk::SignalListItemFactory::new();
        // setup: build one empty widget. A right-click gesture opens the
        // actions menu for that row.
        factory.connect_setup(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let row = ConversationRow::new(window.avatars());
                let gesture = gtk::GestureClick::builder()
                    .button(gdk::BUTTON_SECONDARY)
                    .build();
                let list_item = item.clone();
                gesture.connect_pressed(glib::clone!(
                    #[weak]
                    window,
                    move |gesture, _, x, y| window.on_row_right_click(gesture, x, y, &list_item)
                ));
                row.add_controller(gesture);
                item.set_child(Some(&row));
            }
        ));
        // bind: fill an existing widget from its item. Runs often, so keep it cheap.
        factory.connect_bind(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let (Some(row), Some(conversation)) = (
                    item.child().and_downcast::<ConversationRow>(),
                    item.item().and_downcast::<ConversationObject>(),
                ) else {
                    return;
                };
                let is_outgoing = window
                    .current_folder()
                    .is_some_and(|folder| folders::is_outgoing_folder(&folder.name));
                let account_label = if window.is_unified_view() {
                    window
                        .account_for_folder(conversation.with(|c| c.folder_id()))
                        .map(|(account, _)| account.email)
                } else {
                    None
                };
                conversation.with(|c| row.bind(c, is_outgoing, account_label.as_deref()));
            }
        ));
        imp.conversation_list.set_factory(Some(&factory));
    }

    /// Scrolling to the bottom pulls the next-older page for the open folder,
    /// if the last sync said there's more to fetch.
    pub(super) fn on_list_edge_reached(&self, position: gtk::PositionType) {
        if position != gtk::PositionType::Bottom
            || !self.state().is_online
            || self.is_current_account_syncing()
        {
            return;
        }
        for folder in self.current_folders() {
            let (has_more, offset, account) = {
                let state = self.state();
                (
                    state
                        .folders_with_more_mail
                        .get(&folder.id)
                        .copied()
                        .unwrap_or(false),
                    state.loaded_counts.get(&folder.id).copied().unwrap_or(0),
                    state.accounts.get(&folder.account_id).cloned(),
                )
            };
            if let (true, Some(account)) = (has_more, account) {
                self.start_sync(&account, true, Some(&folder.name), offset);
            }
        }
    }

    pub(super) fn on_search_action(&self) {
        let bar = &self.imp().search_bar;
        bar.set_search_mode(!bar.is_search_mode());
    }

    pub(super) fn on_refresh_clicked(&self) {
        // Not in the background, so this is also the way past the sync
        // cooldown on the open folder.
        self.sync_all(false);
    }
}

/// Put the scroll position back after the store was replaced. Deferred to an
/// idle callback because the new contents have not been laid out yet.
fn restore_scroll(vadjustment: &gtk::Adjustment, position: f64) {
    if position <= 0.0 {
        return;
    }
    let vadjustment = vadjustment.clone();
    glib::idle_add_local_once(move || {
        let highest = vadjustment.upper() - vadjustment.page_size();
        vadjustment.set_value(position.min(highest));
    });
}

//! The conversation list: contents, search, and paging.

use super::{MainWindow, PAGE_EMPTY, PAGE_LIST, PAGE_LOADING, SEARCH_DEBOUNCE_MS};
use crate::i18n;
use crate::objects::ConversationObject;
use crate::widgets::conversation_row::ConversationRow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{gdk, gio};
use rustle_core::dates;
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
        let sections = day_sections(matches);
        let store = self.conversation_sections();
        let selection = self.selection();
        self.state_mut().is_selection_update_in_progress = true;
        selection.unselect_all();
        store.splice(0, store.n_items(), &sections);
        match target {
            Some(index) => selection.select_item(index as u32, true),
            None => selection.unselect_all(),
        };
        self.state_mut().is_selection_update_in_progress = false;
    }

    pub(super) fn show_list_or_placeholder(&self) {
        let page = if self.conversation_model().n_items() > 0 {
            PAGE_LIST
        } else if self.is_current_account_syncing() {
            PAGE_LOADING
        } else {
            PAGE_EMPTY
        };
        self.imp().conversation_stack.set_visible_child_name(page);
    }

    /// Pins the day of the topmost visible row above the list, so the date
    /// stays readable however far down a long day you've scrolled. Hidden
    /// when a section's own header is at the top edge, so it isn't doubled,
    /// and when the list is empty.
    pub(super) fn update_sticky_day(&self) {
        let imp = self.imp();
        let scroller = &imp.conversation_scroller;
        // Just inside the top edge, past the header's own hairline.
        let hit = scroller.pick(scroller.width() as f64 / 2.0, 1.0, gtk::PickFlags::DEFAULT);
        let row = hit.and_then(|widget| {
            widget
                .ancestor(ConversationRow::static_type())
                .and_downcast::<ConversationRow>()
        });
        match row {
            Some(row) if scroller.vadjustment().value() > 0.0 => {
                imp.sticky_day.set_label(&row.day_label());
                imp.sticky_day.set_visible(true);
            }
            _ => imp.sticky_day.set_visible(false),
        }
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
                let account = if window.is_unified_view() {
                    window
                        .account_for_folder(conversation.with(|c| c.folder_id()))
                        .map(|(account, _)| account)
                } else {
                    None
                };
                conversation.with(|c| row.bind(c, is_outgoing, account.as_ref()));
            }
        ));
        imp.conversation_list.set_factory(Some(&factory));

        // Sections are days (see `day_sections`); each gets a sticky header
        // labelled from its first conversation.
        let headers = gtk::SignalListItemFactory::new();
        headers.connect_setup(|_, item| {
            let Some(header) = item.downcast_ref::<gtk::ListHeader>() else {
                return;
            };
            let label = gtk::Label::builder()
                .xalign(0.0)
                .css_classes(["conversation-day-header"])
                .build();
            header.set_child(Some(&label));
        });
        headers.connect_bind(|_, item| {
            let Some(header) = item.downcast_ref::<gtk::ListHeader>() else {
                return;
            };
            let (Some(label), Some(conversation)) = (
                header.child().and_downcast::<gtk::Label>(),
                header.item().and_downcast::<ConversationObject>(),
            ) else {
                return;
            };
            label.set_label(&conversation.with(|c| i18n::section_label(c.is_pinned(), c.date())));
        });
        imp.conversation_list.set_header_factory(Some(&headers));
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
/// Split the (pinned-then-date-ordered) matches into one store per section:
/// every pinned thread in a first run, then one per calendar day, in the
/// order they arrive. Unreadable dates all fall into a single run.
fn day_sections(matches: Vec<Conversation>) -> Vec<gio::ListStore> {
    let mut sections: Vec<gio::ListStore> = Vec::new();
    let mut current_day = None;
    for conversation in matches {
        let day = if conversation.is_pinned() {
            (true, None)
        } else {
            (false, dates::day_of(conversation.date()))
        };
        if sections.is_empty() || current_day != Some(day) {
            sections.push(gio::ListStore::new::<ConversationObject>());
            current_day = Some(day);
        }
        sections
            .last()
            .expect("pushed above")
            .append(&ConversationObject::new(conversation));
    }
    sections
}

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

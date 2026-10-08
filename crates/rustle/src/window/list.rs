//! The email list: contents, search, and paging.

use super::{MainWindow, PAGE_EMPTY, PAGE_LIST, PAGE_LOADING, SEARCH_DEBOUNCE_MS};
use crate::i18n;
use crate::objects::EmailObject;
use crate::settings as keys;
use crate::widgets::email_row::EmailRow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{gdk, gio};
use rustle_core::dates;
use rustle_core::folders;
use rustle_core::models::Email;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

impl MainWindow {
    /// Rebuild the email list from the current view, applying the
    /// search query if one is typed. The emails in `keep` stay selected
    /// if they're still in the list, so a mail action or a background sync
    /// can refresh without reloading the reader or dropping a
    /// multi-selection.
    pub(super) fn refresh_emails(&self, keep: &[i64]) {
        if self.state().view.is_none() {
            return;
        }
        {
            let mut state = self.state_mut();
            if state.is_row_menu_open {
                state.is_refresh_deferred = true;
                return;
            }
        }
        let scroller = &self.imp().email_scroller;
        let vadjustment = scroller.vadjustment();
        let scroll_position = vadjustment.value();

        let keep: HashSet<i64> = keep.iter().copied().collect();
        let matches = self.matching_emails(&keep);
        self.replace_emails(matches, &keep);
        self.show_list_or_placeholder();
        self.update_reader();
        restore_scroll(&vadjustment, scroll_position);
    }

    /// Refresh the list, leaving whatever is selected selected.
    pub(super) fn refresh_keeping_selection(&self) {
        let keep = self.selected_ids();
        self.refresh_emails(&keep);
    }

    /// The view's emails, narrowed by the search box and filter.
    fn matching_emails(&self, keep: &HashSet<i64>) -> Vec<Email> {
        let imp = self.imp();
        let folder_ids = self.current_folder_ids();
        let query = imp.search_entry.text().trim().to_string();
        let smart = self.state().smart_search.clone();
        let server_ids = {
            let state = self.state();
            if state.typed_matches.0 == query {
                state.typed_matches.1.clone()
            } else {
                HashSet::new()
            }
        };
        let matches = {
            let db = self.db();
            let db = db.borrow();
            let result = if let Some(smart) = &smart {
                db.filter_emails(&folder_ids, &smart.filter, &smart.server_ids)
            } else if query.is_empty() {
                db.emails_in_folders(&folder_ids)
            } else {
                db.search_emails_with(&folder_ids, &query, &server_ids)
            };
            result.unwrap_or_else(|error| {
                log::error!("could not load emails: {error}");
                Vec::new()
            })
        };
        let matches = if self.settings().boolean(keys::GROUP_CONVERSATIONS) {
            group_conversations(matches)
        } else {
            matches
        };
        if !imp.unread_button.is_active() {
            return matches;
        }
        // Keep the emails being read even once they're marked read, so
        // opening a mail here doesn't make it vanish under you.
        matches
            .into_iter()
            .filter(|c| c.is_unread || keep.contains(&c.id))
            .collect()
    }

    /// Swap in the new list, keeping the emails in `keep` selected if they
    /// survived. MultiSelection tracks positions while `keep` tracks the
    /// emails themselves, so the selection is cleared before the store is
    /// spliced and restored by identity afterwards. The rows are new
    /// objects, which the list can't follow its keyboard focus to, so the
    /// focus is put back on its email too.
    fn replace_emails(&self, matches: Vec<Email>, keep: &HashSet<i64>) {
        let ids: Vec<i64> = matches.iter().map(|c| c.id).collect();
        let targets = positions_of(&ids, keep);
        let focus = self
            .focused_email_id()
            .and_then(|id| ids.iter().position(|&listed| listed == id));
        let sections = day_sections(matches);
        let store = self.email_sections();
        let selection = self.selection();
        self.state_mut().is_selection_update_in_progress = true;
        selection.unselect_all();
        store.splice(0, store.n_items(), &sections);
        if !targets.is_empty() {
            let selected = gtk::Bitset::new_empty();
            for position in targets {
                selected.add(position);
            }
            let everything = gtk::Bitset::new_range(0, selection.n_items());
            selection.set_selection(&selected, &everything);
        }
        self.state_mut().is_selection_update_in_progress = false;
        // Scrolls too, but the caller puts the scroll position back after.
        if let Some(position) = focus {
            self.imp()
                .email_list
                .scroll_to(position as u32, gtk::ListScrollFlags::FOCUS, None);
        }
    }

    /// The email whose row has the keyboard focus, if one has.
    fn focused_email_id(&self) -> Option<i64> {
        let focus = GtkWindowExt::focus(self)?;
        if !focus.is_ancestor(&*self.imp().email_list) {
            return None;
        }
        // The list's own row widget takes the focus, around ours.
        let row = focus
            .ancestor(EmailRow::static_type())
            .or_else(|| focus.first_child())
            .and_downcast::<EmailRow>()?;
        Some(row.email_id())
    }

    pub(super) fn show_list_or_placeholder(&self) {
        let page = if self.email_model().n_items() > 0 {
            PAGE_LIST
        } else if self.is_current_account_syncing() {
            PAGE_LOADING
        } else {
            PAGE_EMPTY
        };
        self.imp().email_stack.set_visible_child_name(page);
    }

    /// The pinned day label is laid out by `place_sticky_day`; scrolling and
    /// model changes only ask the overlay for another pass, which runs after
    /// the list has settled at its new position.
    pub(super) fn update_sticky_day(&self) {
        self.imp().email_overlay.queue_allocate();
    }

    /// Where the pinned day label sits this frame. It floats over the top of
    /// the list naming the day at the top edge, so the date stays readable
    /// however far down a long day you've scrolled, and the next day's header
    /// pushes it out as it arrives. GTK's section headers don't stick, hence
    /// the overlay. Parked out of sight at the very top, where the section's
    /// own header is showing, and when the list is empty.
    pub(super) fn place_sticky_day(&self) -> gdk::Rectangle {
        let imp = self.imp();
        let scroller = &imp.email_scroller;
        let label = &imp.sticky_day;
        let day = self.day_at_top();
        if let Some(day) = &day {
            if label.label() != *day {
                label.set_label(day);
            }
        }
        let width = imp.email_overlay.width();
        let (_, height, _, _) = label.measure(gtk::Orientation::Vertical, width);
        let mut y = if day.is_some() && scroller.vadjustment().value() > 0.0 {
            0
        } else {
            -height
        };
        // A header coming up underneath pushes the label off the top.
        let mut child = imp.email_list.first_child();
        while let Some(widget) = child {
            // The list keeps off-screen headers around unmapped, with
            // meaningless bounds.
            let top = day_header_label(&widget)
                .filter(|_| widget.is_mapped())
                .and_then(|_| widget.compute_bounds(&**scroller))
                .map(|bounds| bounds.y().round() as i32);
            if let Some(top) = top.filter(|top| (1..height).contains(top)) {
                y = y.min(top - height);
            }
            child = widget.next_sibling();
        }
        gdk::Rectangle::new(0, y, width, height)
    }

    /// The section label of whatever is at the list's top edge: a row's day,
    /// or a header's own text.
    fn day_at_top(&self) -> Option<String> {
        let scroller = &self.imp().email_scroller;
        // Pick just inside the viewport: its exact top edge can belong to
        // the scroller instead of the first row or section header.
        let hit = scroller.pick(scroller.width() as f64 / 2.0, 1.0, gtk::PickFlags::DEFAULT)?;
        if let Some(row) = hit
            .ancestor(EmailRow::static_type())
            .and_downcast::<EmailRow>()
        {
            return Some(row.day_label());
        }
        let mut widget = Some(hit);
        while let Some(current) = widget {
            if let Some(label) = day_header_label(&current) {
                return Some(label.label().into());
            }
            widget = current.parent();
        }
        None
    }

    /// Debounce keystrokes: query the database ~200ms after typing stops.
    pub(super) fn on_search_changed(&self) {
        if let Some(previous) = self.state_mut().search_timeout.take() {
            previous.remove();
        }
        // Smart Search runs on Enter, not as you type.
        if self.is_smart_typing() {
            return;
        }
        let source = glib::timeout_add_local_once(
            Duration::from_millis(SEARCH_DEBOUNCE_MS),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.state_mut().search_timeout = None;
                    window.refresh_emails(&[]);
                    window.search_server_for_typed();
                }
            ),
        );
        self.state_mut().search_timeout = Some(source);
    }

    /// Potentially thousands of rows, so this uses the scalable GTK4 pattern:
    /// a ListStore of data, a MultiSelection wrapper, and a factory that
    /// recycles a handful of EmailRow widgets as you scroll.
    pub(super) fn setup_email_list(&self) {
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
        imp.email_list.set_model(Some(&self.selection()));

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
                let row = EmailRow::new(window.avatars());
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
                // Drag to a folder in the sidebar to move there.
                let drag = gtk::DragSource::builder()
                    .actions(gdk::DragAction::MOVE)
                    .build();
                let weak_item = item.downgrade();
                drag.connect_prepare(glib::clone!(
                    #[weak]
                    window,
                    #[upgrade_or]
                    None,
                    move |_, _, _| {
                        let payload = window.drag_payload(&weak_item.upgrade()?)?;
                        Some(gdk::ContentProvider::for_value(&payload.to_value()))
                    }
                ));
                row.add_controller(drag);
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
                let (Some(row), Some(email)) = (
                    item.child().and_downcast::<EmailRow>(),
                    item.item().and_downcast::<EmailObject>(),
                ) else {
                    return;
                };
                let is_outgoing = window
                    .current_folder()
                    .is_some_and(|folder| folders::is_outgoing_folder(&folder.name));
                let account = if window.is_unified_view() {
                    window
                        .account_for_folder(email.with(|c| c.folder_id))
                        .map(|(account, _)| account)
                } else {
                    None
                };
                email.with(|c| row.bind(c, is_outgoing, account.as_ref()));
            }
        ));
        imp.email_list.set_factory(Some(&factory));

        // Sections are days (see `day_sections`); each gets a sticky header
        // labelled from its first email.
        let headers = gtk::SignalListItemFactory::new();
        headers.connect_setup(|_, item| {
            let Some(header) = item.downcast_ref::<gtk::ListHeader>() else {
                return;
            };
            let label = gtk::Label::builder()
                .xalign(0.0)
                .css_classes(["email-day-header"])
                .build();
            header.set_child(Some(&label));
        });
        headers.connect_bind(|_, item| {
            let Some(header) = item.downcast_ref::<gtk::ListHeader>() else {
                return;
            };
            let (Some(label), Some(email)) = (
                header.child().and_downcast::<gtk::Label>(),
                header.item().and_downcast::<EmailObject>(),
            ) else {
                return;
            };
            label.set_label(&email.with(|c| i18n::section_label(c.is_pinned, &c.date)));
        });
        imp.email_list.set_header_factory(Some(&headers));
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
                        .unwrap_or(false)
                        // The backfill pages this folder by UID; an offset
                        // fetch on top would only re-download its next batch.
                        && !state
                            .backfills
                            .get(&folder.account_id)
                            .is_some_and(|sweep| sweep.has_queued(folder.id)),
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
/// every pinned email in a first run, then one per calendar day, in the
/// order they arrive. Unreadable dates all fall into a single run.
fn day_sections(matches: Vec<Email>) -> Vec<gio::ListStore> {
    let mut sections: Vec<gio::ListStore> = Vec::new();
    let mut current_day = None;
    for email in matches {
        let day = if email.is_pinned {
            (true, None)
        } else {
            (false, dates::day_of(&email.date))
        };
        if sections.is_empty() || current_day != Some(day) {
            sections.push(gio::ListStore::new::<EmailObject>());
            current_day = Some(day);
        }
        sections
            .last()
            .expect("pushed above")
            .append(&EmailObject::new(email));
    }
    sections
}

/// Runs at the top too: left alone, the list re-anchors on the first email
/// and leaves that day's header scrolled just out of view.
fn restore_scroll(vadjustment: &gtk::Adjustment, position: f64) {
    let vadjustment = vadjustment.clone();
    glib::idle_add_local_once(move || {
        let highest = vadjustment.upper() - vadjustment.page_size();
        vadjustment.set_value(position.min(highest).max(0.0));
    });
}

/// The label of a section header, given the label itself or the list's header
/// widget around it.
fn day_header_label(widget: &gtk::Widget) -> Option<gtk::Label> {
    let is_header = |label: &gtk::Label| label.has_css_class("email-day-header");
    widget
        .downcast_ref::<gtk::Label>()
        .cloned()
        .or_else(|| widget.first_child().and_downcast::<gtk::Label>())
        .filter(is_header)
}

/// One row per conversation: the first of each (the list is sorted pinned
/// first, then newest), counting the rest. Unread when any of it is.
fn group_conversations(matches: Vec<Email>) -> Vec<Email> {
    let items: Vec<(i64, &str, &str)> = matches
        .iter()
        .map(|email| {
            (
                email.id,
                email.thread_root.as_str(),
                email.thread_outlook.as_str(),
            )
        })
        .collect();
    let groups = rustle_core::threads::group(&items);
    let mut sizes: HashMap<i64, (u32, bool)> = HashMap::new();
    for email in &matches {
        let entry = sizes.entry(groups[&email.id]).or_default();
        entry.0 += 1;
        entry.1 |= email.is_unread;
    }
    let mut seen = HashSet::new();
    matches
        .into_iter()
        .filter_map(|mut email| {
            let group = groups[&email.id];
            if !seen.insert(group) {
                return None;
            }
            let (size, any_unread) = sizes[&group];
            email.thread_size = size;
            email.is_unread |= any_unread;
            Some(email)
        })
        .collect()
}

/// The positions in `ids` (the list, in display order) of the emails in
/// `keep`.
fn positions_of(ids: &[i64], keep: &HashSet<i64>) -> Vec<u32> {
    ids.iter()
        .enumerate()
        .filter(|(_, id)| keep.contains(id))
        .map(|(position, _)| position as u32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_follow_the_emails_not_where_they_were() {
        let keep = HashSet::from([7, 3, 9]);
        // New mail on top pushed them down, and 9 went away.
        assert_eq!(positions_of(&[11, 12, 7, 5, 3], &keep), vec![2, 4]);
    }

    #[test]
    fn nothing_kept_selects_nothing() {
        assert!(positions_of(&[1, 2, 3], &HashSet::new()).is_empty());
        assert!(positions_of(&[], &HashSet::from([1])).is_empty());
    }
}

//! The phone layout (the `phone` feature), for phone shells such as
//! omarchy-mobile's: one page at a time, nothing to press at the top, and
//! the controls at the bottom, in thumb reach.
//!
//! It rearranges the desktop window rather than replacing it, so every
//! handler stays where it is:
//! - The list and the reader are always collapsed into one page at a time,
//!   and the folders float over them. The header bars show only the title:
//!   no buttons, no back arrow, no window buttons; the folder rail goes.
//! - The list has the search field and New Message in a bar at the bottom;
//!   the reader has Reply, Reply All, Forward, Archive and Delete, and the
//!   rest under a More menu. The bars are the colour of the shell's bars.
//!   The buttons drive the (hidden) header buttons and their actions, so
//!   sensitivity and behaviour follow the desktop's.
//! - Selecting a message opens the reader (the desktop shows both); back
//!   to the list clears the selection, so the same message opens again.
//! - Refresh, Unread Only, Folders, accounts and preferences are the app's
//!   menu (the menubar, which the phone shell shows from its home bar).
//! - It is called Mail (the window, the welcome page, About); Rustle is the
//!   codename.
//! - `app.go-back`, the shell's back gesture, closes the folders, then
//!   leaves the reader; on the list it is disabled and the shell goes home.

use super::MainWindow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};

/// The header and the bottom bars are the colour of the shell's bars (the
/// theme's dark background, libadwaita's header bar colour).
const CSS: &str = "
.phone-bar { padding: 8px 12px; background-color: var(--headerbar-bg-color); }
.phone-bar button { min-height: 44px; min-width: 44px; }
toolbarview > .top-bar { background-color: var(--headerbar-bg-color); }
";

/// An icon button for a floating row.
fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build()
}

/// A button standing in for `original`: same sensitivity, and a tap clicks it.
fn mirror(original: &gtk::Button, icon: &str, tooltip: &str) -> gtk::Button {
    let button = icon_button(icon, tooltip);
    original
        .bind_property("sensitive", &button, "sensitive")
        .sync_create()
        .build();
    let original = original.downgrade();
    button.connect_clicked(move |_| {
        if let Some(original) = original.upgrade() {
            original.emit_clicked();
        }
    });
    button
}

/// Only the title: no buttons, back arrow or window buttons.
fn title_only(header: &adw::HeaderBar) {
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    header.set_show_back_button(false);
}

/// A bar of controls docked at the bottom of `toolbar`.
fn dock(toolbar: &adw::ToolbarView, row: &gtk::Box) {
    row.add_css_class("phone-bar");
    toolbar.add_bottom_bar(row);
}

impl MainWindow {
    pub(super) fn setup_phone(&self) {
        let imp = self.imp();
        self.set_title(Some("Mail"));
        imp.welcome_page.set_title("Welcome to Mail");
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("a display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
        );

        // One page at a time, also in landscape, where the desktop's
        // breakpoint would open the split again.
        for split in [
            imp.outer_split.upcast_ref::<glib::Object>(),
            imp.inner_split.upcast_ref(),
        ] {
            split.set_property("collapsed", true);
            split.connect_notify_local(Some("collapsed"), |split, _| {
                if !split.property::<bool>("collapsed") {
                    split.set_property("collapsed", true);
                }
            });
        }
        imp.outer_split.set_show_sidebar(false);
        imp.folder_rail.set_visible(false);

        for header in [&*imp.folder_header, &*imp.list_header, &*imp.reader_header] {
            title_only(header);
        }
        // The header buttons stay, hidden, as the floating buttons' targets.
        for widget in [
            imp.refresh_button.upcast_ref::<gtk::Widget>(),
            imp.unread_button.upcast_ref(),
            imp.compose_button.upcast_ref(),
            imp.search_button.upcast_ref(),
        ] {
            widget.set_visible(false);
        }
        for in_box in [
            &*imp.reply_button,
            &*imp.mark_read_button,
            &*imp.archive_button,
        ] {
            if let Some(parent) = in_box.parent() {
                parent.set_visible(false);
            }
        }
        // The folder header's own buttons (hide, main menu).
        let mut child = imp.folder_header.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            hide_buttons(&widget);
        }

        // The list: search and New Message at the bottom.
        imp.search_bar.set_key_capture_widget(None::<&gtk::Widget>);
        imp.search_bar.set_child(None::<&gtk::Widget>);
        imp.search_bar.set_visible(false);
        imp.search_entry
            .set_input_purpose(gtk::InputPurpose::FreeForm);
        let compose = icon_button(
            "mail-message-new-symbolic",
            &imp.compose_button.tooltip_text().unwrap_or_default(),
        );
        compose.add_css_class("suggested-action");
        let original = imp.compose_button.downgrade();
        compose.connect_clicked(move |_| {
            if let Some(original) = original.upgrade() {
                original.emit_clicked();
            }
        });
        let list_row = gtk::Box::builder().spacing(8).build();
        imp.search_entry.set_hexpand(true);
        list_row.append(&*imp.search_entry);
        list_row.append(&compose);
        dock(&imp.list_toolbar, &list_row);

        // The reader: the common actions, then More.
        let reader_row = gtk::Box::builder()
            .spacing(8)
            .halign(gtk::Align::Center)
            .build();
        reader_row.append(&mirror(
            &imp.reply_button,
            "mail-reply-sender-symbolic",
            "Reply",
        ));
        reader_row.append(&mirror(
            &imp.reply_all_button,
            "mail-reply-all-symbolic",
            "Reply All",
        ));
        reader_row.append(&mirror(
            &imp.forward_button,
            "mail-forward-symbolic",
            "Forward",
        ));
        let archive = icon_button("mail-archive-symbolic", "Archive");
        archive.set_action_name(Some("win.archive"));
        reader_row.append(&archive);
        let trash = icon_button("user-trash-symbolic", "Delete");
        trash.set_action_name(Some("win.trash"));
        reader_row.append(&trash);
        let more_menu = gio::Menu::new();
        more_menu.append(Some("Mark Read or Unread"), Some("win.toggle-read"));
        more_menu.append(Some("Star"), Some("win.toggle-star"));
        more_menu.append(Some("Pin"), Some("win.toggle-pin"));
        more_menu.append(Some("Move to Folder…"), Some("win.phone-move"));
        let more = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("More")
            .menu_model(&more_menu)
            .build();
        reader_row.append(&more);
        dock(&imp.reader_toolbar, &reader_row);
        let move_action = gio::SimpleAction::new("phone-move", None);
        let move_button = imp.move_button.downgrade();
        move_action.connect_activate(move |_, _| {
            // The move menu opens from its (hidden) button's place; on a
            // phone, a popover from the floating row's More.
            if let Some(button) = move_button.upgrade() {
                if let Some(popover) = button.popover() {
                    popover.unparent();
                    popover.set_parent(&more);
                    popover.popup();
                }
            }
        });
        imp.move_button
            .bind_property("sensitive", &move_action, "enabled")
            .sync_create()
            .build();
        self.add_action(&move_action);

        // Selecting a message opens it; back to the list clears the
        // selection, so tapping the same one opens it again.
        self.selection().connect_selection_changed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |selection, _, _| {
                if !selection.selection().is_empty()
                    && !window.state().is_selection_update_in_progress
                {
                    window.imp().inner_split.set_show_content(true);
                }
            }
        ));
        imp.inner_split.connect_show_content_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |split| {
                if !split.shows_content() {
                    window.selection().unselect_all();
                }
            }
        ));

        self.setup_phone_menu();
        self.setup_phone_back();
    }

    /// The app's menu, for the shell: what the header buttons and the main
    /// menu held.
    fn setup_phone_menu(&self) {
        let imp = self.imp();
        let refresh = gio::SimpleAction::new("phone-refresh", None);
        let button = imp.refresh_button.downgrade();
        refresh.connect_activate(move |_, _| {
            if let Some(button) = button.upgrade() {
                button.emit_clicked();
            }
        });
        self.add_action(&refresh);
        self.add_action(&gio::PropertyAction::new(
            "phone-unread-only",
            &*imp.unread_button,
            "active",
        ));

        let menu = gio::Menu::new();
        let mail = gio::Menu::new();
        mail.append(Some("Folders"), Some("win.toggle-sidebar"));
        mail.append(Some("Refresh"), Some("win.phone-refresh"));
        mail.append(Some("Unread Only"), Some("win.phone-unread-only"));
        menu.append_section(None, &mail);
        let app_section = gio::Menu::new();
        app_section.append(Some("Manage Accounts"), Some("win.manage-accounts"));
        app_section.append(Some("About Mail"), Some("app.about"));
        menu.append_section(None, &app_section);
        let settings = gio::Menu::new();
        settings.append(Some("Settings"), Some("app.preferences"));
        menu.append_section(None, &settings);
        if let Some(app) = self.application() {
            app.set_menubar(Some(&menu));
        }
        self.set_show_menubar(false);
    }

    /// `app.go-back`: the folders close, then the reader goes back to the
    /// list; disabled on the list, so the shell goes home.
    fn setup_phone_back(&self) {
        let imp = self.imp();
        let Some(app) = self.application() else {
            return;
        };
        let action = gio::SimpleAction::new("go-back", None);
        action.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                let imp = window.imp();
                if imp.outer_split.shows_sidebar() {
                    imp.outer_split.set_show_sidebar(false);
                } else if imp.inner_split.shows_content() {
                    imp.inner_split.set_show_content(false);
                }
            }
        ));
        let update = glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            action,
            move || {
                let imp = window.imp();
                action.set_enabled(
                    imp.outer_split.shows_sidebar() || imp.inner_split.shows_content(),
                );
            }
        );
        update();
        let u = update.clone();
        imp.outer_split.connect_show_sidebar_notify(move |_| u());
        imp.inner_split
            .connect_show_content_notify(move |_| update());
        app.add_action(&action);
    }
}

/// Hide every button inside `widget` (a header bar's packed children).
fn hide_buttons(widget: &gtk::Widget) {
    if widget.is::<gtk::Button>()
        || widget.is::<gtk::MenuButton>()
        || widget.is::<gtk::ToggleButton>()
    {
        widget.set_visible(false);
        return;
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        child = c.next_sibling();
        hide_buttons(&c);
    }
}

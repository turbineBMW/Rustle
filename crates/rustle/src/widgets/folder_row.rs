//! One row of the folder sidebar: an icon, a name, a sync spinner and an
//! unread badge. The same widget draws account headings and the unified inbox.

use crate::i18n::gettext;
use crate::objects::SidebarItem;
use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::*;
use rustle_core::folders;
use rustle_core::models::{Account, Folder};
use std::cell::RefCell;

mod imp {
    use super::*;

    pub struct FolderRow {
        pub icon: gtk::Image,
        pub name: gtk::Label,
        pub spinner: adw::Spinner,
        pub badge: gtk::Label,
        /// Set only for account rows: clicking the address toggles its
        /// folders, the same as the expander arrow next to it.
        pub expandable: RefCell<Option<gtk::TreeListRow>>,
    }

    impl Default for FolderRow {
        fn default() -> Self {
            FolderRow {
                icon: gtk::Image::new(),
                name: gtk::Label::builder()
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
                spinner: adw::Spinner::builder().visible(false).build(),
                badge: gtk::Label::builder().css_classes(["dim-label"]).build(),
                expandable: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FolderRow {
        const NAME: &'static str = "RustleFolderRow";
        type Type = super::FolderRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for FolderRow {
        fn constructed(&self) {
            self.parent_constructed();
            let row = self.obj().clone();
            row.set_orientation(gtk::Orientation::Horizontal);
            row.set_spacing(12);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.append(&self.icon);
            row.append(&self.name);
            row.append(&self.spinner);
            row.append(&self.badge);

            let click = gtk::GestureClick::new();
            click.connect_released(glib::clone!(
                #[weak]
                row,
                move |_, n_press, _, _| {
                    if n_press != 1 {
                        return;
                    }
                    if let Some(expandable) = row.imp().expandable.borrow().as_ref() {
                        expandable.set_expanded(!expandable.is_expanded());
                    }
                }
            ));
            row.add_controller(click);
        }
    }
    impl WidgetImpl for FolderRow {}
    impl BoxImpl for FolderRow {}
}

glib::wrapper! {
    pub struct FolderRow(ObjectSubclass<imp::FolderRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for FolderRow {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl FolderRow {
    /// Fill this row from a folder. Called every time the row is (re)used, so
    /// it undoes whatever `bind_account` set.
    pub fn bind_folder(&self, folder: &Folder, unread_count: i64) {
        let imp = self.imp();
        imp.icon.set_icon_name(Some(&folder.icon_name));
        imp.name.set_label(&folders::display_name_for_folder(
            &folder.name,
            folder.display_delimiter(),
        ));
        imp.name.remove_css_class("heading");
        imp.expandable.replace(None);
        self.set_syncing(false);
        self.set_badge(unread_count);
    }

    pub fn bind_unified_inbox(&self, unread_count: i64) {
        let imp = self.imp();
        imp.icon.set_icon_name(Some("mail-inbox-symbolic"));
        imp.name.set_label(&gettext("All Inboxes"));
        imp.name.remove_css_class("heading");
        imp.expandable.replace(None);
        self.set_syncing(false);
        self.set_badge(unread_count);
    }

    pub fn bind_account(&self, account: &Account, tree_row: &gtk::TreeListRow, is_syncing: bool) {
        let imp = self.imp();
        imp.expandable.replace(Some(tree_row.clone()));
        imp.icon.set_icon_name(Some("avatar-default-symbolic"));
        imp.name.set_label(&account.email);
        imp.name.add_css_class("heading");
        self.set_syncing(is_syncing);
        imp.badge.set_visible(false);
    }

    fn set_badge(&self, unread_count: i64) {
        let badge = &self.imp().badge;
        badge.set_label(&unread_count.to_string());
        badge.set_visible(unread_count > 0);
    }

    /// Only account rows use this; the window calls it as syncs start and finish.
    pub fn set_syncing(&self, is_syncing: bool) {
        self.imp().spinner.set_visible(is_syncing);
    }

    pub fn item_of(list_item: &gtk::ListItem) -> Option<(gtk::TreeListRow, SidebarItem)> {
        let tree_row = list_item.item().and_downcast::<gtk::TreeListRow>()?;
        let item = tree_row.item().and_downcast::<SidebarItem>()?;
        Some((tree_row, item))
    }
}

//! One row of the email list.

use crate::account_colors;
use crate::avatar_loader::AvatarLoader;
use crate::i18n;
use adw::prelude::*;
use gtk::glib;
use gtk::pango;
use gtk::subclass::prelude::*;
use rustle_core::models::{Account, Email};
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;

    pub struct EmailRow {
        pub avatar: adw::Avatar,
        pub sender: gtk::Label,
        pub star: gtk::Image,
        pub pin: gtk::Image,
        pub date: gtk::Label,
        pub subject: gtk::Label,
        pub preview: gtk::Label,
        pub account: gtk::Box,
        pub address: RefCell<String>,
        pub date_value: RefCell<String>,
        pub pinned: Cell<bool>,
        pub avatars: RefCell<Option<AvatarLoader>>,
    }

    impl Default for EmailRow {
        fn default() -> Self {
            let label = |classes: &[&str]| {
                gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::End)
                    .css_classes(classes)
                    .build()
            };
            EmailRow {
                avatar: adw::Avatar::new(40, None, true),
                sender: label(&["email-sender"]),
                star: gtk::Image::builder()
                    .icon_name("starred-symbolic")
                    .pixel_size(12)
                    .build(),
                pin: gtk::Image::builder()
                    .icon_name("view-pin-symbolic")
                    .pixel_size(12)
                    .css_classes(["email-pin"])
                    .build(),
                date: gtk::Label::builder()
                    .xalign(1.0)
                    .css_classes(["dim-label"])
                    .build(),
                subject: label(&["email-subject"]),
                preview: label(&["email-preview", "dim-label"]),
                account: gtk::Box::builder()
                    .width_request(10)
                    .height_request(10)
                    .valign(gtk::Align::Center)
                    .css_classes(["account-dot"])
                    .visible(false)
                    .build(),
                address: RefCell::new(String::new()),
                date_value: RefCell::new(String::new()),
                pinned: Cell::new(false),
                avatars: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EmailRow {
        const NAME: &'static str = "RustleEmailRow";
        type Type = super::EmailRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for EmailRow {
        fn constructed(&self) {
            self.parent_constructed();
            let row = self.obj().clone();
            row.set_orientation(gtk::Orientation::Horizontal);
            row.set_spacing(12);
            // Padding rather than margins, so an unread row's tint runs
            // edge to edge (see .email-row in style.css).
            row.add_css_class("email-row");
            row.append(&self.avatar);

            let text = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .hexpand(true)
                .build();
            row.append(&text);

            let top = gtk::Box::builder().spacing(6).build();
            self.sender.set_hexpand(true);
            top.append(&self.account);
            top.append(&self.sender);
            top.append(&self.star);
            top.append(&self.pin);
            top.append(&self.date);
            text.append(&top);
            text.append(&self.subject);

            text.append(&self.preview);
        }
    }
    impl WidgetImpl for EmailRow {}
    impl BoxImpl for EmailRow {}
}

glib::wrapper! {
    pub struct EmailRow(ObjectSubclass<imp::EmailRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl EmailRow {
    pub fn new(avatars: AvatarLoader) -> Self {
        let row: Self = glib::Object::new();
        row.imp().avatars.replace(Some(avatars));
        row
    }

    /// Fill this row from a email. In an outgoing folder the sender of
    /// every message is the account itself, so the row names the recipient
    /// instead. `account` is given in the unified inbox, where the account a
    /// message belongs to is otherwise invisible: the row then carries a dot
    /// in that account's colour before the sender.
    pub fn bind(&self, email: &Email, is_outgoing: bool, account: Option<&Account>) {
        let imp = self.imp();
        let (name, address) = if is_outgoing && !email.recipient.is_empty() {
            (&email.recipient, &email.recipient_address)
        } else {
            (&email.sender, &email.sender_address)
        };

        imp.avatar.set_text(Some(name));
        self.load_avatar(address);
        imp.sender.set_label(name);
        imp.star.set_visible(email.is_starred);
        imp.pin.set_visible(email.is_pinned);
        imp.pinned.set(email.is_pinned);
        imp.date.set_label(&i18n::time_label(&email.date));
        imp.date_value.replace(email.date.to_string());
        imp.subject.set_label(&email.subject);
        imp.preview.set_label(&email.preview);
        match account {
            Some(account) => {
                imp.account.set_tooltip_text(Some(&account.email));
                account_colors::tag(&imp.account, Some(account.id));
                imp.account.set_visible(true);
            }
            None => imp.account.set_visible(false),
        }
        // The list's own row widget wraps this box; tag it too so the
        // highlight covers the full row, edge to edge.
        if email.is_unread {
            self.add_css_class("unread");
        } else {
            self.remove_css_class("unread");
        }
    }

    /// The section header this row belongs under, for the sticky label.
    pub fn day_label(&self) -> String {
        let imp = self.imp();
        i18n::section_label(imp.pinned.get(), &imp.date_value.borrow())
    }

    fn load_avatar(&self, address: &str) {
        // Rows are recycled, so clear the old face and ignore a fetch that
        // lands after this row was rebound to someone else.
        let imp = self.imp();
        imp.address.replace(address.to_string());
        imp.avatar.set_custom_image(None::<&gtk::gdk::Paintable>);
        let Some(avatars) = imp.avatars.borrow().clone() else {
            return;
        };
        let expected = address.to_string();
        let row = self.downgrade();
        avatars.load(address, move |texture| {
            if let Some(row) = row.upgrade() {
                if *row.imp().address.borrow() == expected {
                    row.imp().avatar.set_custom_image(Some(texture));
                }
            }
        });
    }
}

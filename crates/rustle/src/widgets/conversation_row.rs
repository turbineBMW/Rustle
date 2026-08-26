//! One row of the conversation list.

use crate::avatar_loader::AvatarLoader;
use crate::i18n;
use adw::prelude::*;
use gtk::glib;
use gtk::pango;
use gtk::subclass::prelude::*;
use rustle_core::models::Conversation;
use std::cell::RefCell;

mod imp {
    use super::*;

    pub struct ConversationRow {
        pub avatar: adw::Avatar,
        pub sender: gtk::Label,
        pub star: gtk::Image,
        pub date: gtk::Label,
        pub subject: gtk::Label,
        pub preview: gtk::Label,
        pub unread_dot: gtk::Image,
        pub account: gtk::Label,
        pub address: RefCell<String>,
        pub avatars: RefCell<Option<AvatarLoader>>,
    }

    impl Default for ConversationRow {
        fn default() -> Self {
            let label = |classes: &[&str]| {
                gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::End)
                    .css_classes(classes)
                    .build()
            };
            ConversationRow {
                avatar: adw::Avatar::new(40, None, true),
                sender: label(&["conversation-sender"]),
                star: gtk::Image::builder()
                    .icon_name("starred-symbolic")
                    .pixel_size(12)
                    .build(),
                date: gtk::Label::builder()
                    .xalign(1.0)
                    .css_classes(["dim-label"])
                    .build(),
                subject: label(&["conversation-subject"]),
                preview: label(&["dim-label"]),
                unread_dot: gtk::Image::builder()
                    .icon_name("media-record-symbolic")
                    .pixel_size(10)
                    .valign(gtk::Align::Center)
                    .css_classes(["unread-dot"])
                    .build(),
                account: gtk::Label::builder()
                    .xalign(1.0)
                    .ellipsize(pango::EllipsizeMode::End)
                    .max_width_chars(18)
                    .css_classes(["caption", "dim-label", "conversation-account"])
                    .visible(false)
                    .build(),
                address: RefCell::new(String::new()),
                avatars: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ConversationRow {
        const NAME: &'static str = "RustleConversationRow";
        type Type = super::ConversationRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for ConversationRow {
        fn constructed(&self) {
            self.parent_constructed();
            let row = self.obj().clone();
            row.set_orientation(gtk::Orientation::Horizontal);
            row.set_spacing(12);
            row.set_margin_top(8);
            row.set_margin_bottom(8);
            row.set_margin_start(12);
            row.set_margin_end(12);
            row.append(&self.avatar);

            let text = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .hexpand(true)
                .build();
            row.append(&text);

            let top = gtk::Box::builder().spacing(6).build();
            self.sender.set_hexpand(true);
            top.append(&self.sender);
            top.append(&self.star);
            top.append(&self.date);
            text.append(&top);
            text.append(&self.subject);

            let bottom = gtk::Box::builder().spacing(6).build();
            self.preview.set_hexpand(true);
            bottom.append(&self.preview);
            bottom.append(&self.account);
            bottom.append(&self.unread_dot);
            text.append(&bottom);
        }
    }
    impl WidgetImpl for ConversationRow {}
    impl BoxImpl for ConversationRow {}
}

glib::wrapper! {
    pub struct ConversationRow(ObjectSubclass<imp::ConversationRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl ConversationRow {
    pub fn new(avatars: AvatarLoader) -> Self {
        let row: Self = glib::Object::new();
        row.imp().avatars.replace(Some(avatars));
        row
    }

    /// Fill this row from a conversation. In an outgoing folder the sender of
    /// every message is the account itself, so the row names the recipient
    /// instead. `account_label` is shown in the unified inbox, where the
    /// account a thread belongs to is otherwise invisible.
    pub fn bind(
        &self,
        conversation: &Conversation,
        is_outgoing: bool,
        account_label: Option<&str>,
    ) {
        let imp = self.imp();
        let mut subject = conversation.subject().to_string();
        if conversation.count() > 1 {
            subject = format!("{subject}  ({})", conversation.count());
        }
        let latest = conversation.latest();
        let (name, address, participants) = if is_outgoing && !latest.recipient.is_empty() {
            (
                latest.recipient.clone(),
                latest.recipient_address.clone(),
                latest.recipient.clone(),
            )
        } else {
            (
                latest.sender.clone(),
                latest.sender_address.clone(),
                conversation.participants(),
            )
        };

        imp.avatar.set_text(Some(&name));
        self.load_avatar(&address);
        imp.sender.set_label(&participants);
        imp.star.set_visible(conversation.is_starred());
        imp.date.set_label(&i18n::date_label(conversation.date()));
        imp.subject.set_label(&subject);
        imp.preview.set_label(conversation.preview());
        imp.unread_dot.set_visible(conversation.is_unread());
        match account_label {
            Some(label) => {
                imp.account.set_label(label);
                imp.account.set_visible(true);
            }
            None => imp.account.set_visible(false),
        }
        if conversation.is_unread() {
            self.add_css_class("unread");
        } else {
            self.remove_css_class("unread");
        }
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

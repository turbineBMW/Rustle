//! GObject wrappers around the core's plain records, because `gio::ListStore`
//! and `gtk::TreeListModel` only hold GObjects. Each wraps a `RefCell` so a
//! flag toggle can update the row in place without rebuilding the list.

use gtk::glib;
use gtk::subclass::prelude::*;
use rustle_core::models::{Account, Conversation, Folder};
use std::cell::RefCell;

/// What one row of the folder sidebar stands for.
#[derive(Clone, Debug)]
pub enum SidebarKind {
    /// Every account's inbox, merged.
    UnifiedInbox,
    /// A heading over an account's folders; not selectable.
    Account(Account),
    Folder(Folder),
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SidebarItem {
        pub kind: RefCell<Option<SidebarKind>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SidebarItem {
        const NAME: &'static str = "RustleSidebarItem";
        type Type = super::SidebarItem;
    }

    impl ObjectImpl for SidebarItem {}

    #[derive(Default)]
    pub struct ConversationObject {
        pub conversation: RefCell<Option<Conversation>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ConversationObject {
        const NAME: &'static str = "RustleConversation";
        type Type = super::ConversationObject;
    }

    impl ObjectImpl for ConversationObject {}
}

glib::wrapper! {
    pub struct SidebarItem(ObjectSubclass<imp::SidebarItem>);
}

impl SidebarItem {
    pub fn new(kind: SidebarKind) -> Self {
        let item: Self = glib::Object::new();
        item.imp().kind.replace(Some(kind));
        item
    }

    pub fn kind(&self) -> SidebarKind {
        self.imp()
            .kind
            .borrow()
            .clone()
            .expect("set at construction")
    }

    pub fn folder(&self) -> Option<Folder> {
        match self.kind() {
            SidebarKind::Folder(folder) => Some(folder),
            _ => None,
        }
    }

    pub fn account(&self) -> Option<Account> {
        match self.kind() {
            SidebarKind::Account(account) => Some(account),
            _ => None,
        }
    }

    pub fn is_unified_inbox(&self) -> bool {
        matches!(self.kind(), SidebarKind::UnifiedInbox)
    }
}

glib::wrapper! {
    pub struct ConversationObject(ObjectSubclass<imp::ConversationObject>);
}

impl ConversationObject {
    pub fn new(conversation: Conversation) -> Self {
        let object: Self = glib::Object::new();
        object.imp().conversation.replace(Some(conversation));
        object
    }

    /// Read through the wrapper without cloning the whole thread.
    pub fn with<R>(&self, read: impl FnOnce(&Conversation) -> R) -> R {
        let borrowed = self.imp().conversation.borrow();
        read(borrowed.as_ref().expect("set at construction"))
    }

    pub fn update(&self, write: impl FnOnce(&mut Conversation)) {
        let mut borrowed = self.imp().conversation.borrow_mut();
        write(borrowed.as_mut().expect("set at construction"));
    }

    pub fn get(&self) -> Conversation {
        self.with(Clone::clone)
    }

    pub fn id(&self) -> i64 {
        self.with(Conversation::id)
    }
}

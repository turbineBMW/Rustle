//! GObject wrappers around the core's plain records, because `gio::ListStore`
//! and `gtk::TreeListModel` only hold GObjects. Each wraps a `RefCell` so a
//! flag toggle can update the row in place without rebuilding the list.

use gtk::glib;
use gtk::subclass::prelude::*;
use rustle_core::models::{Account, Email, Folder};
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
    pub struct EmailObject {
        pub email: RefCell<Option<Email>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EmailObject {
        const NAME: &'static str = "RustleEmail";
        type Type = super::EmailObject;
    }

    impl ObjectImpl for EmailObject {}
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
    pub struct EmailObject(ObjectSubclass<imp::EmailObject>);
}

impl EmailObject {
    pub fn new(email: Email) -> Self {
        let object: Self = glib::Object::new();
        object.imp().email.replace(Some(email));
        object
    }

    /// Read through the wrapper without cloning the email.
    pub fn with<R>(&self, read: impl FnOnce(&Email) -> R) -> R {
        let borrowed = self.imp().email.borrow();
        read(borrowed.as_ref().expect("set at construction"))
    }

    pub fn update(&self, write: impl FnOnce(&mut Email)) {
        let mut borrowed = self.imp().email.borrow_mut();
        write(borrowed.as_mut().expect("set at construction"));
    }

    pub fn get(&self) -> Email {
        self.with(Clone::clone)
    }

    pub fn id(&self) -> i64 {
        self.with(|email| email.id)
    }
}

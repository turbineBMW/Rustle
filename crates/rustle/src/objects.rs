//! GObject wrappers around the core's plain records, because `gio::ListStore`
//! and `gtk::TreeListModel` only hold GObjects. Each wraps a `RefCell` so a
//! flag toggle can update the row in place without rebuilding the list.

use crate::i18n::gettext;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use rustle_core::folders::FolderRole;
use rustle_core::models::{Account, Email, Folder};
use std::cell::RefCell;
use std::sync::OnceLock;

/// A folder every account has, shown merged: one row above the accounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unified {
    Inbox,
    Sent,
}

impl Unified {
    /// In the order the sidebar and its collapsed rail list them.
    pub const ALL: [Self; 2] = [Self::Inbox, Self::Sent];

    /// The role of the folder it takes from each account.
    pub fn role(self) -> FolderRole {
        match self {
            Self::Inbox => FolderRole::Inbox,
            Self::Sent => FolderRole::Sent,
        }
    }

    pub fn icon_name(self) -> &'static str {
        match self {
            Self::Inbox => "mail-inbox-symbolic",
            Self::Sent => "mail-send-symbolic",
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Inbox => gettext("All Inboxes"),
            Self::Sent => gettext("All Sent"),
        }
    }
}

/// What one row of the folder sidebar stands for.
#[derive(Clone, Debug)]
pub enum SidebarKind {
    /// Every account's inbox, or its sent mail, merged.
    Unified(Unified),
    /// A heading over an account's folders; not selectable.
    Account(Box<Account>),
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

    impl ObjectImpl for EmailObject {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("changed").build()])
        }
    }
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
            SidebarKind::Account(account) => Some(*account),
            _ => None,
        }
    }

    pub fn unified(&self) -> Option<Unified> {
        match self.kind() {
            SidebarKind::Unified(which) => Some(which),
            _ => None,
        }
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

    /// Say the email changed in place, so the row bound to it redraws
    /// without the list being rebuilt.
    pub fn notify_changed(&self) {
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn connect_changed(&self, handler: impl Fn(&Self) + 'static) -> glib::SignalHandlerId {
        self.connect_closure(
            "changed",
            false,
            glib::closure_local!(move |object: Self| handler(&object)),
        )
    }

    pub fn get(&self) -> Email {
        self.with(Clone::clone)
    }

    pub fn id(&self) -> i64 {
        self.with(|email| email.id)
    }
}

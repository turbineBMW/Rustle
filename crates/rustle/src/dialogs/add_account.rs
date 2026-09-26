//! The first step of adding an account: Online Accounts or manual setup.
//! The chosen dialog opens on top of this one, so cancelling it comes back
//! here; once it adds an account, this one closes too.

use super::account::AccountDialog;
use super::online_accounts::OnlineAccountsDialog;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::db::Database;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/add-account-dialog.ui")]
    pub struct AddAccountDialog {
        #[template_child]
        pub online_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub manual_row: TemplateChild<adw::ActionRow>,
        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        pub has_added: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AddAccountDialog {
        const NAME: &'static str = "RustleAddAccountDialog";
        type Type = super::AddAccountDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AddAccountDialog {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("account-added").build()])
        }

        fn constructed(&self) {
            self.parent_constructed();
            let dialog = self.obj().clone();
            self.online_row.connect_activated(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.on_online_activated()
            ));
            self.manual_row.connect_activated(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.on_manual_activated()
            ));
        }
    }
    impl WidgetImpl for AddAccountDialog {}
    impl AdwDialogImpl for AddAccountDialog {}
}

glib::wrapper! {
    pub struct AddAccountDialog(ObjectSubclass<imp::AddAccountDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl AddAccountDialog {
    pub fn new(db: Rc<RefCell<Database>>) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().db.replace(Some(db));
        dialog
    }

    /// Fires once per account added, from either path.
    pub fn connect_account_added(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_local("account-added", false, move |values| {
            let dialog = values[0].get::<Self>().expect("the emitter");
            callback(&dialog);
            None
        });
    }

    fn db(&self) -> Rc<RefCell<Database>> {
        self.imp().db.borrow().clone().expect("set at construction")
    }

    fn on_account_added(&self) {
        self.imp().has_added.set(true);
        self.emit_by_name::<()>("account-added", &[]);
    }

    /// Online Accounts stays open so several can be added in one go; this
    /// one closes behind it if any were.
    fn after_child_closed(&self) {
        if self.imp().has_added.get() {
            self.close();
        }
    }

    fn on_online_activated(&self) {
        let dialog = OnlineAccountsDialog::new(self.db());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.on_account_added()
        ));
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.after_child_closed()
        ));
        dialog.present(Some(self));
    }

    fn on_manual_activated(&self) {
        let dialog = AccountDialog::new(self.db());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.on_account_added()
        ));
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.after_child_closed()
        ));
        dialog.present(Some(self));
    }
}

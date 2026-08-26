//! The Manage Accounts dialog: the list, with remove and the two add paths.

use super::account::AccountDialog;
use super::online_accounts::OnlineAccountsDialog;
use crate::i18n::gettext;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::db::Database;
use rustle_core::secrets;
use std::cell::RefCell;
use std::rc::Rc;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/accounts-dialog.ui")]
    pub struct AccountsDialog {
        #[template_child]
        pub accounts_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub add_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub online_accounts_button: TemplateChild<gtk::Button>,
        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        pub rows: RefCell<Vec<adw::ActionRow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AccountsDialog {
        const NAME: &'static str = "RustleAccountsDialog";
        type Type = super::AccountsDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AccountsDialog {
        fn constructed(&self) {
            self.parent_constructed();
            let dialog = self.obj().clone();
            self.add_button.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.on_add_clicked()
            ));
            self.online_accounts_button.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.on_online_accounts_clicked()
            ));
        }
    }
    impl WidgetImpl for AccountsDialog {}
    impl AdwDialogImpl for AccountsDialog {}
}

glib::wrapper! {
    pub struct AccountsDialog(ObjectSubclass<imp::AccountsDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl AccountsDialog {
    pub fn new(db: Rc<RefCell<Database>>) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().db.replace(Some(db));
        dialog.reload();
        dialog
    }

    fn db(&self) -> Rc<RefCell<Database>> {
        self.imp().db.borrow().clone().expect("set at construction")
    }

    fn reload(&self) {
        let imp = self.imp();
        for row in imp.rows.borrow_mut().drain(..) {
            imp.accounts_group.remove(&row);
        }
        let accounts = self.db().borrow().accounts().unwrap_or_default();
        for account in accounts {
            let row = adw::ActionRow::builder()
                .title(&account.email)
                .subtitle(&account.display_name)
                .build();
            let remove_button = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Remove Account"))
                .css_classes(["flat"])
                .build();
            let account_id = account.id;
            remove_button.connect_clicked(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |_| dialog.on_remove_clicked(account_id)
            ));
            row.add_suffix(&remove_button);
            imp.accounts_group.add(&row);
            imp.rows.borrow_mut().push(row);
        }
    }

    fn on_remove_clicked(&self, account_id: i64) {
        if let Err(error) = self.db().borrow_mut().delete_account(account_id) {
            log::error!("could not delete account {account_id}: {error}");
            return;
        }
        workers::run(
            move || secrets::clear_password(account_id),
            move |result| {
                if let Err(error) = result {
                    log::warn!(
                        "could not clear the keyring entry of account {account_id}: {error}"
                    );
                }
            },
        );
        self.reload();
    }

    fn on_add_clicked(&self) {
        let dialog = AccountDialog::new(self.db());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.reload()
        ));
        dialog.present(Some(self));
    }

    fn on_online_accounts_clicked(&self) {
        let dialog = OnlineAccountsDialog::new(self.db());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.reload()
        ));
        dialog.present(Some(self));
    }
}

//! The Manage Accounts dialog: the list, with remove and the two add paths.

use super::account::AccountDialog;
use super::online_accounts::OnlineAccountsDialog;
use super::signature::SignatureDialog;
use crate::account_colors;
use crate::i18n::gettext;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, glib};
use rustle_core::db::Database;
use rustle_core::models::Account;
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
        pub rows: RefCell<Vec<adw::ExpanderRow>>,
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
        account_colors::apply(&accounts);
        for account in accounts {
            let row = adw::ExpanderRow::builder()
                .title(account.name())
                .subtitle(if account.label.trim().is_empty() {
                    &account.display_name
                } else {
                    &account.email
                })
                .build();
            row.add_row(&self.name_row(&account));
            row.add_row(&self.signature_row(&account));
            let color_button = gtk::ColorDialogButton::builder()
                .dialog(
                    &gtk::ColorDialog::builder()
                        .title(gettext("Account Colour"))
                        .with_alpha(false)
                        .build(),
                )
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Colour used to mark this account's mail"))
                .build();
            if let Some(rgba) = account_colors::parse_hex(account.color_hex()) {
                color_button.set_rgba(&rgba);
            }
            let account_id = account.id;
            color_button.connect_rgba_notify(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |button| dialog.on_color_picked(account_id, &button.rgba())
            ));
            row.add_suffix(&color_button);
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

    /// What the user calls the account, saved as it is typed. The row's
    /// title follows on the next reload (closing the dialog), like the
    /// sidebar.
    fn name_row(&self, account: &Account) -> gtk::Widget {
        let row = adw::EntryRow::builder()
            .title(gettext("Name"))
            .text(&account.label)
            .build();
        let account_id = account.id;
        row.connect_changed(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |row| dialog.on_label_changed(account_id, &row.text())
        ));
        row.upcast()
    }

    /// One line of the signature and a button into the editor.
    fn signature_row(&self, account: &Account) -> gtk::Widget {
        let preview = rustle_core::html::html_to_text(&account.signature_html());
        let first_line = preview.lines().find(|line| !line.trim().is_empty());
        let row = adw::ActionRow::builder()
            .title(gettext("Signature"))
            .subtitle(first_line.unwrap_or(&gettext("None")))
            .subtitle_lines(1)
            .activatable(true)
            .build();
        let edit = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text(gettext("Edit Signature"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&edit);
        row.set_activatable_widget(Some(&edit));
        let account = account.clone();
        edit.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_edit_signature(&account)
        ));
        row.upcast()
    }

    fn on_edit_signature(&self, account: &Account) {
        let account_id = account.id;
        let editor = SignatureDialog::new(
            account.name(),
            &account.signature_html(),
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |html| {
                    dialog.on_signature_changed(account_id, html);
                    dialog.reload();
                }
            ),
        );
        editor.present(Some(self));
    }

    fn on_label_changed(&self, account_id: i64, label: &str) {
        if let Err(error) = self.db().borrow().set_account_label(account_id, label) {
            log::error!("could not rename account {account_id}: {error}");
        }
    }

    fn on_signature_changed(&self, account_id: i64, signature: &str) {
        if let Err(error) = self
            .db()
            .borrow()
            .set_account_signature(account_id, signature)
        {
            log::error!("could not save the signature of account {account_id}: {error}");
        }
    }

    /// Persist a picked colour and recolour every tagged widget at once.
    fn on_color_picked(&self, account_id: i64, rgba: &gdk::RGBA) {
        let hex = crate::accent::rgba_hex(rgba);
        let db = self.db();
        if let Err(error) = db.borrow().set_account_color(account_id, &hex) {
            log::error!("could not save the colour of account {account_id}: {error}");
            return;
        }
        let accounts = db.borrow().accounts().unwrap_or_default();
        account_colors::apply(&accounts);
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

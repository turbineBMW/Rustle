//! The Add Account dialog: a typed-in IMAP/SMTP account, with the server
//! fields filled in from the address for the providers we know.

use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::db::Database;
use rustle_core::models::{parse_port, NewAccount, Security};
use rustle_core::{providers, secrets};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/account-dialog.ui")]
    pub struct AccountDialog {
        #[template_child]
        pub cancel_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub add_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub display_name_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub email_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub password_row: TemplateChild<adw::PasswordEntryRow>,
        #[template_child]
        pub imap_host_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub imap_port_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub smtp_host_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub smtp_port_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub imap_security_row: TemplateChild<adw::ComboRow>,
        #[template_child]
        pub smtp_security_row: TemplateChild<adw::ComboRow>,
        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        /// Values we put in the server fields ourselves, so a later autofill
        /// can tell its own text from something the user typed.
        pub autofilled_text: RefCell<HashMap<String, String>>,
        pub autofilled_security: RefCell<HashMap<String, u32>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AccountDialog {
        const NAME: &'static str = "RustleAccountDialog";
        type Type = super::AccountDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AccountDialog {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("account-added").build()])
        }

        fn constructed(&self) {
            self.parent_constructed();
            let dialog = self.obj().clone();
            self.cancel_button.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| {
                    dialog.close();
                }
            ));
            self.add_button.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.on_add_clicked()
            ));

            let mut text = self.autofilled_text.borrow_mut();
            for (name, row) in [
                ("imap_host", &self.imap_host_row),
                ("imap_port", &self.imap_port_row),
                ("smtp_host", &self.smtp_host_row),
                ("smtp_port", &self.smtp_port_row),
            ] {
                text.insert(name.to_string(), row.text().to_string());
            }
            drop(text);
            let mut security = self.autofilled_security.borrow_mut();
            security.insert("imap".into(), self.imap_security_row.selected());
            security.insert("smtp".into(), self.smtp_security_row.selected());
            drop(security);

            self.email_row.connect_changed(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.autofill_servers()
            ));
            for row in [
                &self.display_name_row,
                &self.email_row,
                &self.imap_host_row,
                &self.smtp_host_row,
                &self.imap_port_row,
                &self.smtp_port_row,
            ] {
                row.connect_changed(glib::clone!(
                    #[weak]
                    dialog,
                    move |_| dialog.update_add_sensitivity()
                ));
            }
            self.password_row.connect_changed(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.update_add_sensitivity()
            ));
        }
    }
    impl WidgetImpl for AccountDialog {}
    impl AdwDialogImpl for AccountDialog {}
}

glib::wrapper! {
    pub struct AccountDialog(ObjectSubclass<imp::AccountDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl AccountDialog {
    pub fn new(db: Rc<RefCell<Database>>) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().db.replace(Some(db));
        dialog
    }

    pub fn connect_account_added(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_local("account-added", false, move |values| {
            let dialog = values[0].get::<Self>().expect("the emitter");
            callback(&dialog);
            None
        });
    }

    fn autofill_servers(&self) {
        let imp = self.imp();
        let Some(settings) = providers::settings_for_email(&imp.email_row.text()) else {
            return;
        };
        let mut text = imp.autofilled_text.borrow_mut();
        for (name, row, value) in [
            (
                "imap_host",
                &imp.imap_host_row,
                settings.imap_host.to_string(),
            ),
            (
                "imap_port",
                &imp.imap_port_row,
                settings.imap_port.to_string(),
            ),
            (
                "smtp_host",
                &imp.smtp_host_row,
                settings.smtp_host.to_string(),
            ),
            (
                "smtp_port",
                &imp.smtp_port_row,
                settings.smtp_port.to_string(),
            ),
        ] {
            if text
                .get(name)
                .is_some_and(|own| *own == row.text().as_str())
            {
                row.set_text(&value);
                text.insert(name.to_string(), value);
            }
        }
        let mut security = imp.autofilled_security.borrow_mut();
        for (name, combo, value) in [
            ("imap", &imp.imap_security_row, settings.imap_security),
            ("smtp", &imp.smtp_security_row, settings.smtp_security),
        ] {
            if security
                .get(name)
                .is_some_and(|own| *own == combo.selected())
            {
                combo.set_selected(value.index());
                security.insert(name.to_string(), value.index());
            }
        }
    }

    fn update_add_sensitivity(&self) {
        let imp = self.imp();
        let required = [
            imp.display_name_row.text(),
            imp.email_row.text(),
            imp.password_row.text(),
            imp.imap_host_row.text(),
            imp.smtp_host_row.text(),
        ];
        let ports_are_valid = parse_port(&imp.imap_port_row.text()).is_some()
            && parse_port(&imp.smtp_port_row.text()).is_some();
        imp.add_button.set_sensitive(
            ports_are_valid && required.iter().all(|field| !field.trim().is_empty()),
        );
    }

    fn on_add_clicked(&self) {
        let imp = self.imp();
        let (Some(imap_port), Some(smtp_port)) = (
            parse_port(&imp.imap_port_row.text()),
            parse_port(&imp.smtp_port_row.text()),
        ) else {
            return;
        };
        let new_account = NewAccount {
            email: imp.email_row.text().trim().to_string(),
            display_name: imp.display_name_row.text().trim().to_string(),
            imap_host: imp.imap_host_row.text().trim().to_string(),
            imap_port,
            imap_security: Security::from_index(imp.imap_security_row.selected()),
            smtp_host: imp.smtp_host_row.text().trim().to_string(),
            smtp_port,
            smtp_security: Security::from_index(imp.smtp_security_row.selected()),
            goa_id: String::new(),
        };
        let db = imp.db.borrow().clone().expect("set at construction");
        let account = match db.borrow().save_account(&new_account) {
            Ok(account) => account,
            Err(error) => {
                log::error!("could not save account {}: {error}", new_account.email);
                return;
            }
        };
        // The keyring blocks on IPC and may prompt to unlock; keep the main
        // loop responsive while it does.
        let password = imp.password_row.text().to_string();
        let email = account.email.clone();
        workers::run(
            move || secrets::store_password(account.id, &password),
            move |result| {
                if let Err(error) = result {
                    log::error!("could not store the password for {email}: {error}");
                }
            },
        );
        self.emit_by_name::<()>("account-added", &[]);
        self.close();
    }
}

//! Importing an account from GNOME Online Accounts.

use crate::i18n::{self, gettext};
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::db::Database;
use rustle_core::goa::{self, OnlineAccount};
use rustle_core::models::NewAccount;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

const PAGE_LIST: &str = "list";
const PAGE_EMPTY: &str = "empty";

const SETTINGS_BUS_NAME: &str = "org.gnome.Settings";
const SETTINGS_OBJECT_PATH: &str = "/org/gnome/Settings";
const ONLINE_ACCOUNTS_PANEL: &str = "online-accounts";
/// Settings is D-Bus activated, so the first call waits for it to start.
const SETTINGS_TIMEOUT_MS: i32 = 30_000;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/online-accounts-dialog.ui")]
    pub struct OnlineAccountsDialog {
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub accounts_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub accounts_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub settings_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub empty_settings_button: TemplateChild<gtk::Button>,
        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        pub rows: RefCell<Vec<adw::ActionRow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OnlineAccountsDialog {
        const NAME: &'static str = "RustleOnlineAccountsDialog";
        type Type = super::OnlineAccountsDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OnlineAccountsDialog {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("account-added").build()])
        }

        fn constructed(&self) {
            self.parent_constructed();
            let dialog = self.obj().clone();
            for button in [&self.settings_button, &self.empty_settings_button] {
                button.connect_clicked(glib::clone!(
                    #[weak]
                    dialog,
                    move |_| dialog.on_settings_clicked()
                ));
            }
        }
    }
    impl WidgetImpl for OnlineAccountsDialog {}
    impl AdwDialogImpl for OnlineAccountsDialog {}
}

glib::wrapper! {
    pub struct OnlineAccountsDialog(ObjectSubclass<imp::OnlineAccountsDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl OnlineAccountsDialog {
    pub fn new(db: Rc<RefCell<Database>>) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().db.replace(Some(db));
        dialog.reload();
        dialog
    }

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

    /// Listing walks the bus, so it runs off the main thread and fills the
    /// dialog when it lands.
    fn reload(&self) {
        workers::run(
            goa::mail_accounts,
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |accounts| this.populate(accounts)
            ),
        );
    }

    fn populate(&self, accounts: Vec<OnlineAccount>) {
        let imp = self.imp();
        for row in imp.rows.borrow_mut().drain(..) {
            imp.accounts_group.remove(&row);
        }
        imp.accounts_stack
            .set_visible_child_name(if accounts.is_empty() {
                PAGE_EMPTY
            } else {
                PAGE_LIST
            });

        // By address, not goa_id: that also catches the same mailbox already
        // added by hand, which would otherwise sync into a second folder tree.
        let in_use: HashSet<String> = self
            .db()
            .borrow()
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.email)
            .collect();
        for online in accounts {
            let row = adw::ActionRow::builder()
                .title(&online.email)
                .subtitle(&online.provider_name)
                .build();
            if !online.is_mail_enabled {
                row.set_subtitle(&gettext("Mail is turned off for this account in Settings"));
                row.set_sensitive(false);
            } else if !online.is_mail_supported {
                row.set_subtitle(&i18n::format(
                    &gettext("{provider} accounts don't allow IMAP mail access"),
                    &[("provider", &online.provider_name)],
                ));
                row.set_sensitive(false);
            } else if !online.is_oauth2 {
                row.set_subtitle(&gettext("Use Add Account to set this one up"));
                row.set_sensitive(false);
            } else if in_use.contains(&online.email) {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(gettext("Added"))
                        .valign(gtk::Align::Center)
                        .build(),
                );
                row.set_sensitive(false);
            } else {
                let add_button = gtk::Button::builder()
                    .label(gettext("Add"))
                    .valign(gtk::Align::Center)
                    .css_classes(["suggested-action"])
                    .build();
                add_button.connect_clicked(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    move |_| this.on_add_clicked(&online)
                ));
                row.add_suffix(&add_button);
            }
            imp.accounts_group.add(&row);
            imp.rows.borrow_mut().push(row);
        }
    }

    fn on_add_clicked(&self, online: &OnlineAccount) {
        let new_account = NewAccount {
            email: online.email.clone(),
            display_name: online.display_name.clone(),
            imap_host: online.imap_host.clone(),
            imap_port: online.imap_port,
            imap_security: online.imap_security,
            smtp_host: online.smtp_host.clone(),
            smtp_port: online.smtp_port,
            smtp_security: online.smtp_security,
            goa_id: online.goa_id.clone(),
        };
        if let Err(error) = self.db().borrow().save_account(&new_account) {
            log::error!("could not save online account {}: {error}", online.email);
            return;
        }
        self.reload();
        self.emit_by_name::<()>("account-added", &[]);
    }

    /// Called rather than fired through an action group so that a missing or
    /// unreachable Settings comes back as an error we can show.
    fn on_settings_clicked(&self) {
        let panel = glib::Variant::tuple_from_iter([
            ONLINE_ACCOUNTS_PANEL.to_variant(),
            Vec::<glib::Variant>::new().to_variant(),
        ]);
        let parameters = glib::Variant::tuple_from_iter([
            "launch-panel".to_variant(),
            glib::Variant::array_from_iter::<glib::Variant>([panel]),
            glib::VariantDict::new(None).end(),
        ]);
        let this = self.downgrade();
        glib::spawn_future_local(async move {
            let result = async {
                let bus = gio::bus_get_future(gio::BusType::Session).await?;
                bus.call_future(
                    Some(SETTINGS_BUS_NAME),
                    SETTINGS_OBJECT_PATH,
                    "org.freedesktop.Application",
                    "ActivateAction",
                    Some(&parameters),
                    None,
                    gio::DBusCallFlags::NONE,
                    SETTINGS_TIMEOUT_MS,
                )
                .await
            }
            .await;
            if let Err(error) = result {
                log::warn!("could not open the Online Accounts panel: {error}");
                if let Some(this) = this.upgrade() {
                    this.imp()
                        .toast_overlay
                        .add_toast(adw::Toast::new(&gettext("Could not open Settings.")));
                }
            }
        });
    }
}

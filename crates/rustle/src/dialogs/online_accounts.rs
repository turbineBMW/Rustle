//! The mail accounts on this computer: every one in Evolution Data Server,
//! which is where GNOME Online Accounts, graphmail-bridge and Rustle itself
//! keep them. New ones are added in Online Accounts (this dialog opens it)
//! and appear by themselves; ones removed from Rustle can be shown again
//! here. When the pieces it needs aren't installed, the dialog says which
//! and how to install them instead.

use crate::i18n::{self, gettext};
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::db::Database;
use rustle_core::eds::{self, MailAccount};
use rustle_core::goa_setup::{self, Component, Desktop, Setup, STANDALONE_SETTINGS};
use std::cell::RefCell;
use std::rc::Rc;

const PAGE_LOADING: &str = "loading";
const PAGE_SETUP: &str = "setup";
const PAGE_LIST: &str = "list";
const PAGE_EMPTY: &str = "empty";

const SETTINGS_BUS_NAME: &str = "org.gnome.Settings";
const SETTINGS_OBJECT_PATH: &str = "/org/gnome/Settings";
const ONLINE_ACCOUNTS_PANEL: &str = "online-accounts";
/// Settings is D-Bus activated, so the first call waits for it to start.
const SETTINGS_TIMEOUT_MS: i32 = 30_000;
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";

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
        pub setup_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub install_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub command_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub copy_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub check_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub accounts_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub settings_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub empty_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub empty_settings_button: TemplateChild<gtk::Button>,
        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        pub rows: RefCell<Vec<adw::ActionRow>>,
        pub setup_rows: RefCell<Vec<adw::ActionRow>>,
        /// Refill the list when an account is added or removed elsewhere,
        /// such as in the Settings window this dialog opens.
        pub subscriptions: RefCell<Vec<gio::SignalSubscription>>,
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
            self.copy_button.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| dialog.on_copy_clicked()
            ));
            self.check_button.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| {
                    dialog
                        .imp()
                        .accounts_stack
                        .set_visible_child_name(PAGE_LOADING);
                    dialog.reload();
                }
            ));
            dialog.watch_accounts();
            let desktop = Desktop::detect();
            self.accounts_group
                .set_title(&gettext("Mail Accounts on This Computer"));
            self.empty_page.set_description(Some(&match desktop {
                Desktop::Gnome => {
                    gettext("Connect an account in GNOME Settings and it will show up here.")
                }
                Desktop::Other => gettext(
                    "Connect an account in the Online Accounts window and it will show up here.",
                ),
                Desktop::Phone => gettext(
                    "Connect an account in Settings, under Online Accounts, and it will show up here.",
                ),
            }));
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

    /// Checking and listing walk the bus, so they run off the main thread
    /// and fill the dialog when they land.
    fn reload(&self) {
        workers::run(
            || {
                let setup = goa_setup::check();
                let accounts = if setup.is_ready() {
                    eds::mail_accounts().unwrap_or_else(|error| {
                        log::warn!("could not read Evolution Data Server: {error}");
                        Vec::new()
                    })
                } else {
                    Vec::new()
                };
                (setup, accounts)
            },
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |(setup, accounts): (Setup, Vec<MailAccount>)| {
                    if setup.is_ready() {
                        this.populate(accounts);
                    } else {
                        this.show_setup(&setup);
                    }
                }
            ),
        );
    }

    /// Online Accounts announces a new account at once; EDS a moment later,
    /// with the mail sources this list is made of.
    fn watch_accounts(&self) {
        let bus = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
            Ok(bus) => bus,
            Err(error) => {
                log::debug!("no session bus to watch accounts on: {error}");
                return;
            }
        };
        let mut subscriptions = Vec::new();
        for (sender, path) in [
            (goa_setup::GOA_BUS_NAME, Some(goa_setup::GOA_OBJECT_PATH)),
            (eds::BUS_NAME, Some(eds::OBJECT_PATH)),
        ] {
            let this = self.downgrade();
            subscriptions.push(bus.subscribe_to_signal(
                Some(sender),
                Some(OBJECT_MANAGER),
                None,
                path,
                None,
                gio::DBusSignalFlags::NONE,
                move |_| {
                    if let Some(this) = this.upgrade() {
                        this.reload();
                    }
                },
            ));
        }
        self.imp().subscriptions.replace(subscriptions);
    }

    fn show_setup(&self, setup: &Setup) {
        let imp = self.imp();
        imp.setup_group.set_description(Some(&match setup.desktop {
            Desktop::Gnome => gettext(
                "Rustle signs in through GNOME Online Accounts, but some of what it needs is missing.",
            ),
            Desktop::Other => i18n::format(
                &gettext("Outside GNOME, Rustle signs in through GNOME Online Accounts and its standalone window, {app}. Some of what it needs is missing."),
                &[("app", STANDALONE_SETTINGS)],
            ),
            Desktop::Phone => gettext(
                "Rustle signs in through GNOME Online Accounts, which isn't running.",
            ),
        }));
        for row in imp.setup_rows.borrow_mut().drain(..) {
            imp.setup_group.remove(&row);
        }
        for component in &setup.missing {
            let row = adw::ActionRow::builder()
                .title(component_name(*component))
                .subtitle(component.package())
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
            imp.setup_group.add(&row);
            imp.setup_rows.borrow_mut().push(row);
        }
        match setup.install_command() {
            Some(command) => {
                imp.install_group
                    .set_description(Some(&gettext("Run this in a terminal, then check again.")));
                imp.command_row.set_title(&command);
            }
            None => {
                imp.install_group.set_description(Some(&gettext(
                    "Install these packages with your distribution's package manager, then check again.",
                )));
                imp.command_row.set_title(&setup.packages().join(" "));
            }
        }
        imp.accounts_stack.set_visible_child_name(PAGE_SETUP);
    }

    fn on_copy_clicked(&self) {
        let imp = self.imp();
        self.clipboard().set_text(&imp.command_row.title());
        imp.toast_overlay
            .add_toast(adw::Toast::new(&gettext("Copied to clipboard")));
    }

    fn populate(&self, accounts: Vec<MailAccount>) {
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
        let db = self.db();
        let shown = db.borrow().accounts().unwrap_or_default();
        let hidden = db.borrow().hidden_accounts().unwrap_or_default();
        let all = Rc::new(accounts);
        for account in all.iter() {
            let row = adw::ActionRow::builder()
                .title(&account.email)
                .subtitle(&account.label)
                .build();
            if shown.iter().any(|row| row.eds_uid == account.uid) {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(gettext("Added"))
                        .valign(gtk::Align::Center)
                        .build(),
                );
                row.set_sensitive(false);
            } else {
                let is_hidden = hidden.iter().any(|row| row.eds_uid == account.uid);
                let add_button = gtk::Button::builder()
                    .label(if is_hidden {
                        gettext("Show")
                    } else {
                        gettext("Add")
                    })
                    .valign(gtk::Align::Center)
                    .css_classes(["suggested-action"])
                    .build();
                let uid = account.uid.clone();
                let all = all.clone();
                add_button.connect_clicked(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    move |_| this.on_add_clicked(&uid, &all)
                ));
                row.add_suffix(&add_button);
            }
            imp.accounts_group.add(&row);
            imp.rows.borrow_mut().push(row);
        }
    }

    /// Show an account again, or pick one up before the window has: either
    /// way the database is brought in line with the whole list first.
    fn on_add_clicked(&self, uid: &str, accounts: &[MailAccount]) {
        let db = self.db();
        if let Err(error) = db.borrow_mut().reconcile_eds(accounts) {
            log::error!("could not update accounts from Evolution Data Server: {error}");
            return;
        }
        let hidden = db.borrow().hidden_accounts().unwrap_or_default();
        if let Some(account) = hidden.iter().find(|account| account.eds_uid == uid) {
            if let Err(error) = db.borrow().show_account(account.id) {
                log::error!("could not show account {}: {error}", account.email);
                return;
            }
        }
        self.reload();
        self.emit_by_name::<()>("account-added", &[]);
    }

    /// GNOME Settings' panel on GNOME, the standalone window anywhere else:
    /// Settings refuses to run outside GNOME.
    fn on_settings_clicked(&self) {
        if Desktop::detect() == Desktop::Phone {
            self.open_phone_settings();
            return;
        }
        if Desktop::detect() == Desktop::Other {
            if let Err(error) = std::process::Command::new(STANDALONE_SETTINGS).spawn() {
                log::warn!("could not launch {STANDALONE_SETTINGS}: {error}");
                self.imp()
                    .toast_overlay
                    .add_toast(adw::Toast::new(&gettext("Could not open Online Accounts.")));
            }
            return;
        }
        self.open_settings_panel();
    }

    /// The phone's settings, at its Online Accounts: a link the phone build
    /// names (`RUSTLE_ACCOUNTS_URI` at build time, such as omarchy-mobile's
    /// `omarchy-settings:accounts`), opened through the portal.
    fn open_phone_settings(&self) {
        let toast = gettext("Open Settings and add the account under Online Accounts.");
        let Some(uri) = option_env!("RUSTLE_ACCOUNTS_URI") else {
            self.imp().toast_overlay.add_toast(adw::Toast::new(&toast));
            return;
        };
        let this = self.downgrade();
        let window = self.root().and_downcast::<gtk::Window>();
        gtk::UriLauncher::new(uri).launch(window.as_ref(), gio::Cancellable::NONE, move |result| {
            if let (Err(error), Some(this)) = (result, this.upgrade()) {
                log::warn!("could not open the phone's settings ({uri}): {error}");
                this.imp().toast_overlay.add_toast(adw::Toast::new(&toast));
            }
        });
    }

    /// Called rather than fired through an action group so that a missing or
    /// unreachable Settings comes back as an error we can show.
    fn open_settings_panel(&self) {
        let panel = glib::Variant::tuple_from_iter([
            ONLINE_ACCOUNTS_PANEL.to_variant(),
            Vec::<glib::Variant>::new().to_variant(),
        ]);
        let parameters = glib::Variant::tuple_from_iter([
            "launch-panel".to_variant(),
            glib::Variant::array_from_iter::<glib::Variant>([panel.to_variant()]),
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
                log::debug!("could not open the Online Accounts panel: {error}");
                if let Err(error) = std::process::Command::new(STANDALONE_SETTINGS).spawn() {
                    log::warn!("could not launch {STANDALONE_SETTINGS}: {error}");
                } else {
                    return;
                }
                if let Some(this) = this.upgrade() {
                    this.imp()
                        .toast_overlay
                        .add_toast(adw::Toast::new(&gettext("Could not open Settings.")));
                }
            }
        });
    }
}

fn component_name(component: Component) -> String {
    match component {
        Component::Daemon => gettext("Online Accounts service"),
        Component::DataServer => gettext("Evolution Data Server"),
        Component::GnomeSettings => gettext("GNOME Settings"),
        Component::StandaloneSettings => gettext("Online Accounts window"),
        Component::Keyring => gettext("Keyring"),
    }
}

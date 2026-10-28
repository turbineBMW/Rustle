//! Adding a Microsoft 365 account through the built-in graphmail-bridge
//! (src/bridge.rs): the address, how to sign in -- as Microsoft Office,
//! which works where a tenant blocks other apps, or with a GNOME Online
//! Accounts sign-in where it doesn't -- then the device code to enter at
//! Microsoft, and the account lands in Evolution Data Server like any other.
//! Without the data server (and a keyring for the bridge's secrets) there is
//! nowhere to land, so the dialog checks for them first and says what to
//! install instead of signing in.

use crate::bridge::{self, GoaAccount};
use crate::i18n::{self, gettext};
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use graphmail_bridge::config::{AppPaths, AuthProfile};
use graphmail_bridge::oauth::DeviceCode;
use graphmail_bridge::setup::{self, NewAccount};
use gtk::{gio, glib};
use rustle_core::goa_setup::{self, Component, Setup};
use std::cell::RefCell;

const PAGE_LOADING: &str = "loading";
const PAGE_SETUP: &str = "setup";
const PAGE_FORM: &str = "form";
const PAGE_CODE: &str = "code";

/// What the bridge can't add an account without: the registry the account
/// goes into, and the keyring its tokens and bridge password are kept in.
const REQUIRED: [Component; 2] = [Component::DataServer, Component::Keyring];

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BridgeAccountDialog {
        pub stack: gtk::Stack,
        pub email_row: adw::EntryRow,
        pub office_check: gtk::CheckButton,
        /// One check per GOA Microsoft 365 account, grouped with Office's.
        pub goa_checks: RefCell<Vec<(gtk::CheckButton, GoaAccount)>>,
        pub sign_in_button: gtk::Button,
        pub error_label: gtk::Label,
        pub code_label: gtk::Label,
        pub code_url: RefCell<String>,
        pub setup_group: adw::PreferencesGroup,
        pub install_group: adw::PreferencesGroup,
        pub command_row: adw::ActionRow,
        pub setup_rows: RefCell<Vec<adw::ActionRow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BridgeAccountDialog {
        const NAME: &'static str = "RustleBridgeAccountDialog";
        type Type = super::BridgeAccountDialog;
        type ParentType = adw::Dialog;
    }

    impl ObjectImpl for BridgeAccountDialog {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("account-added").build()])
        }

        fn constructed(&self) {
            self.parent_constructed();
            let dialog = self.obj();
            dialog.build();
            dialog.check_setup(|dialog| {
                dialog.imp().stack.set_visible_child_name(PAGE_FORM);
            });
        }
    }
    impl WidgetImpl for BridgeAccountDialog {}
    impl AdwDialogImpl for BridgeAccountDialog {}
}

glib::wrapper! {
    pub struct BridgeAccountDialog(ObjectSubclass<imp::BridgeAccountDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for BridgeAccountDialog {
    fn default() -> Self {
        glib::Object::builder()
            .property("title", gettext("Microsoft 365"))
            .property("content-width", 460)
            .build()
    }
}

impl BridgeAccountDialog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fires once the account is in EDS and the bridge serves it.
    pub fn connect_account_added(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_local("account-added", false, move |values| {
            let dialog = values[0].get::<Self>().expect("the emitter");
            callback(&dialog);
            None
        });
    }

    fn build(&self) {
        let imp = self.imp();
        let page = adw::PreferencesPage::new();

        let account = adw::PreferencesGroup::builder()
            .description(gettext(
                "For a work or school account whose organisation won't let other mail apps in. Rustle reaches it through a bridge built into the app, which keeps it working while Rustle runs.",
            ))
            .build();
        imp.email_row.set_title(&gettext("Email Address"));
        imp.email_row.set_input_purpose(gtk::InputPurpose::Email);
        account.add(&imp.email_row);
        page.add(&account);

        let method = adw::PreferencesGroup::builder()
            .title(gettext("Sign In With"))
            .build();
        imp.office_check.set_active(true);
        method.add(&choice_row(
            &imp.office_check,
            &gettext("Microsoft Office"),
            &gettext(
                "Works where other apps are blocked. Your organisation's sign-in log will show Microsoft Office.",
            ),
        ));
        for goa in bridge::goa_microsoft_accounts() {
            let check = gtk::CheckButton::new();
            check.set_group(Some(&imp.office_check));
            method.add(&choice_row(
                &check,
                &i18n::format(&gettext("Online Accounts: {email}"), &[("email", &goa.email)]),
                &gettext("The Microsoft 365 sign-in your desktop already has, where your organisation allows it."),
            ));
            imp.goa_checks.borrow_mut().push((check, goa));
        }
        page.add(&method);

        let actions = adw::PreferencesGroup::new();
        imp.error_label.set_wrap(true);
        imp.error_label.set_xalign(0.0);
        imp.error_label.add_css_class("error");
        imp.error_label.set_visible(false);
        imp.sign_in_button.set_label(&gettext("Sign In"));
        imp.sign_in_button.add_css_class("suggested-action");
        imp.sign_in_button.add_css_class("pill");
        imp.sign_in_button.set_halign(gtk::Align::Center);
        imp.sign_in_button.set_margin_top(12);
        imp.sign_in_button.set_sensitive(false);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
        column.append(&imp.error_label);
        column.append(&imp.sign_in_button);
        actions.add(&column);
        page.add(&actions);

        imp.email_row.connect_changed(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |row| {
                let text = row.text();
                let email = text.trim();
                dialog
                    .imp()
                    .sign_in_button
                    .set_sensitive(email.contains('@') && !email.ends_with('@'));
                // An Online Accounts sign-in for this very address is the
                // natural pick.
                for (check, goa) in dialog.imp().goa_checks.borrow().iter() {
                    if goa.email.eq_ignore_ascii_case(email) {
                        check.set_active(true);
                    }
                }
            }
        ));
        imp.sign_in_button.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.sign_in()
        ));

        let waiting = adw::StatusPage::builder()
            .icon_name("dialog-password-symbolic")
            .title(gettext("Sign In at Microsoft"))
            .description(gettext(
                "Enter this code on the page that opened, then finish signing in there.",
            ))
            .build();
        imp.code_label.add_css_class("title-1");
        imp.code_label.set_selectable(true);
        let copy = gtk::Button::with_label(&gettext("Copy Code"));
        copy.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |button| button
                .clipboard()
                .set_text(&dialog.imp().code_label.label())
        ));
        let open = gtk::Button::with_label(&gettext("Open Page Again"));
        open.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.open_code_page()
        ));
        let buttons = gtk::Box::builder()
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();
        for button in [&copy, &open] {
            button.add_css_class("pill");
            buttons.append(button);
        }
        let spinner = adw::Spinner::builder().height_request(32).build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        column.append(&imp.code_label);
        column.append(&buttons);
        column.append(&spinner);
        waiting.set_child(Some(&column));

        imp.stack
            .add_named(&self.build_setup_page(), Some(PAGE_SETUP));
        imp.stack.add_named(
            &adw::Spinner::builder()
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .width_request(32)
                .height_request(32)
                .build(),
            Some(PAGE_LOADING),
        );
        imp.stack.add_named(&page, Some(PAGE_FORM));
        imp.stack.add_named(&waiting, Some(PAGE_CODE));
        imp.stack.set_visible_child_name(PAGE_LOADING);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&imp.stack));
        self.set_child(Some(&toolbar));
    }

    /// What's missing for the account to land anywhere, laid out like the
    /// Online Accounts dialog's: the pieces, one line to install them, and
    /// a button to look again.
    fn build_setup_page(&self) -> adw::PreferencesPage {
        let imp = self.imp();
        let page = adw::PreferencesPage::new();
        imp.setup_group
            .set_title(&gettext("Evolution Data Server Isn't Installed"));
        imp.setup_group.set_description(Some(&gettext(
            "Rustle keeps its accounts in Evolution Data Server, and the bridge keeps its sign-in in the keyring. Some of what it needs is missing.",
        )));
        page.add(&imp.setup_group);

        imp.install_group.set_title(&gettext("Install"));
        imp.command_row.set_title_selectable(true);
        imp.command_row.set_use_markup(false);
        imp.command_row.add_css_class("monospace");
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(gettext("Copy"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        copy.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |button| button
                .clipboard()
                .set_text(&dialog.imp().command_row.title())
        ));
        imp.command_row.add_suffix(&copy);
        imp.install_group.add(&imp.command_row);
        page.add(&imp.install_group);

        let check = gtk::Button::builder()
            .label(gettext("Check Again"))
            .halign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        check.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| {
                dialog.imp().stack.set_visible_child_name(PAGE_LOADING);
                dialog.check_setup(|dialog| {
                    dialog.imp().stack.set_visible_child_name(PAGE_FORM);
                });
            }
        ));
        let actions = adw::PreferencesGroup::new();
        actions.add(&check);
        page.add(&actions);
        page
    }

    /// Look for the data server and keyring off the main thread; show
    /// what's missing, or carry on with `on_ready`.
    fn check_setup(&self, on_ready: impl FnOnce(&Self) + 'static) {
        workers::run(
            || goa_setup::check_components(&REQUIRED),
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |setup: Setup| {
                    if setup.is_ready() {
                        on_ready(&dialog);
                    } else {
                        dialog.show_setup(&setup);
                    }
                }
            ),
        );
    }

    fn show_setup(&self, setup: &Setup) {
        let imp = self.imp();
        for row in imp.setup_rows.borrow_mut().drain(..) {
            imp.setup_group.remove(&row);
        }
        for component in &setup.missing {
            let row = adw::ActionRow::builder()
                .title(match component {
                    Component::DataServer => gettext("Evolution Data Server"),
                    Component::Keyring => gettext("Keyring"),
                    Component::Daemon => gettext("Online Accounts service"),
                    Component::GnomeSettings => gettext("GNOME Settings"),
                    Component::StandaloneSettings => gettext("Online Accounts window"),
                })
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
        log::warn!(
            "a Microsoft 365 account can't be added: missing {}",
            setup.packages().join(", ")
        );
        imp.stack.set_visible_child_name(PAGE_SETUP);
    }

    fn sign_in(&self) {
        let imp = self.imp();
        let email = imp.email_row.text().trim().to_string();
        let goa = imp
            .goa_checks
            .borrow()
            .iter()
            .find(|(check, _)| check.is_active())
            .map(|(_, goa)| goa.clone());
        let new = NewAccount {
            name: bridge::account_name_for(&email),
            email: email.clone(),
            auth_profile: if goa.is_some() {
                AuthProfile::Goa
            } else {
                AuthProfile::MicrosoftOffice
            },
            tenant: None,
            client_id: None,
            goa_account: goa.map(|goa| goa.id),
            file_secrets: false,
        };
        imp.error_label.set_visible(false);
        imp.sign_in_button.set_sensitive(false);
        let shown: glib::SendWeakRef<BridgeAccountDialog> = self.downgrade().into();
        workers::run(
            move || -> Result<Vec<(String, String)>, String> {
                let tokio = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                let paths = AppPaths::discover().map_err(|error| format!("{error:#}"))?;
                let on_code = move |code: &DeviceCode| {
                    let shown = shown.clone();
                    let code = code.clone();
                    glib::MainContext::default().invoke(move || {
                        if let Some(dialog) = shown.upgrade() {
                            dialog.show_code(&code);
                        }
                    });
                };
                tokio
                    .block_on(async {
                        let (account, address) = setup::add_account(&paths, new, &on_code).await?;
                        log::info!("the bridge signed in to Microsoft 365 as {address}");
                        setup::eds_sources(&paths, &account.name).await
                    })
                    .map_err(|error| {
                        log::error!(
                            "could not add a Microsoft 365 account through the bridge: {error:#}"
                        );
                        format!("{error:#}")
                    })
            },
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |result: Result<Vec<(String, String)>, String>| match result {
                    Ok(sources) => dialog.finish(&sources),
                    Err(message) => dialog.show_error(&message),
                }
            ),
        );
    }

    fn show_code(&self, code: &DeviceCode) {
        let imp = self.imp();
        imp.code_label.set_label(&code.user_code);
        imp.code_url.replace(code.verification_url.clone());
        imp.stack.set_visible_child_name(PAGE_CODE);
        self.open_code_page();
    }

    fn open_code_page(&self) {
        let url = self.imp().code_url.borrow().clone();
        if url.is_empty() {
            return;
        }
        let window = self.root().and_downcast::<gtk::Window>();
        gtk::UriLauncher::new(&url).launch(window.as_ref(), gio::Cancellable::NONE, |_| {});
    }

    fn show_error(&self, message: &str) {
        let imp = self.imp();
        imp.stack.set_visible_child_name(PAGE_FORM);
        imp.error_label.set_label(&i18n::format(
            &gettext("Couldn't add the account: {msg}"),
            &[("msg", message)],
        ));
        imp.error_label.set_visible(true);
        imp.sign_in_button.set_sensitive(true);
    }

    /// Signed in: serve the account, and hand it to EDS through the
    /// registry, where Rustle's account list picks it up. Without the
    /// registry the sign-in is kept (the bridge has it) but the account is
    /// nowhere Rustle can see, so that is an error, not a finished add.
    fn finish(&self, sources: &[(String, String)]) {
        bridge::restart();
        if let Err(error) = rustle_core::eds::create_sources(sources) {
            log::warn!("could not create the bridged account's EDS sources: {error}");
            // With the registry running, the sources are already there from
            // an earlier add, most likely, and the bridge's fresh password
            // is stored either way. Without it, the account is nowhere
            // Rustle can see: say what's missing instead of closing.
            self.check_setup(|dialog| {
                dialog.emit_by_name::<()>("account-added", &[]);
                dialog.close();
            });
            return;
        }
        self.emit_by_name::<()>("account-added", &[]);
        self.close();
    }
}

/// A row picked by its check button.
fn choice_row(check: &gtk::CheckButton, title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(title))
        .subtitle(glib::markup_escape_text(subtitle))
        .activatable_widget(check)
        .build();
    check.set_valign(gtk::Align::Center);
    row.add_prefix(check);
    row
}

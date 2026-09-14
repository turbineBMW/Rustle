//! The application: one database and settings object shared by every
//! window, the app-level actions, and the mailto: entry point.

use crate::composer::ComposerWindow;
use crate::config::{APP_ID, RESOURCE_PATH, VERSION};
use crate::dialogs::preferences::PreferencesDialog;
use crate::i18n::gettext;
use crate::media_activity::MediaActivity;
use crate::settings;
use crate::window::MainWindow;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::db::Database;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

const MAILTO_SCHEME: &str = "mailto:";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RustleApplication {
        pub db: OnceCell<Rc<RefCell<Database>>>,
        pub settings: OnceCell<gio::Settings>,
        pub media_activity: OnceCell<MediaActivity>,
        /// For autostart: build the window (so the sync timer runs) but skip
        /// presenting it. The Background portal puts this flag in the
        /// autostart entry it writes -- see "Start at Login" in preferences.
        pub should_start_hidden: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RustleApplication {
        const NAME: &'static str = "RustleApplication";
        type Type = super::RustleApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for RustleApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let app = self.obj();
            app.add_main_option(
                "hidden",
                glib::Char::from(0u8),
                glib::OptionFlags::NONE,
                glib::OptionArg::None,
                &gettext("Start in the background without showing a window"),
                None,
            );
            app.setup_actions();
        }
    }

    impl ApplicationImpl for RustleApplication {
        fn startup(&self) {
            self.parent_startup();
            let app = self.obj();
            app.open_database();
            let _ = self.media_activity.set(MediaActivity::new());
            app.load_css();
        }

        fn handle_local_options(
            &self,
            options: &glib::VariantDict,
        ) -> std::ops::ControlFlow<glib::ExitCode> {
            self.should_start_hidden.set(options.contains("hidden"));
            self.parent_handle_local_options(options)
        }

        fn activate(&self) {
            let app = self.obj();
            let window = match app.active_window().and_downcast::<MainWindow>() {
                Some(window) => window,
                None => app.new_window(),
            };
            if self.should_start_hidden.replace(false) {
                // Only the launch activation stays hidden; later ones raise it.
                return;
            }
            window.present();
        }

        fn open(&self, files: &[gio::File], _hint: &str) {
            let app = self.obj();
            for file in files {
                let uri = file.uri().to_string();
                if uri.to_lowercase().starts_with(MAILTO_SCHEME) {
                    app.open_mailto(&uri);
                } else {
                    log::warn!("ignoring unsupported URI {uri}");
                }
            }
        }
    }
    impl GtkApplicationImpl for RustleApplication {}
    impl AdwApplicationImpl for RustleApplication {}
}

glib::wrapper! {
    pub struct RustleApplication(ObjectSubclass<imp::RustleApplication>)
        @extends adw::Application, gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl Default for RustleApplication {
    fn default() -> Self {
        Self::new()
    }
}

impl RustleApplication {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", APP_ID)
            // HANDLES_OPEN so the desktop can hand us mailto: links.
            .property("flags", gio::ApplicationFlags::HANDLES_OPEN)
            .property("resource-base-path", RESOURCE_PATH)
            .build()
    }

    pub fn db(&self) -> Rc<RefCell<Database>> {
        self.imp().db.get().expect("opened at startup").clone()
    }

    pub fn settings(&self) -> gio::Settings {
        self.imp()
            .settings
            .get()
            .expect("loaded at startup")
            .clone()
    }

    pub fn media_is_playing(&self) -> bool {
        self.imp()
            .media_activity
            .get()
            .is_some_and(MediaActivity::is_playing)
    }

    fn open_database(&self) {
        let data_dir = glib::user_data_dir().join("rustle");
        if let Err(error) = std::fs::create_dir_all(&data_dir) {
            log::error!("could not create {}: {error}", data_dir.display());
        }
        let path = data_dir.join("rustle.db");
        let db = Database::open(&path)
            .unwrap_or_else(|error| panic!("could not open {}: {error}", path.display()));
        let _ = self.imp().db.set(Rc::new(RefCell::new(db)));
        let _ = self.imp().settings.set(settings::load());
    }

    fn new_window(&self) -> MainWindow {
        MainWindow::new(self, self.db(), &self.settings())
    }

    fn main_window(&self) -> Option<MainWindow> {
        self.windows()
            .into_iter()
            .find_map(|window| window.downcast::<MainWindow>().ok())
    }

    /// A mailto: link opens the composer and nothing else; the main window
    /// is only presented when there is no account to compose from.
    fn open_mailto(&self, uri: &str) {
        if let Some(window) = self.main_window() {
            window.open_mailto(uri);
            return;
        }
        let accounts = self.db().borrow().accounts().unwrap_or_default();
        let Some(account) = accounts.first() else {
            log::warn!("no account to compose {uri} from, opening the window");
            self.activate();
            return;
        };
        ComposerWindow::for_mailto(Some(self.upcast_ref()), self.db(), account, uri).present();
    }

    fn load_css(&self) {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let provider = gtk::CssProvider::new();
        provider.load_from_resource(&format!("{RESOURCE_PATH}/style.css"));
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        crate::accent::install_fallback(&display);
    }

    fn setup_actions(&self) {
        let about = gio::ActionEntry::builder("about")
            .activate(|app: &Self, _, _| app.show_about())
            .build();
        let preferences = gio::ActionEntry::builder("preferences")
            .activate(|app: &Self, _, _| {
                PreferencesDialog::new(&app.settings()).present(app.active_window().as_ref());
            })
            .build();
        let new_window = gio::ActionEntry::builder("new-window")
            .activate(|app: &Self, _, _| app.new_window().present())
            .build();
        let shortcuts = gio::ActionEntry::builder("shortcuts")
            .activate(|app: &Self, _, _| app.show_shortcuts())
            .build();
        let quit = gio::ActionEntry::builder("quit")
            .activate(|app: &Self, _, _| app.quit())
            .build();
        let focus_mail = gio::ActionEntry::builder("focus-mail")
            .activate(|app: &Self, _, _| app.activate())
            .build();
        let open_mail = gio::ActionEntry::builder("open-mail")
            .parameter_type(Some(&glib::VariantType::new("(xs)").expect("valid type")))
            .activate(|app: &Self, _, parameter| {
                app.activate();
                let Some((folder_id, uid)) = parameter.and_then(|p| p.get::<(i64, String)>())
                else {
                    return;
                };
                if let Some(window) = app.active_window().and_downcast::<MainWindow>() {
                    window.open_email(folder_id, &uid);
                }
            })
            .build();
        self.add_action_entries([
            about,
            preferences,
            new_window,
            shortcuts,
            quit,
            focus_mail,
            open_mail,
        ]);

        for (name, accels) in [
            ("app.preferences", &["<control>comma"][..]),
            ("app.new-window", &["<control><shift>n"]),
            ("app.shortcuts", &["<control>question"]),
            ("app.quit", &["<control>q"]),
            // Flag actions are Ctrl-modified so they don't fire while typing in search.
            ("win.toggle-read", &["<control>i"]),
            ("win.toggle-star", &["<control>s"]),
            ("win.toggle-pin", &["<control>p"]),
            ("win.archive", &["<control>e"]),
            ("win.trash", &["<control>Delete"]),
            ("win.compose", &["<control>n"]),
            ("win.reply", &["<control>r"]),
            ("win.reply-all", &["<control><shift>r"]),
            ("win.forward", &["<control><shift>f"]),
            ("win.refresh", &["F5"]),
            ("win.search", &["<control>f"]),
        ] {
            self.set_accels_for_action(name, accels);
        }
    }

    fn show_about(&self) {
        let about =
            adw::AboutDialog::from_appdata(&format!("{RESOURCE_PATH}/metainfo.xml"), Some(VERSION));
        about.set_translator_credits(&gettext("translator-credits"));
        about.set_developers(&["Brandon Williams"]);
        about.set_copyright("© 2026 Brandon Williams");
        about.present(self.active_window().as_ref());
    }

    fn show_shortcuts(&self) {
        let builder =
            gtk::Builder::from_resource(&format!("{RESOURCE_PATH}/ui/shortcuts-dialog.ui"));
        if let Some(dialog) = builder.object::<adw::ShortcutsDialog>("shortcuts_dialog") {
            dialog.present(self.active_window().as_ref());
        }
    }
}

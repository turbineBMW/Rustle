//! Preferences: bound straight to GSettings, except autostart, which the
//! desktop portal decides.

use crate::i18n::gettext;
use crate::settings as keys;
use crate::sound;
use crate::widgets::sound_row;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use std::cell::{Cell, RefCell};

/// The sync-interval combo, in row order: the minute value stored in
/// GSettings and the label shown for it. 0 = manual only.
fn sync_intervals() -> [(i32, String); 5] {
    [
        (0, gettext("Manually")),
        (5, gettext("Every 5 minutes")),
        (15, gettext("Every 15 minutes")),
        (30, gettext("Every 30 minutes")),
        (60, gettext("Every hour")),
    ]
}

const DEFAULT_SYNC_INTERVAL_MINUTES: i32 = 15;

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const BACKGROUND_INTERFACE: &str = "org.freedesktop.portal.Background";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/preferences-dialog.ui")]
    pub struct PreferencesDialog {
        #[template_child]
        pub notifications_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub sound_row: TemplateChild<adw::ComboRow>,
        #[template_child]
        pub images_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub avatars_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub background_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub autostart_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub interval_row: TemplateChild<adw::ComboRow>,
        pub settings: RefCell<Option<gio::Settings>>,
        pub autostart_subscription: Cell<Option<gio::SignalSubscriptionId>>,
        pub is_settling: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PreferencesDialog {
        const NAME: &'static str = "RustlePreferencesDialog";
        type Type = super::PreferencesDialog;
        type ParentType = adw::PreferencesDialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PreferencesDialog {}
    impl WidgetImpl for PreferencesDialog {}
    impl AdwDialogImpl for PreferencesDialog {}
    impl PreferencesDialogImpl for PreferencesDialog {}
}

glib::wrapper! {
    pub struct PreferencesDialog(ObjectSubclass<imp::PreferencesDialog>)
        @extends adw::PreferencesDialog, adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PreferencesDialog {
    pub fn new(settings: &gio::Settings) -> Self {
        let dialog: Self = glib::Object::new();
        let imp = dialog.imp();
        imp.settings.replace(Some(settings.clone()));

        settings
            .bind(keys::NOTIFICATIONS, &*imp.notifications_row, "active")
            .build();
        sound_row::setup(
            &imp.sound_row,
            sound::default_sound(settings),
            None,
            glib::clone!(
                #[weak]
                dialog,
                move |choice| {
                    if let Some(settings) = dialog.imp().settings.borrow().as_ref() {
                        let _ = settings.set_string(keys::NOTIFICATION_SOUND, &choice.as_setting());
                    }
                }
            ),
        );
        settings
            .bind(keys::LOAD_REMOTE_IMAGES, &*imp.images_row, "active")
            .build();
        settings
            .bind(keys::LOAD_SENDER_AVATARS, &*imp.avatars_row, "active")
            .build();
        settings
            .bind(keys::RUN_IN_BACKGROUND, &*imp.background_row, "active")
            .build();

        let intervals = sync_intervals();
        let labels: Vec<&str> = intervals.iter().map(|(_, label)| label.as_str()).collect();
        imp.interval_row
            .set_model(Some(&gtk::StringList::new(&labels)));
        imp.interval_row
            .set_selected(Self::interval_index(settings.int(keys::SYNC_INTERVAL)));
        imp.interval_row.connect_selected_notify(glib::clone!(
            #[weak]
            dialog,
            move |row| {
                let (minutes, _) = &sync_intervals()[row.selected() as usize];
                if let Some(settings) = dialog.imp().settings.borrow().as_ref() {
                    let _ = settings.set_int(keys::SYNC_INTERVAL, *minutes);
                }
            }
        ));

        // Not bound to GSettings: the portal decides, and the key only
        // records its answer, so the row follows the reply, not the click.
        imp.autostart_row
            .set_active(settings.boolean(keys::START_AT_LOGIN));
        imp.autostart_row.connect_active_notify(glib::clone!(
            #[weak]
            dialog,
            move |row| dialog.on_autostart_toggled(row.is_active())
        ));
        dialog
    }

    /// The combo row for a stored interval, falling back to the default.
    fn interval_index(minutes: i32) -> u32 {
        let intervals = sync_intervals();
        let wanted = if intervals.iter().any(|(value, _)| *value == minutes) {
            minutes
        } else {
            DEFAULT_SYNC_INTERVAL_MINUTES
        };
        intervals
            .iter()
            .position(|(value, _)| *value == wanted)
            .unwrap_or(0) as u32
    }

    fn settings(&self) -> gio::Settings {
        self.imp()
            .settings
            .borrow()
            .clone()
            .expect("set at construction")
    }

    fn on_autostart_toggled(&self, is_wanted: bool) {
        let imp = self.imp();
        // Also the exit for the row being put back after a refused request.
        if imp.is_settling.get() || is_wanted == self.settings().boolean(keys::START_AT_LOGIN) {
            return;
        }
        // A second request before the first answer would leave the row
        // showing the losing one, so the row waits until the portal replied.
        imp.autostart_row.set_sensitive(false);
        let this = self.downgrade();
        glib::spawn_future_local(async move {
            let Some(dialog) = this.upgrade() else { return };
            if let Err(error) = dialog.request_autostart(is_wanted).await {
                log::error!("could not reach the background portal: {error}");
                let current = dialog.settings().boolean(keys::START_AT_LOGIN);
                dialog.settle_autostart(
                    current,
                    Some(gettext("Could not reach the desktop portal.")),
                );
            }
        });
    }

    async fn request_autostart(&self, is_wanted: bool) -> Result<(), glib::Error> {
        #[allow(deprecated)]
        let bus = gio::bus_get_future(gio::BusType::Session).await?;
        // The reply comes back as a signal on a path derived from the token,
        // so subscribe before asking -- the portal may answer immediately.
        let token = format!("rustle_{}", glib::uuid_string_random().replace('-', "_"));
        let sender = bus
            .unique_name()
            .map(|name| name.trim_start_matches(':').replace('.', "_"))
            .unwrap_or_default();
        let request_path = format!("{PORTAL_PATH}/request/{sender}/{token}");
        let this = self.downgrade();
        #[allow(deprecated)]
        let subscription = bus.signal_subscribe(
            Some(PORTAL_NAME),
            Some(REQUEST_INTERFACE),
            Some("Response"),
            Some(&request_path),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, parameters| {
                if let Some(dialog) = this.upgrade() {
                    dialog.on_autostart_response(parameters);
                }
            },
        );
        self.imp().autostart_subscription.set(Some(subscription));

        let options = glib::VariantDict::new(None);
        options.insert("handle_token", token);
        options.insert(
            "reason",
            gettext("Rustle checks for new mail after you log in."),
        );
        options.insert("autostart", is_wanted);
        // Becomes the Exec line of the autostart entry the portal writes.
        options.insert(
            "commandline",
            vec!["rustle".to_string(), "--hidden".to_string()],
        );
        let parameters = glib::Variant::tuple_from_iter(["".to_variant(), options.end()]);
        let result = bus
            .call_future(
                Some(PORTAL_NAME),
                PORTAL_PATH,
                BACKGROUND_INTERFACE,
                "RequestBackground",
                Some(&parameters),
                None,
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await;
        if let Err(error) = result {
            log::error!("background portal refused the autostart request: {error}");
            let current = self.settings().boolean(keys::START_AT_LOGIN);
            self.settle_autostart(
                current,
                Some(gettext("Could not change whether Rustle starts at login.")),
            );
        }
        Ok(())
    }

    fn on_autostart_response(&self, parameters: &glib::Variant) {
        let response = parameters.child_value(0).get::<u32>().unwrap_or(1);
        let results = parameters.child_value(1);
        let is_wanted = self.imp().autostart_row.is_active();
        // A non-zero response is a cancel or a failure: nothing was changed.
        let is_enabled = if response == 0 {
            glib::VariantDict::new(Some(&results))
                .lookup::<bool>("autostart")
                .ok()
                .flatten()
                .unwrap_or(false)
        } else {
            self.settings().boolean(keys::START_AT_LOGIN)
        };
        let message = if is_enabled != is_wanted {
            log::warn!(
                "background portal did not set autostart to {is_wanted} (response {response})"
            );
            Some(gettext("Could not change whether Rustle starts at login."))
        } else {
            None
        };
        self.settle_autostart(is_enabled, message);
    }

    /// Record what the portal actually did and match the row to it.
    fn settle_autostart(&self, is_enabled: bool, message: Option<String>) {
        let imp = self.imp();
        if let Some(subscription) = imp.autostart_subscription.take() {
            if let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
                #[allow(deprecated)]
                bus.signal_unsubscribe(subscription);
            }
        }
        // Before the row, so the notify handler sees them agree and stops.
        let _ = self
            .settings()
            .set_boolean(keys::START_AT_LOGIN, is_enabled);
        imp.is_settling.set(true);
        imp.autostart_row.set_active(is_enabled);
        imp.is_settling.set(false);
        imp.autostart_row.set_sensitive(true);
        if let Some(message) = message {
            self.add_toast(adw::Toast::new(&message));
        }
    }
}

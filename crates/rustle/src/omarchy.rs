//! Follow the Omarchy desktop theme, live.
//!
//! The palette itself is resolved in `rustle_core::omarchy`; this is the half
//! that touches GTK. The theme's CSS goes into a provider stacked above the
//! app stylesheet and the accent fallback, libadwaita is forced light or dark
//! to match the theme's mode, and the accent is handed to `accent` for the
//! WebKit views. Turning the setting off empties the provider and hands the
//! colour scheme back to the system, so nothing here is sticky.

use crate::{accent, settings as keys};
use adw::prelude::*;
use gtk::{gio, glib};
use rustle_core::omarchy;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// One theme switch is a burst of file events; reload once it settles.
const SETTLE: Duration = Duration::from_millis(120);

/// Owns the file monitor, so it has to outlive the windows: the application
/// keeps it.
pub struct OmarchyTheme {
    _inner: Rc<Inner>,
}

struct Inner {
    settings: gio::Settings,
    provider: gtk::CssProvider,
    monitor: RefCell<Option<gio::FileMonitor>>,
}

pub fn detected() -> bool {
    omarchy::detected(&glib::home_dir())
}

pub fn theme_name() -> Option<String> {
    omarchy::theme_name(&glib::home_dir())
}

impl OmarchyTheme {
    /// Call once, after `accent::install_fallback`: the provider has to
    /// outrank the fallback's `--accent-*` variables.
    pub fn install(display: &gtk::gdk::Display, settings: &gio::Settings) -> Self {
        let inner = Rc::new(Inner {
            settings: settings.clone(),
            provider: gtk::CssProvider::new(),
            monitor: RefCell::new(None),
        });
        gtk::style_context_add_provider_for_display(
            display,
            &inner.provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
        );
        inner.provider.connect_parsing_error(|_, section, error| {
            log::warn!(
                "omarchy theme css line {}: {error}",
                section.start_location().lines() + 1
            );
        });
        settings.connect_changed(
            Some(keys::FOLLOW_OMARCHY_THEME),
            glib::clone!(
                #[weak]
                inner,
                move |_, _| inner.reload()
            ),
        );
        inner.watch();
        inner.reload();
        Self { _inner: inner }
    }
}

impl Inner {
    /// Re-read the active theme into the provider. A theme that is missing,
    /// unreadable, or not being followed leaves the provider empty.
    fn reload(&self) {
        let theme = self
            .settings
            .boolean(keys::FOLLOW_OMARCHY_THEME)
            .then(|| omarchy::load(&glib::home_dir()))
            .flatten();
        self.provider
            .load_from_string(theme.as_ref().map_or("", |theme| theme.css.as_str()));

        // The reader and composer re-render on a dark/light flip already, so
        // only an accent change within the same scheme needs announcing.
        let manager = adw::StyleManager::default();
        let was_dark = manager.is_dark();
        let accent_changed = accent::set_override(theme.as_ref().and_then(|t| t.accent.clone()));
        let scheme_changed =
            accent::set_reader_scheme(theme.as_ref().and_then(|t| t.reader.clone()));
        let accent_changed = accent_changed || scheme_changed;
        manager.set_color_scheme(match &theme {
            Some(theme) if theme.light => adw::ColorScheme::ForceLight,
            Some(_) => adw::ColorScheme::ForceDark,
            None => adw::ColorScheme::Default,
        });
        if accent_changed && manager.is_dark() == was_dark {
            accent::notify();
        }
    }

    /// Recolour live when the desktop theme changes. `omarchy theme set`
    /// renames a staging directory over `current/theme` and rewrites
    /// `current/theme.name`, so the watch is on the stable parent -- a monitor
    /// on a file inside the theme would not survive the directory swap.
    fn watch(self: &Rc<Self>) {
        if !detected() {
            return;
        }
        let dir = omarchy::state_dir(&glib::home_dir());
        let monitor = match gio::File::for_path(&dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(monitor) => monitor,
            Err(error) => {
                log::warn!("cannot watch {}: {error}", dir.display());
                return;
            }
        };
        let pending = Rc::new(Cell::new(false));
        monitor.connect_changed(glib::clone!(
            #[weak(rename_to = inner)]
            self,
            move |_, _, _, _| {
                if pending.replace(true) {
                    return;
                }
                let pending = pending.clone();
                glib::timeout_add_local_once(
                    SETTLE,
                    glib::clone!(
                        #[weak]
                        inner,
                        move || {
                            pending.set(false);
                            inner.reload();
                        }
                    ),
                );
            }
        ));
        self.monitor.replace(Some(monitor));
    }
}

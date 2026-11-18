//! Follow the Omarchy desktop theme, live.
//!
//! The palette itself is resolved in `rustle_core::omarchy`; this is the half
//! that touches GTK. The theme's CSS goes into a provider stacked above the
//! app stylesheet and the accent fallback, libadwaita is forced light or dark
//! to match the theme's mode, and the accent is handed to `accent` for the
//! WebKit views. Turning the setting off empties the provider and hands the
//! colour scheme back to the system, so nothing here is sticky.
//!
//! The see-through sidebar is Rustle's own setting, not the theme's:
//! *Preferences → Appearance → Transparent Sidebar* and *Sidebar Opacity*
//! (`transparent-sidebar`, `sidebar-opacity`). The opacity goes into a
//! provider above the theme's, so a theme's rustle.css can't change it. A
//! see-through sidebar also needs the main window's own background out of
//! the way, which CSS alone can't decide: [`sync_transparency`] gives the
//! window the `rustle-transparent` class that clears it (style.css).
//!
//! Themes used to set `--rustle-sidebar-opacity` themselves. The first launch
//! with the setting carries a followed theme's value over, once
//! ([`adopt_theme_opacity`]), so nothing changes on screen.

use crate::window::MainWindow;
use crate::{accent, settings as keys};
use adw::prelude::*;
use gtk::{gio, glib};
use rustle_core::omarchy;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// One theme switch is a burst of file events; reload once it settles.
const SETTLE: Duration = Duration::from_millis(120);

thread_local! {
    /// The sidebar opacity in force: 1, style.css's default, while the
    /// setting is off.
    static SIDEBAR_OPACITY: Cell<f32> = const { Cell::new(1.0) };
}

/// Clear `window`'s background while the folder sidebar is see-through, and
/// put it back once it isn't. For main windows only: the class also repaints
/// the parts of the main window that stood on that background.
pub fn sync_transparency(window: &impl IsA<gtk::Widget>) {
    if SIDEBAR_OPACITY.get() < 1.0 {
        window.add_css_class("rustle-transparent");
    } else {
        window.remove_css_class("rustle-transparent");
    }
}

/// Owns the file monitor, so it has to outlive the windows: the application
/// keeps it.
pub struct OmarchyTheme {
    _inner: Rc<Inner>,
}

struct Inner {
    settings: gio::Settings,
    provider: gtk::CssProvider,
    /// `--rustle-sidebar-opacity`, above `provider`.
    sidebar: gtk::CssProvider,
    monitor: RefCell<Option<gio::FileMonitor>>,
}

/// The opacity the settings ask for: 1 when the sidebar isn't transparent.
fn sidebar_opacity(transparent: bool, opacity: f64) -> f32 {
    if transparent {
        opacity.clamp(0.0, 1.0) as f32
    } else {
        1.0
    }
}

/// Carry a followed theme's old `--rustle-sidebar-opacity` into the setting,
/// the first time only.
fn adopt_theme_opacity(settings: &gio::Settings) {
    if settings.boolean(keys::THEME_SIDEBAR_OPACITY_ADOPTED) {
        return;
    }
    let theme = settings
        .boolean(keys::FOLLOW_OMARCHY_THEME)
        .then(|| omarchy::load(&glib::home_dir()))
        .flatten();
    if let Some(opacity) = theme.and_then(|t| t.sidebar_opacity).filter(|o| *o < 1.0) {
        let _ = settings.set_boolean(keys::TRANSPARENT_SIDEBAR, true);
        // Two places, as the slider shows it: 0.8f32 is 0.800000011920929 widened.
        let _ = settings.set_double(
            keys::SIDEBAR_OPACITY,
            (f64::from(opacity) * 100.0).round() / 100.0,
        );
    }
    let _ = settings.set_boolean(keys::THEME_SIDEBAR_OPACITY_ADOPTED, true);
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
        adopt_theme_opacity(settings);
        let inner = Rc::new(Inner {
            settings: settings.clone(),
            provider: gtk::CssProvider::new(),
            sidebar: gtk::CssProvider::new(),
            monitor: RefCell::new(None),
        });
        gtk::style_context_add_provider_for_display(
            display,
            &inner.sidebar,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 3,
        );
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
        for key in [keys::TRANSPARENT_SIDEBAR, keys::SIDEBAR_OPACITY] {
            settings.connect_changed(
                Some(key),
                glib::clone!(
                    #[weak]
                    inner,
                    move |_, _| inner.apply_sidebar()
                ),
            );
        }
        inner.watch();
        inner.reload();
        inner.apply_sidebar();
        Self { _inner: inner }
    }
}

impl Inner {
    /// State the sidebar opacity the settings ask for, and clear or restore
    /// each main window's background to match.
    fn apply_sidebar(&self) {
        let opacity = sidebar_opacity(
            self.settings.boolean(keys::TRANSPARENT_SIDEBAR),
            self.settings.double(keys::SIDEBAR_OPACITY),
        );
        SIDEBAR_OPACITY.set(opacity);
        self.sidebar.load_from_string(&format!(
            ":root {{ {}: {opacity:.2}; }}",
            omarchy::SIDEBAR_OPACITY
        ));
        for window in gtk::Window::list_toplevels() {
            if window.is::<MainWindow>() {
                sync_transparency(&window);
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::sidebar_opacity;

    #[test]
    fn the_sidebar_is_only_see_through_while_the_setting_is_on() {
        assert_eq!(sidebar_opacity(true, 0.8), 0.8);
        assert_eq!(sidebar_opacity(false, 0.3), 1.0);
        assert_eq!(sidebar_opacity(true, 1.5), 1.0);
    }
}

//! Gmail-style single-key shortcuts, for those who turn them on: j and k
//! walk the list, e archives, # deletes, r/a/f reply, reply all and forward,
//! s stars, c composes, / searches, Shift+I and Shift+U mark read and
//! unread. They stand down whenever a key would be typing: in the search
//! box, a dialog's entry, or the composer.

use super::MainWindow;
use crate::settings as keys;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, glib};

impl MainWindow {
    pub(super) fn setup_single_keys(&self) {
        let controller = gtk::EventControllerKey::new();
        controller.connect_key_pressed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| window.on_single_key(key, modifiers)
        ));
        self.add_controller(controller);
    }

    fn on_single_key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> glib::Propagation {
        let held = gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK;
        if !self.settings().boolean(keys::SINGLE_KEY_SHORTCUTS)
            || modifiers.intersects(held)
            || self.is_typing()
        {
            return glib::Propagation::Proceed;
        }
        let Some(name) = key.to_unicode() else {
            return glib::Propagation::Proceed;
        };
        let action = match name {
            'j' => return self.step_selection(1),
            'k' => return self.step_selection(-1),
            'e' => "win.archive",
            '#' => "win.trash",
            'r' => "win.reply",
            'a' => "win.reply-all",
            'f' => "win.forward",
            's' => "win.toggle-star",
            'c' => "win.compose",
            '/' => "win.search",
            'I' => return self.mark_selection(false),
            'U' => return self.mark_selection(true),
            _ => return glib::Propagation::Proceed,
        };
        let _ = WidgetExt::activate_action(self, action, None);
        glib::Propagation::Stop
    }

    /// Whether the key would land in something being typed in.
    fn is_typing(&self) -> bool {
        let Some(focus) = GtkWindowExt::focus(self) else {
            return false;
        };
        if focus.is::<gtk::Text>() || focus.is::<gtk::TextView>() || focus.is::<gtk::Editable>() {
            return true;
        }
        let composer = self
            .state()
            .inline_composer
            .as_ref()
            .map(|inline| inline.widget());
        composer.is_some_and(|composer| focus.is_ancestor(&composer))
    }

    /// Select the next (1) or previous (-1) message, and show it.
    fn step_selection(&self, step: i32) -> glib::Propagation {
        let selection = self.selection();
        let count = selection.n_items();
        if count == 0 {
            return glib::Propagation::Stop;
        }
        let current = (0..count).find(|&index| selection.is_selected(index));
        let target = match current {
            Some(index) => (index as i64 + i64::from(step)).clamp(0, count as i64 - 1) as u32,
            None => 0,
        };
        selection.select_item(target, true);
        self.imp()
            .email_list
            .scroll_to(target, gtk::ListScrollFlags::FOCUS, None);
        glib::Propagation::Stop
    }

    /// Shift+I and Shift+U: set read or unread, rather than toggle.
    fn mark_selection(&self, unread: bool) -> glib::Propagation {
        self.set_unread(&self.selected_emails(), unread);
        glib::Propagation::Stop
    }
}

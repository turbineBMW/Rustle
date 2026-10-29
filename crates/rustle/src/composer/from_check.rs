//! Asking which account a new message goes out from, before it goes. A
//! reply already comes from the account its original arrived in; a fresh
//! message takes whatever the composer opened with, which is easy to miss.

use super::Composer;
use crate::editor;
use crate::i18n::{self, gettext};
use crate::settings as keys;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, glib};
use std::cell::Cell;
use std::rc::Rc;

impl Composer {
    /// Send, once the sending account is confirmed if it needs to be. The
    /// Send button and Send Later both come through here.
    pub(super) fn on_send_requested(&self) {
        if !self.should_confirm_from() {
            self.on_send_clicked();
            return;
        }
        self.ask_from();
    }

    /// Only a new message, with more than one account to choose between,
    /// and only while the setting is on.
    fn should_confirm_from(&self) -> bool {
        let imp = self.imp();
        let is_on = imp
            .settings
            .borrow()
            .as_ref()
            .is_none_or(|settings| settings.boolean(keys::CONFIRM_SEND_ACCOUNT));
        is_on && imp.in_reply_to.borrow().is_empty() && imp.accounts.borrow().len() > 1
    }

    /// One row per account, the composer's own pick focused, so Enter
    /// confirms it, the arrows or a digit pick another, and Escape goes back
    /// to the message.
    fn ask_from(&self) {
        let imp = self.imp();
        let current = imp.from_dropdown.selected() as usize;
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Browse)
            .css_classes(["boxed-list"])
            .build();
        let mut current_row = None;
        for (index, account) in imp.accounts.borrow().iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(account.short_label())
                .use_markup(false)
                .activatable(true)
                .build();
            if account.short_label() != account.email {
                row.set_subtitle(&account.email);
            }
            let dot = gtk::Box::builder()
                .width_request(10)
                .height_request(10)
                .valign(gtk::Align::Center)
                .css_classes(["account-dot"])
                .build();
            crate::account_colors::tag(&dot, Some(account.id));
            row.add_prefix(&dot);
            if index == current {
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                check.set_tooltip_text(Some(&gettext("Picked in the Message")));
                row.add_suffix(&check);
                current_row = Some(row.clone());
            }
            if index < 9 {
                row.add_suffix(&adw::ShortcutLabel::new(&(index + 1).to_string()));
            }
            list.append(&row);
        }

        let recipients = self.recipients_display();
        let subject = imp.subject_row.text();
        let summary = gtk::Label::builder()
            .label(i18n::format(
                &gettext("“{subject}” to {recipients}"),
                &[("subject", subject.trim()), ("recipients", &recipients)],
            ))
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        let hint = gtk::Label::builder()
            .label(gettext(
                "Enter sends from the highlighted account, a number from that one; Escape goes back",
            ))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        content.append(&summary);
        content.append(&list);
        content.append(&hint);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&content));
        let dialog = adw::Dialog::builder()
            .title(gettext("Send From"))
            .content_width(420)
            .child(&view)
            .build();

        // Everything leaves through `closed`: a pick closes the dialog with
        // `chosen` set, Escape or the close button with it empty.
        let chosen = Rc::new(Cell::new(None::<usize>));
        let pick = {
            let (chosen, dialog) = (chosen.clone(), dialog.downgrade());
            move |index: usize| {
                chosen.set(Some(index));
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
            }
        };
        list.connect_row_activated({
            let pick = pick.clone();
            move |_, row| pick(row.index() as usize)
        });
        let count = imp.accounts.borrow().len();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, keyval, _, modifiers| {
            // Not Shift: some layouts need it for the digits.
            let held = gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SUPER_MASK;
            if modifiers.intersects(held) {
                return glib::Propagation::Proceed;
            }
            match digit(keyval) {
                Some(n) if (1..=count).contains(&n) => {
                    pick(n - 1);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        dialog.add_controller(keys);
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| match chosen.get() {
                Some(index) => window.send_from(index),
                // Send Later's time is for this send only.
                None => window.imp().scheduled_for.set(None),
            }
        ));

        dialog.present(Some(self));
        if let Some(row) = current_row {
            list.select_row(Some(&row));
            dialog.set_focus(Some(&row));
        }
    }

    /// Send from the account at `index`. Switching swaps the signature in
    /// the editor, which reports the new body back on its own; a no-op
    /// script queued behind the swap answers only once that report is in.
    fn send_from(&self, index: usize) {
        let dropdown = &self.imp().from_dropdown;
        if dropdown.selected() as usize == index {
            self.on_send_clicked();
            return;
        }
        dropdown.set_selected(index as u32);
        let Some(webview) = self.imp().webview.borrow().clone() else {
            self.on_send_clicked();
            return;
        };
        editor::after_pending(
            &webview,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || window.on_send_clicked()
            ),
        );
    }
}

/// The number on a digit key, from the main row or the keypad.
fn digit(keyval: gdk::Key) -> Option<usize> {
    let c = keyval.to_unicode()?;
    c.to_digit(10).map(|n| n as usize)
}

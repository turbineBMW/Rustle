//! The composer's extra ways to write and send: Send Later, plain text,
//! pasting without formatting, templates, and offering a reply's original
//! attachments to someone newly added.

use super::Composer;
use crate::editor;
use crate::i18n::{self, gettext};
use adw::prelude::*;
use adw::subclass::prelude::*;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Timelike, Weekday};
use gtk::{gio, glib};
use rustle_core::models::Account;
use rustle_core::{address, html};
use std::rc::Rc;
use webkit::prelude::*;

/// A message the composer put in the Outbox to go later: after the Undo
/// window, or at the time Send Later picked.
#[derive(Clone, Debug)]
pub struct QueuedSend {
    pub account: Account,
    pub email_id: i64,
    pub send_at: DateTime<Local>,
    /// Picked with Send Later, rather than held back for Undo.
    pub is_scheduled: bool,
}

pub type QueueHandler = Rc<dyn Fn(QueuedSend)>;

impl Composer {
    /// Let Send hand the message to `handler` to send later, rather than
    /// sending it at once. Hosts that can show an Undo toast set this.
    pub fn set_queue_handler(&self, handler: impl Fn(QueuedSend) + 'static) {
        self.imp().queue_handler.replace(Some(Rc::new(handler)));
        self.update_send_later();
    }

    pub(super) fn setup_tools(&self) {
        let group = gio::SimpleActionGroup::new();
        let window = self.downgrade();
        let simple = |name: &str, run: fn(&Composer)| {
            let action = gio::SimpleAction::new(name, None);
            let window = window.clone();
            action.connect_activate(move |_, _| {
                if let Some(window) = window.upgrade() {
                    run(&window);
                }
            });
            action
        };
        group.add_action(&simple("send-later", |w| w.ask_send_time()));
        group.add_action(&simple("save-template", |w| w.ask_template_name()));
        group.add_action(&simple("paste-plain", |w| w.paste_plain()));

        let plain = gio::SimpleAction::new_stateful("plain-text", None, &false.to_variant());
        plain.connect_change_state(glib::clone!(
            #[strong]
            window,
            move |action, state| {
                let Some(is_plain) = state.and_then(|s| s.get::<bool>()) else {
                    return;
                };
                action.set_state(&is_plain.to_variant());
                if let Some(window) = window.upgrade() {
                    window.set_plain_text(is_plain);
                }
            }
        ));
        group.add_action(&plain);

        for (name, delete) in [("use-template", false), ("delete-template", true)] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::INT64));
            let window = window.clone();
            action.connect_activate(move |_, parameter| {
                let (Some(window), Some(id)) =
                    (window.upgrade(), parameter.and_then(|p| p.get::<i64>()))
                else {
                    return;
                };
                if delete {
                    window.delete_template(id);
                } else {
                    window.use_template(id);
                }
            });
            group.add_action(&action);
        }
        self.insert_action_group("composer", Some(&group));
        self.imp().tool_actions.replace(Some(group));
        self.update_send_later();
        self.rebuild_more_menu();

        // Ahead of the editor, which would paste with formatting.
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control><Shift>v"),
            Some(gtk::NamedAction::new("composer.paste-plain")),
        ));
        self.add_controller(shortcuts);

        let imp = self.imp();
        imp.originals_banner.connect_button_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |banner| {
                banner.set_revealed(false);
                let originals = std::mem::take(&mut *window.imp().originals.borrow_mut());
                for attachment in originals {
                    window.add_attachment(attachment);
                }
            }
        ));
        for row in [&imp.to_row, &imp.cc_row, &imp.bcc_row] {
            row.connect_changed(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_| window.offer_originals()
            ));
        }
    }

    /// Send Later needs a host that can hold the message.
    fn update_send_later(&self) {
        let imp = self.imp();
        let can_hold = imp.queue_handler.borrow().is_some();
        let action = imp
            .tool_actions
            .borrow()
            .as_ref()
            .and_then(|group| group.lookup_action("send-later"))
            .and_downcast::<gio::SimpleAction>();
        if let Some(action) = action {
            action.set_enabled(can_hold);
        }
    }

    /// The More menu: plain text, pasting, and the templates there are now.
    fn rebuild_more_menu(&self) {
        let menu = gio::Menu::new();
        let writing = gio::Menu::new();
        writing.append(Some(&gettext("Plain Text")), Some("composer.plain-text"));
        writing.append(
            Some(&gettext("Paste Without Formatting")),
            Some("composer.paste-plain"),
        );
        menu.append_section(None, &writing);

        let templates = gio::Menu::new();
        let saved = self.db().borrow().templates().unwrap_or_default();
        if !saved.is_empty() {
            let use_menu = gio::Menu::new();
            let delete_menu = gio::Menu::new();
            for template in &saved {
                for (submenu, action) in [
                    (&use_menu, "composer.use-template"),
                    (&delete_menu, "composer.delete-template"),
                ] {
                    let item = gio::MenuItem::new(Some(&template.name), None);
                    item.set_action_and_target_value(Some(action), Some(&template.id.to_variant()));
                    submenu.append_item(&item);
                }
            }
            templates.append_submenu(Some(&gettext("Insert Template")), &use_menu);
            templates.append_submenu(Some(&gettext("Delete Template")), &delete_menu);
        }
        templates.append(
            Some(&gettext("Save as Template…")),
            Some("composer.save-template"),
        );
        menu.append_section(None, &templates);
        self.imp().more_button.set_menu_model(Some(&menu));
    }

    fn webview(&self) -> Option<webkit::WebView> {
        self.imp().webview.borrow().clone()
    }

    fn paste_plain(&self) {
        if let Some(webview) = self.webview() {
            webview.execute_editing_command("PasteAsPlainText");
        }
    }

    /// Plain text drops the formatting there is, hides the bar that adds
    /// more, and sends text/plain alone. Back off, the bar returns; what was
    /// stripped stays stripped.
    fn set_plain_text(&self, is_plain: bool) {
        let imp = self.imp();
        imp.plain_text.set(is_plain);
        imp.format_bar.set_visible(!is_plain);
        let Some(webview) = self.webview().filter(|_| is_plain) else {
            return;
        };
        let target = webview.clone();
        editor::own_html(&webview, move |own| {
            let text = html::html_to_text(&own);
            editor::replace_own(&target, &html::to_html(text.trim_end()), |_| {});
        });
    }

    // --- templates --------------------------------------------------------

    fn ask_template_name(&self) {
        let entry = gtk::Entry::builder()
            .text(self.imp().subject_row.text().trim())
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::new(
            Some(&gettext("Save as Template")),
            Some(&gettext(
                "The subject and what you wrote are saved, to start other messages from.",
            )),
        );
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", &gettext("Cancel")), ("save", &gettext("Save"))]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        dialog.set_response_enabled("save", !entry.text().trim().is_empty());
        entry.connect_changed(glib::clone!(
            #[weak]
            dialog,
            move |entry| dialog.set_response_enabled("save", !entry.text().trim().is_empty())
        ));
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[weak]
                entry,
                move |_, response| {
                    let name = entry.text().trim().to_string();
                    if response == "save" && !name.is_empty() {
                        window.save_template(name);
                    }
                }
            ),
        );
        dialog.present(Some(self));
    }

    fn save_template(&self, name: String) {
        let Some(webview) = self.webview() else {
            return;
        };
        let subject = self.imp().subject_row.text().trim().to_string();
        editor::own_html(
            &webview,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |body| {
                    match window.db().borrow().save_template(&name, &subject, &body) {
                        Ok(()) => window.toast(&i18n::format(
                            &gettext("Saved the template “{name}”."),
                            &[("name", &name)],
                        )),
                        Err(error) => log::error!("could not save the template {name:?}: {error}"),
                    }
                    window.rebuild_more_menu();
                }
            ),
        );
    }

    /// Insert a template's text where the caret is, and its subject when
    /// there's none yet.
    fn use_template(&self, template_id: i64) {
        let template = self
            .db()
            .borrow()
            .templates()
            .unwrap_or_default()
            .into_iter()
            .find(|template| template.id == template_id);
        let Some(template) = template else {
            return;
        };
        let subject_row = &self.imp().subject_row;
        if subject_row.text().trim().is_empty() && !template.subject.is_empty() {
            subject_row.set_text(&template.subject);
        }
        if let Some(webview) = self.webview() {
            let body = if self.imp().plain_text.get() {
                html::to_html(html::html_to_text(&template.body_html).trim_end())
            } else {
                template.body_html
            };
            editor::exec(&webview, "insertHTML", Some(&body));
        }
    }

    fn delete_template(&self, template_id: i64) {
        if let Err(error) = self.db().borrow().delete_template(template_id) {
            log::error!("could not delete template {template_id}: {error}");
        }
        self.rebuild_more_menu();
    }

    fn toast(&self, text: &str) {
        self.imp().toast_overlay.add_toast(adw::Toast::new(text));
    }

    // --- a reply's original attachments ----------------------------------

    /// Someone the original never went to was just added, and it had
    /// attachments: offer them, once.
    fn offer_originals(&self) {
        let imp = self.imp();
        if imp.originals.borrow().is_empty() || imp.originals_banner.is_revealed() {
            return;
        }
        let people = imp.original_people.borrow();
        let has_newcomer = [&imp.to_row, &imp.cc_row, &imp.bcc_row]
            .iter()
            .flat_map(|row| address::parse_list(&row.text()))
            .any(|mailbox| {
                // Only a finished address counts, not one half typed.
                mailbox.address.contains('@')
                    && mailbox
                        .address
                        .rsplit('@')
                        .next()
                        .is_some_and(|d| d.contains('.'))
                    && !people.contains(&mailbox.address.to_lowercase())
            });
        if !has_newcomer {
            return;
        }
        let count = imp.originals.borrow().len() as u64;
        imp.originals_banner.set_title(&i18n::plural(
            "Someone new is on this reply. Attach the original's attachment?",
            "Someone new is on this reply. Attach the original's {n} attachments?",
            count,
            &[],
        ));
        imp.originals_banner.set_revealed(true);
    }

    // --- Send Later -------------------------------------------------------

    /// Pick a time to send: a preset, or a day and time of one's own.
    fn ask_send_time(&self) {
        let start = default_send_time(Local::now());
        let calendar = gtk::Calendar::new();
        let hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
        let minute = gtk::SpinButton::with_range(0.0, 55.0, 5.0);
        for spin in [&hour, &minute] {
            spin.set_orientation(gtk::Orientation::Vertical);
            spin.connect_output(|spin| {
                spin.set_text(&format!("{:02}", spin.value() as u32));
                glib::Propagation::Stop
            });
        }
        let set = {
            let (calendar, hour, minute) = (calendar.clone(), hour.clone(), minute.clone());
            move |at: DateTime<Local>| {
                if let Ok(day) = glib::DateTime::from_local(
                    at.year(),
                    at.month() as i32,
                    at.day() as i32,
                    0,
                    0,
                    0.0,
                ) {
                    calendar.select_day(&day);
                }
                hour.set_value(f64::from(at.hour()));
                minute.set_value(f64::from(at.minute() / 5 * 5));
            }
        };
        set(start);

        let presets = gtk::Box::builder().spacing(6).homogeneous(true).build();
        let now = Local::now();
        for (label, at) in [
            (gettext("In 1 Hour"), now + Duration::hours(1)),
            (gettext("Tomorrow Morning"), next_morning(now, None)),
            (
                gettext("Monday Morning"),
                next_morning(now, Some(Weekday::Mon)),
            ),
        ] {
            let button = gtk::Button::builder().label(label).build();
            button.add_css_class("pill");
            let set = set.clone();
            button.connect_clicked(move |_| set(at));
            presets.append(&button);
        }
        let time_row = gtk::Box::builder()
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        time_row.append(&hour);
        time_row.append(&gtk::Label::new(Some(":")));
        time_row.append(&minute);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        content.append(&presets);
        content.append(&calendar);
        content.append(&time_row);

        let dialog = adw::AlertDialog::new(Some(&gettext("Send Later")), None);
        dialog.set_extra_child(Some(&content));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("schedule", &gettext("Schedule")),
        ]);
        dialog.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("schedule"));
        dialog.set_close_response("cancel");
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    if response != "schedule" {
                        return;
                    }
                    let day = calendar.date();
                    let at = Local
                        .with_ymd_and_hms(
                            day.year(),
                            day.month() as u32,
                            day.day_of_month() as u32,
                            hour.value() as u32,
                            minute.value() as u32,
                            0,
                        )
                        .earliest();
                    match at {
                        Some(at) if at > Local::now() => {
                            window.imp().scheduled_for.set(Some(at));
                            window.on_send_clicked();
                        }
                        _ => window.toast(&gettext("Pick a time that hasn't passed yet.")),
                    }
                }
            ),
        );
        dialog.present(Some(self));
    }
}

/// Where the Send Later picker starts: the next whole hour, at least
/// half an hour away.
fn default_send_time(now: DateTime<Local>) -> DateTime<Local> {
    let later = now + Duration::minutes(90);
    let Some(hour) = later.date_naive().and_hms_opt(later.hour(), 0, 0) else {
        return later;
    };
    Local.from_local_datetime(&hour).earliest().unwrap_or(later)
}

/// 08:00 tomorrow, or on the next given weekday (a week on when that's today).
fn next_morning(now: DateTime<Local>, weekday: Option<Weekday>) -> DateTime<Local> {
    let mut day = now.date_naive().succ_opt().unwrap_or(now.date_naive());
    if let Some(weekday) = weekday {
        while day.weekday() != weekday {
            day = day.succ_opt().unwrap_or(day);
        }
    }
    let morning = day.and_time(NaiveTime::from_hms_opt(8, 0, 0).expect("a valid time"));
    Local
        .from_local_datetime(&morning)
        .earliest()
        .unwrap_or(now + Duration::days(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mornings_land_on_the_right_day() {
        // A Wednesday afternoon.
        let wednesday = Local.with_ymd_and_hms(2026, 10, 7, 15, 20, 0).unwrap();
        let tomorrow = next_morning(wednesday, None);
        assert_eq!(
            (tomorrow.day(), tomorrow.time().to_string()),
            (8, "08:00:00".into())
        );
        let monday = next_morning(wednesday, Some(Weekday::Mon));
        assert_eq!((monday.weekday(), monday.day()), (Weekday::Mon, 12));
        // On a Monday, "Monday" is next week's.
        let on_monday = next_morning(monday, Some(Weekday::Mon));
        assert_eq!(on_monday.day(), 19);
        let start = default_send_time(wednesday);
        assert_eq!(start.time().to_string(), "16:00:00");
    }
}

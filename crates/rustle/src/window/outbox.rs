//! The Outbox's schedule. A message sent with Undo waits there a few
//! seconds, one sent with Send Later until its time; a timer wakes the
//! window when the soonest is due, and the usual drain sends it. Until then
//! it can be taken back into the composer.

use super::MainWindow;
use crate::composer::{Draft, QueuedSend};
use crate::i18n::{self, gettext};
use adw::prelude::*;
use adw::subclass::prelude::*;
use chrono::{DateTime, Local, Utc};
use gtk::glib;
use rustle_core::models::{Account, Email};
use rustle_core::{address, compose, folders, html, mime};
use std::collections::HashSet;
use std::time::Duration;

impl MainWindow {
    /// The composer left a message in the Outbox to go later. Say so, with
    /// a way to take it back, and wake when it's due.
    pub(super) fn on_send_queued(&self, queued: QueuedSend) {
        let title = if queued.is_scheduled {
            i18n::format(
                &gettext("Scheduled for {time}"),
                &[("time", &when_label(queued.send_at))],
            )
        } else {
            gettext("Sending…")
        };
        let toast = adw::Toast::builder()
            .title(title)
            .button_label(gettext("Undo"))
            .build();
        if !queued.is_scheduled {
            let wait = (queued.send_at - Local::now()).num_seconds().max(1);
            toast.set_timeout(u32::try_from(wait).unwrap_or(5));
        }
        let email_id = queued.email_id;
        toast.connect_button_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.unsend(email_id)
        ));
        self.imp().toast_overlay.add_toast(toast);
        self.reload_folders();
        self.refresh_keeping_selection();
        self.schedule_outbox();
    }

    /// Wake when the soonest scheduled Outbox message is due, and drain.
    pub(super) fn schedule_outbox(&self) {
        if let Some(timer) = self.state_mut().outbox_timer.take() {
            timer.remove();
        }
        let next = self.db().borrow().next_send_at().ok().flatten();
        let Some(next) = next.and_then(|at| at.parse::<DateTime<Utc>>().ok()) else {
            return;
        };
        let wait = (next - Utc::now()).num_milliseconds().max(0) as u64;
        // A beat past the mark, so the drain finds it due.
        let timer = glib::timeout_add_local_once(
            Duration::from_millis(wait + 250),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || {
                    window.state_mut().outbox_timer = None;
                    let accounts: Vec<Account> =
                        window.state().accounts.values().cloned().collect();
                    for account in accounts {
                        window.drain_outbox(&account);
                    }
                    window.schedule_outbox();
                }
            ),
        );
        self.state_mut().outbox_timer = Some(timer);
    }

    /// Take a message back out of the Outbox and into the composer, as it
    /// was when Send was pressed. Not once it's on its way.
    pub(super) fn unsend(&self, email_id: i64) {
        if self.in_flight().borrow().contains(email_id) {
            self.toast(&gettext("It's already on its way."));
            return;
        }
        let (email, raw, entry) = {
            let db = self.db();
            let db = db.borrow();
            (
                db.email(email_id).ok().flatten(),
                db.raw_message(email_id).ok().flatten(),
                db.outbox_entry(email_id).ok().flatten().unwrap_or_default(),
            )
        };
        let (Some(email), Some(raw)) = (email, raw) else {
            self.toast(&gettext("It has already been sent."));
            return;
        };
        let Some((account, _)) = self.account_for_folder(email.folder_id) else {
            return;
        };
        let parsed = mime::parse_message(&raw);
        // Bcc never makes it into the message; the envelope remembers it.
        let shown: HashSet<String> = parsed
            .to
            .iter()
            .chain(&parsed.cc)
            .flat_map(|text| address::parse_list(text))
            .map(|mailbox| mailbox.address.to_lowercase())
            .collect();
        let bcc: Vec<String> = entry
            .recipients
            .into_iter()
            .filter(|recipient| !shown.contains(&recipient.to_lowercase()))
            .collect();
        let body_html = match &parsed.html_body {
            Some(body) => compose::editable_body(body),
            None => html::to_html(parsed.text_body.as_deref().unwrap_or("").trim_end()),
        };
        if let Err(error) = self.db().borrow().delete_email(email_id) {
            log::error!("could not take message {email_id} out of the Outbox: {error}");
            return;
        }
        // Before the composer opens: it pops out when the selection it
        // opened on changes, as it would if the message left the list after.
        self.reload_folders();
        self.refresh_keeping_selection();
        self.open_composer(
            &account,
            Draft {
                to: parsed.to.join(", "),
                cc: parsed.cc.join(", "),
                bcc: bcc.join(", "),
                subject: parsed.subject,
                body_html,
                attachments: parsed.attachments,
                ..Draft::default()
            },
        );
        self.schedule_outbox();
    }

    /// Send a waiting Outbox message now rather than at its time.
    fn send_now(&self, email: &Email) {
        let Some((account, _)) = self.account_for_folder(email.folder_id) else {
            return;
        };
        let updated = {
            let db = self.db();
            let db = db.borrow();
            let entry = db.outbox_entry(email.id).ok().flatten().unwrap_or_default();
            db.set_outbox_entry(
                email.id,
                &rustle_core::db::OutboxEntry {
                    send_at: String::new(),
                    ..entry
                },
            )
        };
        if let Err(error) = updated {
            log::error!("could not reschedule message {}: {error}", email.id);
            return;
        }
        self.drain_outbox(&account);
        self.schedule_outbox();
    }

    /// Above a message waiting in the Outbox: when it goes, and Edit and
    /// Send Now. None for any other message.
    pub(super) fn outbox_bar(&self, email: &Email) -> Option<gtk::Box> {
        let folder = self.db().borrow().folder(email.folder_id).ok().flatten()?;
        if folder.name != folders::OUTBOX_FOLDER {
            return None;
        }
        let entry = self.db().borrow().outbox_entry(email.id).ok().flatten();
        let due = entry
            .as_ref()
            .and_then(|entry| entry.send_at.parse::<DateTime<Utc>>().ok())
            .filter(|at| *at > Utc::now());
        let text = match due {
            Some(at) => i18n::format(
                &gettext("Scheduled to send {time}"),
                &[("time", &when_label(at.with_timezone(&Local)))],
            ),
            None => gettext("Waiting to send"),
        };
        let bar = gtk::Box::builder()
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .css_classes(["toolbar", "outbox-bar"])
            .build();
        bar.append(
            &gtk::Label::builder()
                .label(text)
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["heading"])
                .build(),
        );
        for (label, is_send) in [(gettext("Edit"), false), (gettext("Send Now"), true)] {
            let button = gtk::Button::with_label(&label);
            if is_send {
                button.add_css_class("suggested-action");
            }
            let email = email.clone();
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_| {
                    if is_send {
                        window.send_now(&email);
                    } else {
                        window.unsend(email.id);
                    }
                }
            ));
            bar.append(&button);
        }
        Some(bar)
    }
}

/// "Thu, Oct 8, 08:00": a send time, in local time.
fn when_label(at: DateTime<Local>) -> String {
    at.format("%a, %b %-d, %H:%M").to_string()
}

//! The card a meeting invitation gets above its message body: what changed,
//! when, who organised it, a Join button, and for an invitation, Accept,
//! Tentative and Decline -- which mail the organizer an iTIP reply.

use crate::i18n::{self, gettext};
use adw::prelude::*;
use gtk::{gio, glib, pango};
use rustle_core::invite::{self, Invitation, Method, Response};
use rustle_core::models::Attachment;
use std::rc::Rc;

const GUTTER: i32 = 12;

pub fn card(
    invitation: &Invitation,
    subject: &str,
    on_save: Rc<dyn Fn(&Attachment)>,
    on_respond: Rc<dyn Fn(Response) -> bool>,
) -> gtk::Box {
    let card = gtk::Box::builder()
        .spacing(GUTTER)
        .css_classes(["invitation-card"])
        .build();
    let icon = gtk::Image::builder()
        .icon_name("x-office-calendar-symbolic")
        .pixel_size(24)
        .valign(gtk::Align::Start)
        .css_classes(["invitation-icon"])
        .build();
    card.append(&icon);

    let lines = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    let status = label(
        &status_text(invitation),
        &["caption-heading", "invitation-status"],
    );
    if invitation.is_cancelled {
        status.add_css_class("error");
    }
    lines.append(&status);

    // The subject usually is the summary; say it only when it adds something.
    if !invitation.summary.is_empty() && !subject.contains(&invitation.summary) {
        lines.append(&label(&invitation.summary, &["heading"]));
    }
    let when = invite::span_label(invitation.start, invitation.end);
    let when_label = label("", &["heading"]);
    when_label.set_selectable(true);
    if invitation.is_cancelled {
        when_label.set_markup(&format!("<s>{}</s>", glib::markup_escape_text(&when)));
    } else {
        when_label.set_label(&when);
    }
    lines.append(&when_label);
    if let Some(previous) = invitation.previous_start {
        let previous = invite::span_label(previous, invitation.previous_end);
        let moved = label("", &["caption", "dim-label"]);
        moved.set_markup(&format!(
            "{} <s>{}</s>",
            glib::markup_escape_text(&gettext("Was")),
            glib::markup_escape_text(&previous)
        ));
        lines.append(&moved);
    }

    let mut details = Vec::new();
    if !invitation.location.is_empty() {
        details.push(invitation.location.clone());
    }
    if let Some(organizer) = &invitation.organizer {
        details.push(i18n::format(
            &gettext("Organised by {name}"),
            &[("name", organizer.label())],
        ));
    }
    if invitation.is_recurring {
        details.push(gettext("Recurring"));
    }
    if !details.is_empty() {
        lines.append(&label(&details.join(" · "), &["caption", "dim-label"]));
    }
    let can_answer = invitation.method == Method::Request
        && !invitation.is_cancelled
        && invitation.organizer.is_some();
    if can_answer {
        lines.append(&answer_row(on_respond));
    }
    card.append(&lines);

    let buttons = gtk::Box::builder()
        .spacing(6)
        .valign(gtk::Align::Center)
        .build();
    if let Some(url) = invitation
        .meeting_url
        .clone()
        .filter(|_| !invitation.is_cancelled)
    {
        let join = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("camera-video-symbolic")
                    .label(gettext("Join"))
                    .build(),
            )
            .tooltip_text(&url)
            .css_classes(["suggested-action", "pill", "invitation-join"])
            .build();
        join.connect_clicked(move |button| {
            open_link(button.root().and_downcast::<gtk::Window>().as_ref(), &url)
        });
        buttons.append(&join);
    }
    let save = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text(gettext("Save Invitation"))
        .css_classes(["flat", "circular"])
        .build();
    let ics = invitation.ics.clone();
    save.connect_clicked(move |_| on_save(&ics));
    buttons.append(&save);
    card.append(&buttons);
    card
}

/// Accept, Tentative, Decline. Once one goes out the row says which, so a
/// second click can't send a second, different answer by accident.
fn answer_row(on_respond: Rc<dyn Fn(Response) -> bool>) -> gtk::Box {
    let row = gtk::Box::builder()
        .spacing(6)
        .margin_top(6)
        .css_classes(["invitation-answers"])
        .build();
    let choices = [
        (Response::Accepted, gettext("Accept"), "suggested-action"),
        (Response::Tentative, gettext("Tentative"), ""),
        (Response::Declined, gettext("Decline"), "destructive-action"),
    ];
    for (response, title, class) in choices {
        let button = gtk::Button::builder().label(title).build();
        button.add_css_class("pill");
        if !class.is_empty() {
            button.add_css_class(class);
        }
        let on_respond = on_respond.clone();
        let row_ref = row.downgrade();
        button.connect_clicked(move |_| {
            if !on_respond(response) {
                return;
            }
            let Some(row) = row_ref.upgrade() else { return };
            while let Some(child) = row.first_child() {
                row.remove(&child);
            }
            let said = match response {
                Response::Accepted => gettext("You accepted"),
                Response::Tentative => gettext("You tentatively accepted"),
                _ => gettext("You declined"),
            };
            row.append(&label(&said, &["caption-heading", "dim-label"]));
        });
        row.append(&button);
    }
    row
}

/// Hand a link from a message to the desktop: a Teams meeting to the Teams
/// app when one is installed (teams-for-linux, say), anything else -- or
/// Teams without an app -- to the browser.
pub fn open_link(window: Option<&gtk::Window>, uri: &str) {
    let target = invite::teams_app_uri(uri)
        .filter(|_| gio::AppInfo::default_for_uri_scheme("msteams").is_some())
        .unwrap_or_else(|| uri.to_string());
    let fallback = (target != uri).then(|| uri.to_string());
    let window_for_fallback = window.cloned();
    gtk::UriLauncher::new(&target).launch(window, gio::Cancellable::NONE, move |result| {
        let Err(error) = result else { return };
        log::warn!("could not open {target}: {error}");
        if let Some(uri) = fallback {
            gtk::UriLauncher::new(&uri).launch(
                window_for_fallback.as_ref(),
                gio::Cancellable::NONE,
                |_| {},
            );
        }
    });
}

fn status_text(invitation: &Invitation) -> String {
    let organizer = invitation
        .organizer
        .as_ref()
        .map(|o| o.label().to_string())
        .unwrap_or_default();
    match invitation.method {
        Method::Cancel => gettext("Meeting cancelled"),
        _ if invitation.is_cancelled => gettext("Meeting cancelled"),
        Method::Reply => {
            let Some((attendee, response)) = &invitation.reply else {
                return gettext("Meeting response");
            };
            let template = match response {
                Response::Accepted => gettext("{name} accepted"),
                Response::Tentative => gettext("{name} tentatively accepted"),
                Response::Declined => gettext("{name} declined"),
                Response::Other => gettext("{name} responded"),
            };
            i18n::format(&template, &[("name", attendee.label())])
        }
        Method::Request if invitation.previous_start.is_some() => gettext("Meeting moved"),
        Method::Request if invitation.sequence > 0 && !organizer.is_empty() => i18n::format(
            &gettext("{name} updated the meeting"),
            &[("name", &organizer)],
        ),
        Method::Request if invitation.sequence > 0 => gettext("Meeting updated"),
        Method::Request => gettext("Meeting invitation"),
        Method::Other => gettext("Event"),
    }
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .css_classes(classes.to_vec())
        .build()
}

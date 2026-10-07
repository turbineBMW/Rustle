//! The reading pane: the selected message and its attachments.

use super::{MainWindow, PAGE_EMPTY, PAGE_MESSAGE};
use crate::i18n::{self, gettext};
use crate::objects::EmailObject;
use crate::settings as keys;
use crate::widgets::message_view::{Handlers, LoadCallback, MessageView, RenderedCallback};
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use chrono::Utc;
use gtk::gio;
use gtk::glib;
use rustle_core::db::OutboxEntry;
use rustle_core::invite::{self, Invitation, Person, Response};
use rustle_core::mime::Unsubscribe;
use rustle_core::models::{Account, Attachment, Email, MessageHeader};
use rustle_core::net::errors::classify;
use rustle_core::{compose, dates, folders};
use rustle_core::{secrets, sync};
use std::path::Path;
use std::rc::Rc;

/// Which message body to fetch, as plain values rather than the live Email.
#[derive(Clone)]
struct BodyRequest {
    email_id: i64,
    uid: String,
    folder_name: String,
}

impl MainWindow {
    pub(super) fn setup_message_handlers(&self) {
        let window = self.downgrade();
        let on_load = {
            let window = window.clone();
            move |email: &Email, callback: LoadCallback| {
                if let Some(window) = window.upgrade() {
                    window.load_body(email, callback);
                }
            }
        };
        let on_save = {
            let window = window.clone();
            move |attachment: &Attachment| {
                if let Some(window) = window.upgrade() {
                    window.save_attachment(attachment);
                }
            }
        };
        let on_open = {
            let window = window.clone();
            move |attachment: &Attachment| {
                if let Some(window) = window.upgrade() {
                    window.open_attachment(attachment);
                }
            }
        };
        let on_unsubscribe = {
            let window = window.clone();
            move |target: &Unsubscribe, on_done: Box<dyn Fn()>| {
                if let Some(window) = window.upgrade() {
                    window.on_unsubscribe(target, on_done);
                }
            }
        };
        let on_respond = {
            let window = window.clone();
            move |email: &Email, invitation: &Invitation, response: Response| {
                window
                    .upgrade()
                    .is_some_and(|window| window.respond_to_invitation(email, invitation, response))
            }
        };
        let on_related = {
            let window = window.clone();
            move |email: &Email| {
                window
                    .upgrade()
                    .map(|window| window.related_to(email))
                    .unwrap_or_default()
            }
        };
        let on_open_related = move |email: &Email| {
            if let Some(window) = window.upgrade() {
                window.open_related(email);
            }
        };
        self.state_mut().message_handlers = Some(Rc::new(Handlers {
            on_load: Rc::new(on_load),
            on_save_attachment: Rc::new(on_save),
            on_open_attachment: Rc::new(on_open),
            on_unsubscribe: Rc::new(on_unsubscribe),
            on_respond: Rc::new(on_respond),
            on_related: Rc::new(on_related),
            on_open_related: Rc::new(on_open_related),
        }));
    }

    pub(super) fn update_reader(&self) {
        // The email store only fills once a mail view is loaded, and
        // there is nothing to read before then.
        if self.state().view.is_none() {
            return;
        }
        // Something else to read: an inline composer makes way for it.
        self.on_selection_moved();
        let imp = self.imp();
        let selected = self.selected_emails();
        self.update_move_menu();
        if selected.len() != 1 {
            imp.edit_draft_button.set_visible(false);
            {
                let mut state = self.state_mut();
                state.rendered_id = None;
            }
            self.set_reply_forward_enabled(false);
            self.set_mail_actions_enabled(!selected.is_empty());
            if !selected.is_empty() {
                self.update_action_buttons(&selected);
            }
            // Hiding the pane isn't enough: the view behind it keeps its
            // WebView and holds a web process open.
            self.clear_message();
            imp.reader_stack.set_visible_child_name(PAGE_EMPTY);
            return;
        }

        let email = &selected[0];
        self.update_action_buttons(&selected);
        self.set_reply_forward_enabled(false);
        imp.edit_draft_button
            .set_visible(self.is_in_drafts(email.with(|e| e.folder_id)));

        // Already showing this message (e.g. after a flag change) -- don't rebuild.
        if self.state().rendered_id == Some(email.id()) {
            let is_ready = self
                .state()
                .message_view
                .as_ref()
                .is_some_and(|view| view.raw().is_some());
            self.set_reply_forward_enabled(is_ready);
            imp.reader_stack.set_visible_child_name(PAGE_MESSAGE);
            return;
        }

        {
            let mut state = self.state_mut();
            state.rendered_id = Some(email.id());
        }
        self.render_message(email);
        // Opening an email marks it read (like most mail clients).
        self.mark_email_read(email);
    }

    /// Reflect the selected emails' state on the action buttons.
    fn update_action_buttons(&self, selected: &[EmailObject]) {
        let imp = self.imp();
        self.set_mail_actions_enabled(true);
        if selected.iter().any(|c| c.with(|c| c.is_unread)) {
            imp.mark_read_button.set_icon_name("mail-read-symbolic");
            imp.mark_read_button
                .set_tooltip_text(Some(&gettext("Mark Read")));
        } else {
            imp.mark_read_button.set_icon_name("mail-unread-symbolic");
            imp.mark_read_button
                .set_tooltip_text(Some(&gettext("Mark Unread")));
        }
        if selected.iter().any(|c| c.with(|c| c.is_starred)) {
            imp.star_button.set_icon_name("starred-symbolic");
            imp.star_button.set_tooltip_text(Some(&gettext("Unstar")));
        } else {
            imp.star_button.set_icon_name("non-starred-symbolic");
            imp.star_button.set_tooltip_text(Some(&gettext("Star")));
        }
        if selected.iter().any(|c| c.with(|c| c.is_pinned)) {
            imp.pin_button.set_tooltip_text(Some(&gettext("Unpin")));
            imp.pin_button.add_css_class("pinned");
        } else {
            imp.pin_button.set_tooltip_text(Some(&gettext("Pin")));
            imp.pin_button.remove_css_class("pinned");
        }
    }

    /// Empty the reading pane and release its WebView.
    pub(super) fn clear_message(&self) {
        let bar = self.state_mut().outbox_bar.take();
        if let Some(bar) = bar {
            self.imp().message_box.remove(&bar);
        }
        let view = self.state_mut().message_view.take();
        if let Some(view) = view {
            self.imp().message_box.remove(view.widget());
            view.release();
        }
    }

    /// Display only the selected email.
    fn render_message(&self, email: &EmailObject) {
        let imp = self.imp();
        let email = email.get();
        imp.reader_subject.set_label(&email.subject);
        self.clear_message();

        let should_load_remote_images = self.settings().boolean(keys::LOAD_REMOTE_IMAGES);
        let handlers = self
            .state()
            .message_handlers
            .clone()
            .expect("set at construction");
        let window = self.downgrade();
        let email_id = email.id;
        let on_rendered: RenderedCallback = Box::new(move || {
            if let Some(window) = window.upgrade() {
                if window.state().rendered_id == Some(email_id) {
                    window.set_reply_forward_enabled(true);
                }
            }
        });
        let account = if self.is_unified_view() {
            self.account_for_folder(email.folder_id)
                .map(|(account, _)| account)
        } else {
            None
        };
        let email_for_bar = email.clone();
        let view = MessageView::new(
            email,
            handlers,
            on_rendered,
            should_load_remote_images,
            &self.avatars(),
            account.as_ref(),
        );
        view.set_zoom(self.reader_zoom());
        let bar = self.outbox_bar(&email_for_bar);
        if let Some(bar) = &bar {
            imp.message_box.append(bar);
        }
        self.state_mut().outbox_bar = bar;
        imp.message_box.append(view.widget());
        self.state_mut().message_view = Some(view);
        imp.reader_stack.set_visible_child_name(PAGE_MESSAGE);
    }

    /// Fetch one message's raw bytes for a MessageView: serve the cached copy
    /// if we have it, else pull it over IMAP on a worker thread.
    fn load_body(&self, email: &Email, callback: LoadCallback) {
        let cached = self.db().borrow().raw_message(email.id).ok().flatten();
        if let Some(cached) = cached {
            callback(Some(cached), None);
            return;
        }
        let Some(uid) = email.server_id.clone() else {
            // No UID and no cached copy: nothing to fetch until the next sync.
            callback(
                None,
                Some(gettext("This message hasn't finished syncing yet.")),
            );
            return;
        };
        let Some((account, folder)) = self.account_for_folder(email.folder_id) else {
            callback(None, Some(gettext("No account is open.")));
            return;
        };
        let request = BodyRequest {
            email_id: email.id,
            uid,
            folder_name: folder.name,
        };
        let job = request.clone();
        workers::run(
            move || -> Result<Vec<u8>, String> {
                let Some(credential) = secrets::credential_for(&account) else {
                    log::warn!("could not sign in to account {}", account.email);
                    return Err(gettext("Could not sign in to this account."));
                };
                sync::fetch_full_message(&account, &credential, &job.folder_name, &job.uid).map_err(
                    |error| {
                        log::error!(
                            "could not fetch message uid {} from {} (account {}): {error}",
                            job.uid,
                            job.folder_name,
                            account.email
                        );
                        i18n::failure_message(&classify(&error, &account.imap_host))
                    },
                )
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result: Result<Vec<u8>, String>| match result {
                    Ok(raw) => {
                        // Back on the main thread: cache the body, then hand it over.
                        if let Err(error) = window
                            .db()
                            .borrow()
                            .save_raw_message(request.email_id, &raw)
                        {
                            log::warn!("could not cache message {}: {error}", request.email_id);
                        }
                        callback(Some(raw), None);
                    }
                    Err(message) => callback(None, Some(message)),
                }
            ),
        );
    }

    fn save_attachment(&self, attachment: &Attachment) {
        let dialog = gtk::FileDialog::builder()
            .initial_name(&attachment.filename)
            .build();
        let attachment = attachment.clone();
        dialog.save(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result| {
                    let Ok(file) = result else { return }; // cancelled
                                                           // A full disk or a read-only target fails here, not in the dialog.
                    match file.replace_contents(
                        &attachment.content,
                        None,
                        false,
                        gio::FileCreateFlags::NONE,
                        gio::Cancellable::NONE,
                    ) {
                        Ok(_) => window.toast(&i18n::format(
                            &gettext("Saved {name}."),
                            &[("name", &attachment.filename)],
                        )),
                        Err(error) => {
                            log::error!("could not save attachment to {:?}: {error}", file.path());
                            window.toast(&i18n::format(
                                &gettext("Couldn't save {name}: {msg}"),
                                &[("name", &attachment.filename), ("msg", &error.to_string())],
                            ));
                        }
                    }
                }
            ),
        );
    }

    fn open_attachment(&self, attachment: &Attachment) {
        // Server-supplied name; "../../.bashrc" would otherwise escape the dir.
        let name = Path::new(&attachment.filename)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into());
        let directory = glib::user_cache_dir().join("rustle").join("attachments");
        let _ = std::fs::remove_dir_all(&directory); // keep only the newest copy
        let path = directory.join(&name);
        let written = std::fs::create_dir_all(&directory)
            .and_then(|_| std::fs::write(&path, &attachment.content));
        if let Err(error) = written {
            log::error!(
                "could not write attachment {name} to {}: {error}",
                path.display()
            );
            self.toast(&i18n::format(
                &gettext("Couldn't open {name}."),
                &[("name", &attachment.filename)],
            ));
            return;
        }
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
        // Server-supplied bytes: make the portal ask which app, so one click
        // can't hand an arbitrary file straight to its default handler.
        launcher.set_always_ask(true);
        let filename = attachment.filename.clone();
        launcher.launch(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result| {
                    if let Err(error) = result {
                        if error.matches(gio::IOErrorEnum::Cancelled) {
                            return;
                        }
                        log::warn!("could not open attachment {filename}: {error}");
                        window.toast(&i18n::format(
                            &gettext("Couldn't open {name}."),
                            &[("name", &filename)],
                        ));
                    }
                }
            ),
        );
    }

    /// The rest of `email`'s conversation, each with the folder it's in
    /// (and the account, when there's more than one).
    fn related_to(&self, email: &Email) -> Vec<(Email, String)> {
        let related = match self.db().borrow().related_emails(email) {
            Ok(related) => related,
            Err(error) => {
                log::error!(
                    "could not look up the conversation of message {}: {error}",
                    email.id
                );
                return Vec::new();
            }
        };
        let many_accounts = self.state().accounts.len() > 1;
        related
            .into_iter()
            .map(|message| {
                let place = self
                    .account_for_folder(message.folder_id)
                    .map(|(account, folder)| {
                        let name = folders::display_name_for_folder(&folder.name, None);
                        if many_accounts {
                            format!("{name} ({})", account.email)
                        } else {
                            name
                        }
                    })
                    .unwrap_or_default();
                (message, place)
            })
            .collect()
    }

    /// Answer an invitation: mail the organizer an iTIP reply from the
    /// account the invitation came to. It goes through the Outbox like any
    /// sent mail, so it survives being offline. True once it's queued.
    fn respond_to_invitation(
        &self,
        email: &Email,
        invitation: &Invitation,
        response: Response,
    ) -> bool {
        let Some((account, _)) = self.account_for_folder(email.folder_id) else {
            self.toast(&gettext(
                "Only an invitation in one of your accounts can be answered.",
            ));
            return false;
        };
        let Some(organizer) = &invitation.organizer else {
            return false;
        };
        if organizer.email.eq_ignore_ascii_case(&account.email) {
            self.toast(&gettext("You organised this meeting."));
            return false;
        }
        let me = Person {
            name: account.display_name.clone(),
            email: account.email.clone(),
        };
        let Some(ics) = invite::reply_ics(&invitation.ics.content, &me, response, Utc::now())
        else {
            self.toast(&gettext("Couldn't read this invitation."));
            return false;
        };
        let name = if me.name.is_empty() {
            me.email.clone()
        } else {
            me.name.clone()
        };
        let summary = if invitation.summary.is_empty() {
            email.subject.clone()
        } else {
            invitation.summary.clone()
        };
        let (subject, text) = match response {
            Response::Accepted => (
                gettext("Accepted: {summary}"),
                gettext("{name} has accepted."),
            ),
            Response::Tentative => (
                gettext("Tentative: {summary}"),
                gettext("{name} has tentatively accepted."),
            ),
            _ => (
                gettext("Declined: {summary}"),
                gettext("{name} has declined."),
            ),
        };
        let subject = i18n::format(&subject, &[("summary", &summary)]);
        let text = i18n::format(&text, &[("name", &name)]);
        let raw = match compose::invitation_reply_message(
            &account.email,
            &organizer.email,
            &subject,
            &text,
            &ics,
        ) {
            Ok(raw) => raw,
            Err(error) => {
                log::error!(
                    "could not build a reply to the invitation {summary:?} (account {}): {error}",
                    account.email
                );
                self.toast(&gettext("Couldn't read this invitation."));
                return false;
            }
        };
        let header = MessageHeader {
            sender: account.email.clone(),
            recipient: organizer.label().to_string(),
            recipient_address: organizer.email.clone(),
            subject: subject.clone(),
            preview: text.clone(),
            ..MessageHeader::default()
        };
        self.queue_outgoing(&account, &raw, header, vec![organizer.email.clone()])
    }

    /// Put a message the app wrote itself (an invitation answer, an
    /// unsubscribe request) in the account's Outbox and send it: it gets the
    /// Outbox's retries, and its copy in Sent. True once it's queued.
    fn queue_outgoing(
        &self,
        account: &Account,
        raw: &[u8],
        header: MessageHeader,
        recipients: Vec<String>,
    ) -> bool {
        let queued = {
            let db = self.db();
            let db = db.borrow();
            db.get_or_create_folder(
                account.id,
                folders::OUTBOX_FOLDER,
                folders::icon_for_folder(folders::OUTBOX_FOLDER),
            )
            .and_then(|outbox| {
                let row = db.save_email(
                    outbox.id,
                    &MessageHeader {
                        date: dates::now_iso(),
                        is_unread: false,
                        ..header
                    },
                )?;
                db.save_raw_message(row.id, raw)?;
                db.set_outbox_entry(
                    row.id,
                    &OutboxEntry {
                        recipients,
                        ..OutboxEntry::default()
                    },
                )
            })
        };
        if let Err(error) = queued {
            log::error!(
                "could not queue a message in the Outbox of {}: {error}",
                account.email
            );
            return false;
        }
        self.drain_outbox(account);
        true
    }

    /// A list that only takes unsubscribes by mail: after asking, send the
    /// message its mailto: describes, from the account this mail came to.
    fn unsubscribe_by_mail(&self, mailto: &str, on_done: Box<dyn Fn()>) {
        let account = self
            .selected_email()
            .and_then(|email| self.account_for_folder(email.with(|e| e.folder_id)))
            .map(|(account, _)| account);
        let request = compose::parse_mailto(mailto);
        let Some(account) = account.filter(|_| !request.to.is_empty()) else {
            self.open_mailto(mailto);
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Unsubscribe from this list?"))
            .body(i18n::format(
                &gettext("An email will be sent to {destination} from {account}."),
                &[("destination", &request.to), ("account", &account.email)],
            ))
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("unsubscribe", &gettext("Unsubscribe"));
        dialog.set_response_appearance("unsubscribe", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    if response != "unsubscribe" {
                        return;
                    }
                    let subject = if request.subject.trim().is_empty() {
                        "unsubscribe".to_string()
                    } else {
                        request.subject.clone()
                    };
                    let body = if request.body_html.trim().is_empty() {
                        "unsubscribe".to_string()
                    } else {
                        request.body_html.clone()
                    };
                    let to = compose::split_addresses(&request.to);
                    let message = compose::Outgoing {
                        from: &account.email,
                        to: &to,
                        subject: &subject,
                        body_html: &body,
                        plain_text: true,
                        ..compose::Outgoing::default()
                    };
                    let raw = match compose::build_mime_message(&message) {
                        Ok(raw) => raw,
                        Err(error) => {
                            log::error!(
                                "could not build an unsubscribe request to {to:?}: {error}"
                            );
                            window.toast(&gettext("Couldn't unsubscribe."));
                            return;
                        }
                    };
                    let header = MessageHeader {
                        sender: account.email.clone(),
                        recipient: request.to.clone(),
                        recipient_address: request.to.clone(),
                        subject: subject.clone(),
                        ..MessageHeader::default()
                    };
                    if window.queue_outgoing(&account, &raw, header, to) {
                        on_done();
                        window.toast(&gettext(
                            "Unsubscribe request sent. It can take a few days to take effect.",
                        ));
                    }
                }
            ),
        );
        dialog.present(Some(self));
    }

    fn on_unsubscribe(&self, target: &Unsubscribe, on_done: Box<dyn Fn()>) {
        // Without a One-Click header the list wants a human at the other
        // end: hand it to the browser, or to the composer if all the list
        // published was a mailto. The banner stays -- nothing was sent.
        if !target.is_one_click {
            if !target.url.is_empty() {
                gtk::UriLauncher::new(&target.url).launch(
                    Some(self),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            } else {
                self.unsubscribe_by_mail(&target.mailto, on_done);
            }
            return;
        }
        // A one-click POST cannot be taken back, and its address comes out
        // of a stranger's header, so name where the request is going before
        // making it. Cancel stays the default.
        let host = url::Url::parse(&target.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| target.url.clone());
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Unsubscribe from this list?"))
            .body(i18n::format(
                &gettext("A request will be sent to {destination}."),
                &[("destination", &host)],
            ))
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("unsubscribe", &gettext("Unsubscribe"));
        dialog.set_response_appearance("unsubscribe", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        let url = target.url.clone();
        let on_done = Rc::new(on_done);
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    if response != "unsubscribe" {
                        return;
                    }
                    let url = url.clone();
                    let on_done = on_done.clone();
                    let host = host.clone();
                    // Needs no credentials: the list authenticates the request
                    // by the opaque token already in the URL.
                    workers::run(
                        move || sync::post_unsubscribe(&url),
                        glib::clone!(
                            #[weak]
                            window,
                            move |result: Result<(), String>| match result {
                                Ok(()) => {
                                    on_done();
                                    window.toast(&gettext(
                                        "Unsubscribed. It can take a few days to take effect.",
                                    ));
                                }
                                Err(error) => {
                                    log::error!("could not unsubscribe via {host}: {error}");
                                    window.toast(&gettext(
                                        "Couldn't unsubscribe. The list didn't accept the request.",
                                    ));
                                }
                            }
                        ),
                    );
                }
            ),
        );
        dialog.present(Some(self));
    }
}

//! The reading pane: the thread, message bodies, and attachments.

use super::{MainWindow, PAGE_EMPTY, PAGE_MESSAGE};
use crate::i18n::{self, gettext};
use crate::objects::ConversationObject;
use crate::settings as keys;
use crate::widgets::message_view::{Handlers, LoadCallback, MessageView, RenderedCallback};
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::mime::Unsubscribe;
use rustle_core::models::{Attachment, Email};
use rustle_core::net::errors::classify;
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
        let on_unsubscribe = move |target: &Unsubscribe, on_done: Box<dyn Fn()>| {
            if let Some(window) = window.upgrade() {
                window.on_unsubscribe(target, on_done);
            }
        };
        self.state_mut().message_handlers = Some(Rc::new(Handlers {
            on_load: Rc::new(on_load),
            on_save_attachment: Rc::new(on_save),
            on_open_attachment: Rc::new(on_open),
            on_unsubscribe: Rc::new(on_unsubscribe),
        }));
    }

    pub(super) fn update_reader(&self) {
        // The conversation store only fills once a mail view is loaded, and
        // there is nothing to read before then.
        if self.state().view.is_none() {
            return;
        }
        let imp = self.imp();
        let selected = self.selected_conversations();
        self.update_move_menu();
        if selected.len() != 1 {
            {
                let mut state = self.state_mut();
                state.rendered_id = None;
                state.active_view = None;
            }
            self.set_reply_forward_enabled(false);
            self.set_mail_actions_enabled(!selected.is_empty());
            if !selected.is_empty() {
                self.update_action_buttons(&selected);
            }
            // Hiding the pane isn't enough: the views behind it keep their
            // WebViews, and each one holds a web process open.
            self.clear_thread();
            imp.reader_stack.set_visible_child_name(PAGE_EMPTY);
            return;
        }

        let conversation = &selected[0];
        self.update_action_buttons(&selected);
        self.set_reply_forward_enabled(false);

        // Already showing this thread (e.g. after a flag change) -- don't rebuild.
        if self.state().rendered_id == Some(conversation.id()) {
            let is_ready = self
                .state()
                .active_view
                .as_ref()
                .is_some_and(|view| view.raw().is_some());
            self.set_reply_forward_enabled(is_ready);
            imp.reader_stack.set_visible_child_name(PAGE_MESSAGE);
            return;
        }

        {
            let mut state = self.state_mut();
            state.rendered_id = Some(conversation.id());
            state.active_view = None;
        }
        self.render_thread(conversation);
        // Opening a conversation marks it read (like most mail clients).
        self.mark_conversation_read(conversation);
    }

    /// Reflect the selected conversations' state on the action buttons.
    fn update_action_buttons(&self, selected: &[ConversationObject]) {
        let imp = self.imp();
        self.set_mail_actions_enabled(true);
        if selected.iter().any(|c| c.with(|c| c.is_unread())) {
            imp.mark_read_button.set_icon_name("mail-read-symbolic");
            imp.mark_read_button
                .set_tooltip_text(Some(&gettext("Mark Read")));
        } else {
            imp.mark_read_button.set_icon_name("mail-unread-symbolic");
            imp.mark_read_button
                .set_tooltip_text(Some(&gettext("Mark Unread")));
        }
        if selected.iter().any(|c| c.with(|c| c.is_starred())) {
            imp.star_button.set_icon_name("starred-symbolic");
            imp.star_button.set_tooltip_text(Some(&gettext("Unstar")));
        } else {
            imp.star_button.set_icon_name("non-starred-symbolic");
            imp.star_button.set_tooltip_text(Some(&gettext("Star")));
        }
    }

    /// Empty the reading pane, releasing each view's WebView as it goes.
    pub(super) fn clear_thread(&self) {
        let views = std::mem::take(&mut self.state_mut().thread_views);
        let thread_box = &self.imp().thread_box;
        for view in views {
            thread_box.remove(view.widget());
            view.release();
        }
    }

    /// Build one MessageView per email, newest first. The newest starts
    /// expanded (which loads its body); older ones load when expanded.
    fn render_thread(&self, conversation: &ConversationObject) {
        let imp = self.imp();
        imp.reader_subject
            .set_label(&conversation.with(|c| c.subject().to_string()));
        self.clear_thread();

        let should_load_remote_images = self.settings().boolean(keys::LOAD_REMOTE_IMAGES);
        let handlers = self
            .state()
            .message_handlers
            .clone()
            .expect("set at construction");
        let avatars = self.avatars();
        let emails: Vec<Email> = conversation.with(|c| c.emails.iter().rev().cloned().collect());
        let mut views = Vec::with_capacity(emails.len());
        for (index, email) in emails.into_iter().enumerate() {
            let is_newest = index == 0;
            let on_rendered: Option<RenderedCallback> = if is_newest {
                let window = self.downgrade();
                Some(Box::new(move |view: &MessageView| {
                    if let Some(window) = window.upgrade() {
                        window.on_newest_rendered(view);
                    }
                }))
            } else {
                None
            };
            let view = MessageView::new(
                email,
                handlers.clone(),
                on_rendered,
                is_newest,
                should_load_remote_images,
                &avatars,
            );
            imp.thread_box.append(view.widget());
            views.push(view);
        }
        self.state_mut().thread_views = views;
        imp.reader_stack.set_visible_child_name(PAGE_MESSAGE);
    }

    fn on_newest_rendered(&self, view: &MessageView) {
        if self.selected_conversations().len() != 1 {
            return;
        }
        self.state_mut().active_view = Some(view.clone());
        self.set_reply_forward_enabled(true);
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
                self.open_mailto(&target.mailto);
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

//! What the reader's menu does with the open message: print it, save it as
//! .eml, show its source, zoom it. And the reverse of saving: a .eml file
//! opened from the desktop shows in a window of its own.

use super::MainWindow;
use crate::i18n::{self, gettext};
use crate::settings as keys;
use crate::widgets::message_view::{Handlers, LoadCallback, MessageView};
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use rustle_core::mime;
use rustle_core::models::Email;
use rustle_core::{address, dates};
use std::rc::Rc;

/// The steps Zoom In and Zoom Out walk, as in a browser.
const ZOOM_STEPS: [f64; 12] = [
    0.5, 0.67, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0,
];

impl MainWindow {
    /// Follow the zoom setting, and zoom on Ctrl+scroll over the message.
    pub(super) fn setup_reader_tools(&self) {
        self.settings().connect_changed(
            Some(keys::READER_ZOOM),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |settings, key| {
                    let view = window.state().message_view.clone();
                    if let Some(view) = view {
                        view.set_zoom(settings.double(key));
                    }
                }
            ),
        );
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        // Ahead of the web view, which would otherwise scroll the page.
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        scroll.connect_scroll(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |controller, _, dy| {
                let is_ctrl = controller
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK);
                if !is_ctrl || dy == 0.0 {
                    return glib::Propagation::Proceed;
                }
                window.step_zoom(if dy < 0.0 { 1 } else { -1 });
                glib::Propagation::Stop
            }
        ));
        self.imp().message_box.add_controller(scroll);
    }

    /// The zoom a fresh message view starts at.
    pub(super) fn reader_zoom(&self) -> f64 {
        self.settings().double(keys::READER_ZOOM)
    }

    /// One step up (1), down (-1), or back to normal (0).
    pub(super) fn step_zoom(&self, direction: i32) {
        let settings = self.settings();
        let zoom = next_zoom(settings.double(keys::READER_ZOOM), direction);
        let _ = settings.set_double(keys::READER_ZOOM, zoom);
    }

    pub(super) fn print_message(&self) {
        let view = self.state().message_view.clone();
        if let Some(view) = view {
            view.print(self.upcast_ref());
        }
    }

    pub(super) fn save_message(&self) {
        let view = self.state().message_view.clone();
        let Some((raw, parsed)) = view.and_then(|view| Some((view.raw()?, view.parsed()?))) else {
            return;
        };
        let name = mime::eml_filename(&parsed.subject);
        let dialog = gtk::FileDialog::builder().initial_name(&name).build();
        dialog.save(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result| {
                    let Ok(file) = result else { return }; // cancelled
                    let saved = file.replace_contents(
                        &raw,
                        None,
                        false,
                        gio::FileCreateFlags::NONE,
                        gio::Cancellable::NONE,
                    );
                    match saved {
                        Ok(_) => window
                            .toast(&i18n::format(&gettext("Saved {name}."), &[("name", &name)])),
                        Err(error) => {
                            log::error!("could not save a message to {:?}: {error}", file.path());
                            window.toast(&i18n::format(
                                &gettext("Couldn't save {name}: {msg}"),
                                &[("name", &name), ("msg", &error.to_string())],
                            ));
                        }
                    }
                }
            ),
        );
    }

    /// The message exactly as the server sent it, headers and all.
    pub(super) fn show_source(&self) {
        let view = self.state().message_view.clone();
        let Some(raw) = view.and_then(|view| view.raw()) else {
            return;
        };
        let buffer = gtk::TextBuffer::new(None);
        buffer.set_text(&String::from_utf8_lossy(&raw));
        let text = gtk::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(12)
            .bottom_margin(12)
            .left_margin(12)
            .right_margin(12)
            .build();
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(gettext("Copy Source"))
            .build();
        copy.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            buffer,
            move |button| {
                let (start, end) = buffer.bounds();
                button
                    .clipboard()
                    .set_text(&buffer.text(&start, &end, false));
                window.toast(&gettext("Copied the message source."));
            }
        ));
        let header = adw::HeaderBar::new();
        header.pack_start(&copy);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .child(&text)
                .vexpand(true)
                .build(),
        ));
        adw::Dialog::builder()
            .title(gettext("Message Source"))
            .content_width(720)
            .content_height(640)
            .child(&toolbar)
            .build()
            .present(Some(self));
    }

    /// Show a message from a file (a .eml opened from the desktop) in a window
    /// of its own. It belongs to no account, so the reader's actions stay
    /// with the main window; attachments save and open as usual.
    pub fn show_eml(&self, raw: Vec<u8>) {
        let parsed = mime::parse_message(&raw);
        let email = Email {
            id: -1,
            folder_id: -1,
            server_id: None,
            sender: address::display_name(&parsed.from_header),
            sender_address: address::first_address(&parsed.from_header),
            recipient: String::new(),
            recipient_address: String::new(),
            subject: parsed.subject.clone(),
            preview: String::new(),
            date: dates::to_iso(&parsed.date_header),
            is_unread: false,
            is_starred: false,
            is_pinned: false,
            message_id: parsed.message_id.clone(),
            thread_root: String::new(),
            thread_outlook: String::new(),
            thread_size: 1,
        };
        let Some(shared) = self.state().message_handlers.clone() else {
            return;
        };
        let handlers = Handlers {
            on_load: Rc::new(move |_: &Email, callback: LoadCallback| {
                callback(Some(raw.clone()), None)
            }),
            ..(*shared).clone()
        };
        self.present_viewer(email, Rc::new(handlers));
    }

    /// A message from the reader's Related list: selected in place when the
    /// list is showing it, else opened in a viewer window so the folder on
    /// screen stays put.
    pub(super) fn open_related(&self, email: &Email) {
        let shown = self
            .list_emails_with_ids(&std::collections::HashSet::from([email.id]))
            .pop();
        if let Some(shown) = shown {
            let model = self.email_model();
            let position = (0..model.n_items()).find(|&index| {
                model
                    .item(index)
                    .and_downcast::<crate::objects::EmailObject>()
                    .is_some_and(|item| item == shown)
            });
            if let Some(position) = position {
                let selection = self.selection();
                selection.select_item(position, true);
                self.imp()
                    .email_list
                    .scroll_to(position, gtk::ListScrollFlags::FOCUS, None);
                return;
            }
        }
        let Some(handlers) = self.state().message_handlers.clone() else {
            return;
        };
        self.present_viewer(email.clone(), handlers);
    }

    /// One message in a window of its own, read through `handlers`.
    fn present_viewer(&self, email: Email, handlers: Rc<Handlers>) {
        let title = if email.subject.is_empty() {
            gettext("Message")
        } else {
            email.subject.clone()
        };
        let view = MessageView::new(
            email,
            handlers,
            Box::new(|| {}),
            self.settings().boolean(keys::LOAD_REMOTE_IMAGES),
            &self.avatars(),
            None,
        );
        view.set_zoom(self.reader_zoom());
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(view.widget())
                .vexpand(true)
                .build(),
        ));
        let window = adw::Window::builder()
            .title(title)
            .default_width(820)
            .default_height(720)
            .content(&toolbar)
            .build();
        if let Some(app) = self.application() {
            window.set_application(Some(&app));
        }
        // Released with the window, so its body leaves the shared web process.
        window.connect_close_request(move |_| {
            view.release();
            glib::Propagation::Proceed
        });
        window.present();
    }
}

/// The zoom one step from `current` in `direction`, within the steps.
fn next_zoom(current: f64, direction: i32) -> f64 {
    match direction {
        0 => 1.0,
        d if d > 0 => ZOOM_STEPS
            .into_iter()
            .find(|step| *step > current + 0.001)
            .unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1]),
        _ => ZOOM_STEPS
            .into_iter()
            .rev()
            .find(|step| *step < current - 0.001)
            .unwrap_or(ZOOM_STEPS[0]),
    }
}

#[cfg(test)]
mod tests {
    use super::next_zoom;

    #[test]
    fn zoom_walks_the_steps_and_stops_at_the_ends() {
        assert_eq!(next_zoom(1.0, 1), 1.1);
        assert_eq!(next_zoom(1.0, -1), 0.9);
        assert_eq!(next_zoom(1.05, 1), 1.1, "off-step values snap to the next");
        assert_eq!(next_zoom(3.0, 1), 3.0);
        assert_eq!(next_zoom(0.5, -1), 0.5);
        assert_eq!(next_zoom(2.5, 0), 1.0);
    }
}

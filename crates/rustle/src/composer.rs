//! The composer: recipients, a contenteditable WebKit editor, attachments,
//! and sending through the Outbox so a crash mid-send never loses a message.

use crate::editor::{self, FORMAT_COMMANDS};
use crate::i18n::{self, gettext};
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::pango;
use rustle_core::compose;
use rustle_core::dates;
use rustle_core::db::Database;
use rustle_core::folders;
use rustle_core::models::{Account, Attachment, MessageHeader};
use rustle_core::net::errors::{classify, NetError};
use rustle_core::secrets;
use rustle_core::sync;
use rustle_core::threader::NO_SUBJECT;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use webkit::prelude::*;

/// The composer fields a new window starts with.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body_html: String,
}

/// A drop-down of known addresses under one recipient row. Gtk.EntryCompletion
/// only attaches to a Gtk.Entry, and an Adw.EntryRow is a list box row, so the
/// popover is driven by hand.
pub struct AddressSuggestions {
    row: adw::EntryRow,
    addresses: Rc<Vec<String>>,
    list: gtk::ListBox,
    popover: gtk::Popover,
}

impl AddressSuggestions {
    fn attach(row: &adw::EntryRow, addresses: Rc<Vec<String>>) -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Browse)
            .build();
        // autohide would steal focus from the entry the moment it pops up.
        let popover = gtk::Popover::builder()
            .child(&list)
            .autohide(false)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .build();
        popover.set_parent(row);
        let this = Rc::new(AddressSuggestions {
            row: row.clone(),
            addresses,
            list,
            popover,
        });

        let weak = Rc::downgrade(&this);
        row.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.on_changed();
            }
        });
        let popover = this.popover.clone();
        row.connect_destroy(move |_| popover.unparent());
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                this.pick(row);
            }
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, keyval, _, _| match weak.upgrade() {
            Some(this) => this.on_key_pressed(keyval),
            None => glib::Propagation::Proceed,
        });
        row.add_controller(keys);
        this
    }

    fn on_changed(&self) {
        let matches = compose::suggest_addresses(&self.row.text(), &self.addresses, 5);
        self.list.remove_all();
        for address in &matches {
            let label = gtk::Label::builder()
                .label(address.as_str())
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .max_width_chars(40)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(12)
                .margin_end(12)
                .build();
            self.list.append(&label);
        }
        if matches.is_empty() {
            self.popover.popdown();
        } else {
            self.list.select_row(self.list.row_at_index(0).as_ref());
            // Line the drop-down up with the row it belongs to.
            self.popover.set_size_request(self.row.width(), -1);
            self.popover.popup();
        }
    }

    fn pick(&self, row: &gtk::ListBoxRow) {
        let Some(label) = row.child().and_downcast::<gtk::Label>() else {
            return;
        };
        // The trailing ", " leaves nothing being typed, so the popover closes
        // itself on the resulting "changed" -- and shows how to add another.
        self.row.set_text(&compose::replace_last_address(
            &self.row.text(),
            &label.label(),
        ));
        self.row.set_position(-1);
    }

    fn on_key_pressed(&self, keyval: gdk::Key) -> glib::Propagation {
        if !self.popover.is_visible() {
            return glib::Propagation::Proceed;
        }
        match keyval {
            gdk::Key::Escape => self.popover.popdown(),
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab => match self.list.selected_row()
            {
                Some(row) => self.pick(&row),
                None => return glib::Propagation::Proceed,
            },
            gdk::Key::Down => self.move_selection(1),
            gdk::Key::Up => self.move_selection(-1),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn move_selection(&self, step: i32) {
        let index = self.list.selected_row().map(|row| row.index()).unwrap_or(0) + step;
        if let Some(row) = self.list.row_at_index(index) {
            self.list.select_row(Some(&row));
        }
    }
}

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/composer-window.ui")]
    pub struct ComposerWindow {
        #[template_child]
        pub cancel_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub send_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub send_spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub from_row: TemplateChild<adw::ComboRow>,
        #[template_child]
        pub to_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub cc_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub bcc_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub subject_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub body_container: TemplateChild<gtk::Box>,
        #[template_child]
        pub bold_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub italic_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub underline_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub strike_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub bullets_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub numbers_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub link_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub attach_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub attachments_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,

        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        pub accounts: RefCell<Vec<Account>>,
        pub account: RefCell<Option<Account>>,
        pub attachments: RefCell<Vec<(Attachment, adw::ActionRow)>>,
        pub body_html: RefCell<String>,
        pub is_syncing_buttons: Cell<bool>,
        pub webview: RefCell<Option<webkit::WebView>>,
        pub suggestions: RefCell<Vec<Rc<AddressSuggestions>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ComposerWindow {
        const NAME: &'static str = "RustleComposerWindow";
        type Type = super::ComposerWindow;
        type ParentType = adw::Window;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ComposerWindow {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("finished").build()])
        }
    }
    impl WidgetImpl for ComposerWindow {}
    impl WindowImpl for ComposerWindow {}
    impl AdwWindowImpl for ComposerWindow {}
}

glib::wrapper! {
    pub struct ComposerWindow(ObjectSubclass<imp::ComposerWindow>)
        @extends adw::Window, gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl ComposerWindow {
    pub fn new(
        app: Option<&gtk::Application>,
        db: Rc<RefCell<Database>>,
        account: &Account,
        draft: Draft,
    ) -> Self {
        let window: Self = glib::Object::builder().property("application", app).build();
        let imp = window.imp();
        imp.db.replace(Some(db.clone()));
        imp.body_html.replace(if draft.body_html.is_empty() {
            "<div><br></div>".to_string()
        } else {
            draft.body_html
        });

        window.build_from_row(account);
        imp.to_row.set_text(&draft.to);
        imp.cc_row.set_text(&draft.cc);
        imp.bcc_row.set_text(&draft.bcc);
        imp.subject_row.set_text(&draft.subject);
        window.build_editor();

        imp.cancel_button.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.on_cancel_clicked()
        ));
        imp.send_button.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.on_send_clicked()
        ));
        imp.attach_button.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.on_attach_clicked()
        ));
        imp.link_button.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.on_link_clicked()
        ));
        for (name, command) in FORMAT_COMMANDS {
            let button = window.format_button(name);
            button.connect_toggled(glib::clone!(
                #[weak]
                window,
                move |_| window.on_format_toggled(command)
            ));
        }
        for row in [&imp.to_row, &imp.cc_row, &imp.bcc_row, &imp.subject_row] {
            row.connect_changed(glib::clone!(
                #[weak]
                window,
                move |_| window.update_send_sensitivity()
            ));
        }
        window.update_send_sensitivity();

        let known = Rc::new(db.borrow().contact_addresses().unwrap_or_default());
        let suggestions = [&imp.to_row, &imp.cc_row, &imp.bcc_row]
            .into_iter()
            .map(|row| AddressSuggestions::attach(row, known.clone()))
            .collect();
        imp.suggestions.replace(suggestions);
        window
    }

    /// Build a composer from a mailto: URI. Shared by the main window and by
    /// a mailto: launch, which opens the composer with no main window at all.
    pub fn for_mailto(
        app: Option<&gtk::Application>,
        db: Rc<RefCell<Database>>,
        account: &Account,
        uri: &str,
    ) -> Self {
        let mailto = compose::parse_mailto(uri);
        let signature = account.signature_html();
        let body_html = if !mailto.body_html.is_empty() {
            mailto.body_html
        } else if !signature.is_empty() {
            compose::signature_block(&signature)
        } else {
            String::new()
        };
        Self::new(
            app,
            db,
            account,
            Draft {
                to: mailto.to,
                cc: mailto.cc,
                bcc: mailto.bcc,
                subject: mailto.subject,
                body_html,
            },
        )
    }

    pub fn connect_finished(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_local("finished", false, move |values| {
            let window = values[0].get::<Self>().expect("the emitter");
            callback(&window);
            None
        });
    }

    fn db(&self) -> Rc<RefCell<Database>> {
        self.imp().db.borrow().clone().expect("set at construction")
    }

    fn account(&self) -> Account {
        self.imp()
            .account
            .borrow()
            .clone()
            .expect("set at construction")
    }

    fn format_button(&self, name: &str) -> gtk::ToggleButton {
        let imp = self.imp();
        match name {
            "bold" => imp.bold_button.get(),
            "italic" => imp.italic_button.get(),
            "underline" => imp.underline_button.get(),
            "strike" => imp.strike_button.get(),
            "bullets" => imp.bullets_button.get(),
            _ => imp.numbers_button.get(),
        }
    }

    /// The account every send, draft and Sent copy belongs to. Picking
    /// another here is the only way to change it once the composer is open.
    fn build_from_row(&self, account: &Account) {
        let imp = self.imp();
        let accounts = self.db().borrow().accounts().unwrap_or_default();
        // A labelled account reads "Work <me@example.com>", so the address
        // that will go on the wire is never hidden.
        let names: Vec<String> = accounts
            .iter()
            .map(|each| {
                if each.name() == each.email {
                    each.email.clone()
                } else {
                    format!("{} <{}>", each.name(), each.email)
                }
            })
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        imp.from_row.set_model(Some(&gtk::StringList::new(&names)));
        let index = accounts
            .iter()
            .position(|each| each.id == account.id)
            .unwrap_or(0);
        imp.from_row.set_selected(index as u32);
        imp.account.replace(
            accounts
                .get(index)
                .cloned()
                .or_else(|| Some(account.clone())),
        );
        imp.accounts.replace(accounts);
        imp.from_row.connect_selected_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |row| {
                let imp = window.imp();
                let picked = imp.accounts.borrow().get(row.selected() as usize).cloned();
                if let Some(picked) = picked {
                    let signature = picked.signature_html();
                    imp.account.replace(Some(picked));
                    window.swap_signature(&signature);
                }
            }
        ));
    }

    /// Each account has its own signature, so picking another From replaces
    /// the signature block in the body: the existing one is swapped for the
    /// new account's, or removed when it has none. A body without one (the
    /// previous account had no signature) gets the block where the reply and
    /// forward bodies put it: ahead of the quote, or at the end.
    fn swap_signature(&self, signature: &str) {
        let Some(webview) = self.imp().webview.borrow().clone() else {
            return;
        };
        let block = if signature.is_empty() {
            String::new()
        } else {
            compose::signature_block(signature)
        };
        let script = format!(
            "(function () {{
                var block = {};
                var old = document.querySelector('.signature');
                if (old) {{
                    old.insertAdjacentHTML('beforebegin', block);
                    old.remove();
                }} else if (block) {{
                    var quote = document.querySelector('blockquote');
                    var anchor = quote && (quote.previousElementSibling || quote);
                    if (anchor) anchor.insertAdjacentHTML('beforebegin', block);
                    else document.body.insertAdjacentHTML('beforeend', block);
                }} else return;
                post();
            }})()",
            serde_json::to_string(&block).unwrap_or_default()
        );
        webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
    }

    // --- editor -----------------------------------------------------------

    fn build_editor(&self) {
        let imp = self.imp();
        let webview = editor::build_webview(
            &imp.body_html.borrow(),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |json| window.on_editor_changed(json)
            ),
        );
        imp.body_container.append(&webview);
        imp.webview.replace(Some(webview));
    }

    fn on_editor_changed(&self, json: &str) {
        let Ok(payload) = serde_json::from_str::<editor::EditorPayload>(json) else {
            return;
        };
        let imp = self.imp();
        imp.body_html.replace(payload.html);
        imp.is_syncing_buttons.set(true);
        for (name, command) in FORMAT_COMMANDS {
            self.format_button(name)
                .set_active(payload.states.get(command).copied().unwrap_or(false));
        }
        imp.is_syncing_buttons.set(false);
        self.update_send_sensitivity();
    }

    fn exec(&self, command: &str, argument: Option<&str>) {
        if let Some(webview) = self.imp().webview.borrow().as_ref() {
            editor::exec(webview, command, argument);
        }
    }

    fn on_format_toggled(&self, command: &str) {
        if self.imp().is_syncing_buttons.get() {
            return;
        }
        self.exec(command, None);
        if let Some(webview) = self.imp().webview.borrow().as_ref() {
            webview.grab_focus();
        }
    }

    fn on_link_clicked(&self) {
        let entry = gtk::Entry::builder()
            .placeholder_text("https://")
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Insert Link"))
            .extra_child(&entry)
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("insert", &gettext("Insert"));
        dialog.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("insert"));
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    let url = entry.text().trim().to_string();
                    if response == "insert" && !url.is_empty() {
                        window.exec("createLink", Some(&url));
                    }
                    if let Some(webview) = window.imp().webview.borrow().as_ref() {
                        webview.grab_focus();
                    }
                }
            ),
        );
        dialog.present(Some(self));
    }

    fn update_send_sensitivity(&self) {
        let imp = self.imp();
        let has_recipient = !self.to_addrs().is_empty()
            || !self.cc_addrs().is_empty()
            || !self.bcc_addrs().is_empty();
        imp.send_button
            .set_sensitive(has_recipient && !imp.subject_row.text().trim().is_empty());
    }

    fn preview_text(&self) -> String {
        rustle_core::html::html_to_text(&self.imp().body_html.borrow())
    }

    fn to_addrs(&self) -> Vec<String> {
        compose::split_addresses(&self.imp().to_row.text())
    }

    fn cc_addrs(&self) -> Vec<String> {
        compose::split_addresses(&self.imp().cc_row.text())
    }

    fn bcc_addrs(&self) -> Vec<String> {
        compose::split_addresses(&self.imp().bcc_row.text())
    }

    /// A human-readable stand-in for the "sender" column of the Outbox/Drafts
    /// list, which otherwise has no concept of outgoing recipients.
    fn recipients_display(&self) -> String {
        let imp = self.imp();
        [imp.to_row.text(), imp.cc_row.text(), imp.bcc_row.text()]
            .iter()
            .map(|text| text.trim().to_string())
            .find(|text| !text.is_empty())
            .unwrap_or_else(|| gettext("(no recipient)"))
    }

    fn has_content(&self) -> bool {
        let imp = self.imp();
        [
            imp.to_row.text(),
            imp.cc_row.text(),
            imp.bcc_row.text(),
            imp.subject_row.text(),
        ]
        .iter()
        .any(|text| !text.trim().is_empty())
            || !self.preview_text().is_empty()
    }

    fn local_header(&self, sender: &str, subject: &str) -> MessageHeader {
        let (recipient, recipient_address) = compose::first_recipient(&self.imp().to_row.text());
        MessageHeader {
            sender: sender.to_string(),
            sender_address: String::new(),
            recipient,
            recipient_address,
            subject: subject.to_string(),
            preview: self.preview_text().chars().take(100).collect(),
            date: dates::now_iso(),
            is_unread: false,
            ..MessageHeader::default()
        }
    }

    // --- attachments ------------------------------------------------------

    fn on_attach_clicked(&self) {
        let dialog = gtk::FileDialog::new();
        dialog.open(
            Some(self),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result| {
                    let Ok(file) = result else { return }; // cancelled
                    let Ok((content, _)) = file.load_contents(gio::Cancellable::NONE) else {
                        return;
                    };
                    let filename = file
                        .basename()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attachment".into());
                    let mime_type = mime_guess::from_path(&filename)
                        .first_or_octet_stream()
                        .to_string();
                    window.add_attachment(Attachment {
                        filename,
                        mime_type,
                        content: content.to_vec(),
                    });
                }
            ),
        );
    }

    fn add_attachment(&self, attachment: Attachment) {
        let imp = self.imp();
        let row = adw::ActionRow::builder()
            .title(&attachment.filename)
            .build();
        let remove_button = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(gettext("Remove Attachment"))
            .css_classes(["flat"])
            .build();
        remove_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            #[weak]
            row,
            move |_| {
                let imp = window.imp();
                imp.attachments
                    .borrow_mut()
                    .retain(|(_, each)| *each != row);
                imp.attachments_list.remove(&row);
            }
        ));
        row.add_suffix(&remove_button);
        imp.attachments_list.append(&row);
        imp.attachments.borrow_mut().push((attachment, row));
    }

    fn attachments(&self) -> Vec<Attachment> {
        self.imp()
            .attachments
            .borrow()
            .iter()
            .map(|(attachment, _)| attachment.clone())
            .collect()
    }

    // --- cancel / save draft ----------------------------------------------

    fn on_cancel_clicked(&self) {
        if self.has_content() {
            let account = self.account();
            let subject = self.imp().subject_row.text().trim().to_string();
            let raw = compose::build_mime_message(
                &account.email,
                &self.to_addrs(),
                &self.cc_addrs(),
                &subject,
                &self.imp().body_html.borrow(),
                &self.attachments(),
            );
            let db = self.db();
            let db = db.borrow();
            let saved = db
                .get_or_create_folder(
                    account.id,
                    folders::DRAFTS_FOLDER,
                    folders::icon_for_folder(folders::DRAFTS_FOLDER),
                )
                .and_then(|folder| {
                    let subject = if subject.is_empty() {
                        NO_SUBJECT.to_string()
                    } else {
                        subject
                    };
                    let row = db.save_email(
                        folder.id,
                        &self.local_header(&self.recipients_display(), &subject),
                    )?;
                    if let Ok(raw) = &raw {
                        db.save_raw_message(row.id, raw)?;
                    }
                    Ok(())
                });
            if let Err(error) = saved {
                log::error!("could not save the draft for {}: {error}", account.email);
            }
            self.emit_by_name::<()>("finished", &[]);
        }
        self.close();
    }

    // --- send -------------------------------------------------------------

    fn on_send_clicked(&self) {
        let account = self.account();
        let to_addrs = self.to_addrs();
        let cc_addrs = self.cc_addrs();
        let bcc_addrs = self.bcc_addrs();
        let subject = self.imp().subject_row.text().trim().to_string();

        // Bcc is never written to a received message, so this is the only
        // place a Bcc'd address can be learned.
        let contacts: Vec<(String, String)> = to_addrs
            .iter()
            .chain(&cc_addrs)
            .chain(&bcc_addrs)
            .flat_map(|text| rustle_core::address::parse_list(text))
            .map(|mailbox| (mailbox.name, mailbox.address))
            .collect();
        if let Err(error) = self.db().borrow_mut().save_contacts(&contacts) {
            log::warn!("could not remember the recipients: {error}");
        }

        let raw = match compose::build_mime_message(
            &account.email,
            &to_addrs,
            &cc_addrs,
            &subject,
            &self.imp().body_html.borrow(),
            &self.attachments(),
        ) {
            Ok(raw) => raw,
            Err(error) => {
                self.imp()
                    .toast_overlay
                    .add_toast(adw::Toast::new(&i18n::format(
                        &gettext("Couldn't send: {msg}"),
                        &[("msg", &error.to_string())],
                    )));
                return;
            }
        };

        // The SMTP envelope recipients, unlike the message's own To/Cc
        // headers, also carry Bcc addresses.
        let recipients: Vec<String> = to_addrs
            .iter()
            .chain(&cc_addrs)
            .chain(&bcc_addrs)
            .map(|text| rustle_core::address::first_address(text))
            .filter(|address| !address.is_empty())
            .collect();

        // Save to Outbox before attempting to send -- a crash mid-send can
        // then never lose the message.
        let email_id = {
            let db = self.db();
            let db = db.borrow();
            let saved = db
                .get_or_create_folder(
                    account.id,
                    folders::OUTBOX_FOLDER,
                    folders::icon_for_folder(folders::OUTBOX_FOLDER),
                )
                .and_then(|outbox| {
                    let row = db.save_email(
                        outbox.id,
                        &self.local_header(&self.recipients_display(), &subject),
                    )?;
                    db.save_raw_message(row.id, &raw)?;
                    Ok(row.id)
                });
            match saved {
                Ok(id) => id,
                Err(error) => {
                    log::error!("could not queue the message in the Outbox: {error}");
                    return;
                }
            }
        };

        self.set_sending(true);
        let job_account = account.clone();
        let job_raw = raw.clone();
        let job_recipients = recipients.clone();
        let job_subject = subject.clone();
        workers::run(
            move || send_job(&job_account, &job_subject, &job_recipients, &job_raw),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result: Result<(), String>| match result {
                    Ok(()) => window.on_send_done(&account, email_id, &subject, &raw),
                    Err(message) => window.on_send_failed(&message),
                }
            ),
        );
    }

    fn on_send_done(&self, account: &Account, email_id: i64, subject: &str, raw: &[u8]) {
        let db = self.db();
        let db = db.borrow();
        let filed = db.delete_email(email_id).and_then(|_| {
            let sent = db.sent_folder(account.id)?;
            let mut header = self.local_header(&account.email, subject);
            header.sender_address = account.email.clone();
            header.preview = subject.to_string();
            let row = db.save_email(sent.id, &header)?;
            db.save_raw_message(row.id, raw)
        });
        if let Err(error) = filed {
            log::error!(
                "could not file the sent copy for {}: {error}",
                account.email
            );
        }
        drop(db);
        self.emit_by_name::<()>("finished", &[]);
        self.close();
    }

    fn on_send_failed(&self, message: &str) {
        self.set_sending(false);
        self.imp()
            .toast_overlay
            .add_toast(adw::Toast::new(&i18n::format(
                &gettext("Couldn't send: {msg}. Saved to Outbox."),
                &[("msg", message)],
            )));
        self.emit_by_name::<()>("finished", &[]);
        self.close();
    }

    fn set_sending(&self, is_sending: bool) {
        let imp = self.imp();
        imp.send_button.set_sensitive(!is_sending);
        imp.from_row.set_sensitive(!is_sending);
        imp.cancel_button.set_sensitive(!is_sending);
        imp.send_spinner.set_visible(is_sending);
        imp.send_spinner.set_spinning(is_sending);
    }
}

/// Runs on the worker thread: network only, no widgets, no database.
fn send_job(
    account: &Account,
    subject: &str,
    recipients: &[String],
    raw: &[u8],
) -> Result<(), String> {
    let Some(credential) = secrets::credential_for(account) else {
        log::warn!("could not sign in to account {}", account.email);
        return Err(gettext("Could not sign in to this account."));
    };
    sync::send_message(account, &credential, &account.email, recipients, raw).map_err(
        |error: NetError| {
            log::error!(
                "could not send {subject:?} to {} via {} (account {}): {error}",
                recipients.join(", "),
                account.smtp_host,
                account.email
            );
            i18n::failure_message(&classify(&error, &account.smtp_host))
        },
    )
}

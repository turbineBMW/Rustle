//! The composer: recipients, a contenteditable WebKit editor, attachments,
//! and sending through the Outbox so a crash mid-send never loses a message.
//! Closing it with something written asks whether to keep it as a draft in
//! the account's Drafts mailbox.

use crate::application::RustleApplication;
use crate::editor::{self, ColorKind, LinkInfo, FORMAT_COMMANDS};
use crate::i18n::{self, gettext};
use crate::settings as keys;
use crate::widgets::color_menu::{ColorMenu, HIGHLIGHT_PALETTE, TEXT_PALETTE};
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::pango;
use rustle_core::assistant::{self, Harness, RewriteStyle, Suggestion};
use rustle_core::compose;
use rustle_core::dates;
use rustle_core::db::{Database, OutboxEntry};
use rustle_core::folders;
use rustle_core::models::NO_SUBJECT;
use rustle_core::models::{Account, Attachment, MessageHeader};
use rustle_core::net::errors::{classify, Failure, NetError};
use rustle_core::outbox;
use rustle_core::secrets;
use rustle_core::sync;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use webkit::prelude::*;

/// A host going away once "Save as Draft?" is answered: true when the
/// composer has gone, false when it stays.
type LeavingHost = Box<dyn FnOnce(bool)>;

/// The composer fields a new composer starts with.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body_html: String,
    pub attachments: Vec<Attachment>,
    /// Set when the composer is finishing a saved draft.
    pub resumed: Option<ResumedDraft>,
    /// For a reply: the original's attachments, offered if a recipient is
    /// added who wasn't on it, and the addresses that were (lowercase).
    pub original_attachments: Vec<Attachment>,
    pub original_people: Vec<String>,
    /// A reply's In-Reply-To and References (`compose::reply_threading`),
    /// kept through drafts and the Outbox; "" for anything else.
    pub in_reply_to: String,
    pub references: String,
}

/// A saved draft being finished: the row it was opened from, and the
/// Message-ID its server copies carry, by which they are replaced or removed.
#[derive(Clone, Debug)]
pub struct ResumedDraft {
    pub email_id: i64,
    pub account_id: i64,
    pub message_id: String,
}

/// What the fields hold, to tell whether anything was written since the
/// composer opened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    fields: [String; 4],
    body: String,
    attachments: usize,
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
        // Capture phase: the entry's own Return binding ("activate") would
        // otherwise swallow the key before this handler ever saw it.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, keyval, _, _| match weak.upgrade() {
            Some(this) => this.on_key_pressed(keyval),
            None => glib::Propagation::Proceed,
        });
        row.add_controller(keys);
        // A click or a Shift+Tab into another field leaves nothing to
        // complete, so the drop-down goes with the focus.
        let focus = gtk::EventControllerFocus::new();
        let popover = this.popover.clone();
        focus.connect_leave(move |_| popover.popdown());
        row.add_controller(focus);
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

    /// Enter takes the highlighted address and stays put, ready for the next
    /// one. Tab is only ever a move to the next field: the highlight is a
    /// suggestion, not a choice, so what was typed stays as typed.
    fn on_key_pressed(&self, keyval: gdk::Key) -> glib::Propagation {
        if !self.popover.is_visible() {
            return glib::Propagation::Proceed;
        }
        match keyval {
            gdk::Key::Escape => self.popover.popdown(),
            gdk::Key::Return | gdk::Key::KP_Enter => match self.list.selected_row() {
                Some(row) => self.pick(&row),
                None => return glib::Propagation::Proceed,
            },
            gdk::Key::Tab | gdk::Key::KP_Tab => {
                self.popover.popdown();
                return glib::Propagation::Proceed;
            }
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

/// Which face of the From drop-down a factory builds.
#[derive(Clone, Copy)]
enum FromItem {
    /// The pill in the header bar.
    Button,
    /// One account in the popped-up list.
    Row,
}

mod tools;
pub use tools::QueuedSend;

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/composer.ui")]
    pub struct Composer {
        #[template_child]
        pub header: TemplateChild<adw::HeaderBar>,
        #[template_child]
        pub pop_out_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub assist_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub review_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub rewrite_style: TemplateChild<gtk::DropDown>,
        #[template_child]
        pub rewrite_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub assist_instructions: TemplateChild<gtk::Entry>,
        #[template_child]
        pub assist_spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub assist_status: TemplateChild<gtk::Label>,
        #[template_child]
        pub assist_results: TemplateChild<gtk::Box>,
        #[template_child]
        pub cancel_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub send_button: TemplateChild<adw::SplitButton>,
        #[template_child]
        pub more_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub originals_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub format_bar: TemplateChild<gtk::Box>,
        #[template_child]
        pub send_spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub from_dropdown: TemplateChild<gtk::DropDown>,
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
        pub text_color_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub highlight_button: TemplateChild<gtk::MenuButton>,
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
        pub image_menu: RefCell<Option<(gtk::PopoverMenu, gio::Menu)>>,
        pub image_size_action: RefCell<Option<gio::SimpleAction>>,
        pub selected_image: Cell<Option<usize>>,
        pub text_color_menu: RefCell<Option<Rc<ColorMenu>>>,
        pub highlight_menu: RefCell<Option<Rc<ColorMenu>>>,
        /// What the last editor payload said: the selected text and the
        /// link the caret sits in, which the link button edits in place.
        pub selection: RefCell<String>,
        pub current_link: RefCell<Option<LinkInfo>>,
        /// The link last clicked in the editor, the target of the link menu.
        pub clicked_link: RefCell<Option<LinkInfo>>,
        pub link_menu: RefCell<Option<(gtk::PopoverMenu, gio::Menu)>>,
        pub settings: RefCell<Option<gio::Settings>>,
        /// Bumped by every request to the assistant, so a late answer to an
        /// older one is dropped.
        pub assist_generation: Cell<u64>,
        /// The Message-ID every save of this draft carries, so each save
        /// replaces the copy the last one left.
        pub message_id: RefCell<String>,
        /// What threads a reply under the message it answers.
        pub in_reply_to: RefCell<String>,
        pub references: RefCell<String>,
        pub resumed: RefCell<Option<ResumedDraft>>,
        /// What the fields held when it opened: closing unchanged asks nothing.
        pub(super) opened_with: RefCell<Snapshot>,
        /// Set once it is closing for good, so its window closes unasked.
        pub is_done: Cell<bool>,
        /// "Save as Draft?" is up.
        pub is_asking: Cell<bool>,
        /// Hosts going away once it is answered (see `ask_before_leaving`).
        pub waiting_hosts: RefCell<Vec<LeavingHost>>,
        /// Send text/plain alone.
        pub plain_text: Cell<bool>,
        /// Set by a host that can hold a sent message back for Undo or a
        /// later time; without one, Send sends at once.
        pub queue_handler: RefCell<Option<tools::QueueHandler>>,
        /// The time Send Later picked, for the send it starts.
        pub scheduled_for: Cell<Option<chrono::DateTime<chrono::Local>>>,
        /// A reply's original attachments, and everyone it already went to.
        pub originals: RefCell<Vec<Attachment>>,
        pub original_people: RefCell<std::collections::HashSet<String>>,
        pub tool_actions: RefCell<Option<gio::SimpleActionGroup>>,
        /// OpenPGP for the send, from the More menu.
        pub pgp_sign: Cell<bool>,
        pub pgp_encrypt: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Composer {
        const NAME: &'static str = "RustleComposer";
        type Type = super::Composer;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Composer {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            // finished: the message was sent, queued or saved as a draft, so
            // the folders changed. closed: the host should take it away.
            // pop-out: the inline composer asks for a window of its own.
            // notice: something for the host to tell the user once the
            // composer has gone, such as a draft that stayed on this device.
            SIGNALS.get_or_init(|| {
                let mut signals: Vec<_> = ["finished", "closed", "pop-out"]
                    .into_iter()
                    .map(|name| glib::subclass::Signal::builder(name).build())
                    .collect();
                signals.push(
                    glib::subclass::Signal::builder("notice")
                        .param_types([String::static_type()])
                        .build(),
                );
                signals
            })
        }

        fn dispose(&self) {
            // The picture menu is parented on the editor by hand, so it has
            // to be taken off by hand too.
            if let Some((popover, _)) = self.image_menu.take() {
                popover.unparent();
            }
            if let Some((popover, _)) = self.link_menu.take() {
                popover.unparent();
            }
        }
    }
    impl WidgetImpl for Composer {}
    impl BinImpl for Composer {}
}

glib::wrapper! {
    pub struct Composer(ObjectSubclass<imp::Composer>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

/// Give `composer` a window of its own, which closes with it. Used for a
/// mailto: launch with no main window, on a phone, and to pop an inline
/// composer out of the reader pane.
pub fn present_in_window(app: Option<&gtk::Application>, composer: &Composer) -> adw::Window {
    composer.set_inline(false);
    let window = adw::Window::builder()
        .title(gettext("New Message"))
        .default_width(540)
        .default_height(740)
        .content(composer)
        .build();
    window.set_application(app);
    composer.connect_closed(glib::clone!(
        #[weak]
        window,
        move |_| window.close()
    ));
    // The window's own close (a key, the compositor) asks like Cancel does.
    window.connect_close_request(glib::clone!(
        #[weak]
        composer,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_| {
            if composer.may_close() {
                glib::Propagation::Proceed
            } else {
                glib::Propagation::Stop
            }
        }
    ));
    window.present();
    composer.focus_first_field();
    window
}

impl Composer {
    pub fn new(db: Rc<RefCell<Database>>, account: &Account, draft: Draft) -> Self {
        let window: Self = glib::Object::new();
        let imp = window.imp();
        imp.db.replace(Some(db.clone()));
        imp.body_html.replace(if draft.body_html.is_empty() {
            "<div><br></div>".to_string()
        } else {
            draft.body_html
        });

        window.build_from_dropdown(account);
        imp.to_row.set_text(&draft.to);
        imp.cc_row.set_text(&draft.cc);
        imp.bcc_row.set_text(&draft.bcc);
        imp.subject_row.set_text(&draft.subject);
        window.build_editor();
        for attachment in draft.attachments {
            window.add_attachment(attachment);
        }
        let message_id = draft
            .resumed
            .as_ref()
            .map(|resumed| resumed.message_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| compose::new_message_id(&account.email));
        imp.message_id.replace(message_id);
        imp.in_reply_to.replace(draft.in_reply_to);
        imp.references.replace(draft.references);
        imp.resumed.replace(draft.resumed);
        imp.originals.replace(draft.original_attachments);
        imp.original_people.replace(
            draft
                .original_people
                .iter()
                .map(|address| address.to_lowercase())
                .collect(),
        );

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
        imp.pop_out_button.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.emit_by_name::<()>("pop-out", &[])
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

        window.setup_assistant();
        window.setup_tools();

        let ranked = db.borrow().ranked_contacts().unwrap_or_default();
        let book = rustle_core::address_book::eds_contacts();
        let known = Rc::new(compose::merge_suggestions(&ranked, &book));
        let suggestions = [&imp.to_row, &imp.cc_row, &imp.bcc_row]
            .into_iter()
            .map(|row| AddressSuggestions::attach(row, known.clone()))
            .collect();
        imp.suggestions.replace(suggestions);
        imp.opened_with.replace(window.snapshot());
        window
    }

    /// Build a composer from a mailto: URI. Shared by the main window and by
    /// a mailto: launch, which opens the composer with no main window at all.
    pub fn for_mailto(db: Rc<RefCell<Database>>, account: &Account, uri: &str) -> Self {
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
            db,
            account,
            Draft {
                to: mailto.to,
                cc: mailto.cc,
                bcc: mailto.bcc,
                subject: mailto.subject,
                body_html,
                ..Draft::default()
            },
        )
    }

    pub fn connect_finished(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_signal("finished", callback);
    }

    pub fn connect_closed(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_signal("closed", callback);
    }

    pub fn connect_pop_out(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_signal("pop-out", callback);
    }

    pub fn connect_notice(&self, callback: impl Fn(&str) + 'static) {
        self.connect_local("notice", false, move |values| {
            let text = values[1].get::<String>().expect("the notice text");
            callback(&text);
            None
        });
    }

    fn connect_signal(&self, name: &str, callback: impl Fn(&Self) + 'static) {
        self.connect_local(name, false, move |values| {
            let composer = values[0].get::<Self>().expect("the emitter");
            callback(&composer);
            None
        });
    }

    /// Inline, in the reader pane, the header bar carries the window's
    /// buttons and the pop-out button; in a window of its own, Cancel
    /// stands in for the close button.
    pub fn set_inline(&self, is_inline: bool) {
        let imp = self.imp();
        imp.pop_out_button.set_visible(is_inline);
        imp.header.set_show_start_title_buttons(is_inline);
        imp.header.set_show_end_title_buttons(is_inline);
    }

    /// The first field there is to fill: To on a new message, the body on
    /// a reply, whose recipients are already there.
    pub fn focus_first_field(&self) {
        let imp = self.imp();
        if imp.to_row.text().trim().is_empty() {
            imp.to_row.grab_focus();
        } else if let Some(webview) = imp.webview.borrow().as_ref() {
            webview.grab_focus();
        }
    }

    /// Whether closing it now would lose anything.
    pub fn is_blank(&self) -> bool {
        !self.has_content()
    }

    /// Its window is being closed: true when nothing would be lost. Otherwise
    /// it asks what to do with what was written, and closes itself after.
    pub fn may_close(&self) -> bool {
        if !self.has_unsaved_changes() {
            return true;
        }
        // Mid-send, the send closes it when it is through.
        if self.imp().cancel_button.is_sensitive() {
            self.ask_to_save();
        }
        false
    }

    /// Whether going now would lose something written in it.
    pub fn has_unsaved_changes(&self) -> bool {
        !self.imp().is_done.get() && self.is_changed()
    }

    /// `may_close` for a host going away with it: the main window it is
    /// inline in closing, or the app quitting. `then` hears true once
    /// nothing would be lost -- at once, or after Save Draft or Delete --
    /// and false when the user keeps editing, or a send is under way.
    pub fn ask_before_leaving(&self, then: impl FnOnce(bool) + 'static) {
        if !self.has_unsaved_changes() {
            then(true);
            return;
        }
        if !self.imp().cancel_button.is_sensitive() {
            then(false);
            return;
        }
        self.imp().waiting_hosts.borrow_mut().push(Box::new(then));
        self.ask_to_save();
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

    /// The account every send, draft and Sent copy belongs to, as a pill in
    /// the header bar wearing that account's colour. Picking another here is
    /// the only way to change it once the composer is open.
    fn build_from_dropdown(&self, account: &Account) {
        let imp = self.imp();
        let accounts = self.db().borrow().accounts().unwrap_or_default();
        // A mailto: launch has no main window to have loaded the colours.
        crate::account_colors::apply(&accounts);
        // The model only carries positions; the factories look the account
        // up by index, so no GObject wrapper is needed.
        let ids: Vec<String> = accounts.iter().map(|each| each.id.to_string()).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let index = accounts
            .iter()
            .position(|each| each.id == account.id)
            .unwrap_or(0);
        // Before the model: the button binds its item as soon as there is one.
        imp.accounts.replace(accounts);
        imp.from_dropdown
            .set_model(Some(&gtk::StringList::new(&ids)));
        imp.from_dropdown
            .set_factory(Some(&self.account_factory(FromItem::Button)));
        imp.from_dropdown
            .set_list_factory(Some(&self.account_factory(FromItem::Row)));
        imp.from_dropdown.connect_selected_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |dropdown| {
                let imp = window.imp();
                let picked = imp
                    .accounts
                    .borrow()
                    .get(dropdown.selected() as usize)
                    .cloned();
                if let Some(picked) = picked {
                    let signature = picked.signature_html();
                    window.show_from(&picked);
                    imp.account.replace(Some(picked));
                    window.swap_signature(&signature);
                }
            }
        ));
        imp.from_dropdown.set_selected(index as u32);
        // set_selected on an already-selected 0 notifies nobody.
        let picked = imp.accounts.borrow().get(index).cloned();
        let picked = picked.unwrap_or_else(|| account.clone());
        self.show_from(&picked);
        imp.account.replace(Some(picked));
    }

    /// Colour the pill for this account and name the address that will go
    /// on the wire, which the short label on the pill does not.
    fn show_from(&self, account: &Account) {
        let dropdown = &self.imp().from_dropdown;
        crate::account_colors::tag(&**dropdown, Some(account.id));
        dropdown.set_tooltip_text(Some(&i18n::format(
            &gettext("Send from {email}"),
            &[("email", &account.email)],
        )));
    }

    /// The pill shows the account's short name; each row in the list shows
    /// its colour dot, the same short name and, unless that is the address, the
    /// address underneath.
    fn account_factory(&self, kind: FromItem) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let child: gtk::Widget = match kind {
                FromItem::Button => gtk::Label::builder()
                    .ellipsize(pango::EllipsizeMode::End)
                    .max_width_chars(20)
                    .build()
                    .upcast(),
                FromItem::Row => {
                    let dot = gtk::Box::builder()
                        .width_request(10)
                        .height_request(10)
                        .valign(gtk::Align::Center)
                        .css_classes(["account-dot"])
                        .build();
                    let name = gtk::Label::builder().xalign(0.0).build();
                    let email = gtk::Label::builder()
                        .xalign(0.0)
                        .css_classes(["caption", "dim-label"])
                        .build();
                    let text = gtk::Box::builder()
                        .orientation(gtk::Orientation::Vertical)
                        .build();
                    text.append(&name);
                    text.append(&email);
                    let row = gtk::Box::builder().spacing(10).build();
                    row.append(&dot);
                    row.append(&text);
                    row.upcast()
                }
            };
            item.set_child(Some(&child));
        });
        factory.connect_bind(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let account = window
                    .imp()
                    .accounts
                    .borrow()
                    .get(item.position() as usize)
                    .cloned();
                let (Some(account), Some(child)) = (account, item.child()) else {
                    return;
                };
                match kind {
                    FromItem::Button => {
                        if let Some(label) = child.downcast_ref::<gtk::Label>() {
                            label.set_label(account.short_label());
                        }
                    }
                    FromItem::Row => {
                        let dot = child.first_child();
                        let text = dot.as_ref().and_then(|dot| dot.next_sibling());
                        let name = text.as_ref().and_then(|text| text.first_child());
                        let email = name.as_ref().and_then(|name| name.next_sibling());
                        if let Some(dot) = dot {
                            crate::account_colors::tag(&dot, Some(account.id));
                        }
                        if let Some(name) = name.and_downcast::<gtk::Label>() {
                            name.set_label(account.short_label());
                        }
                        if let Some(email) = email.and_downcast::<gtk::Label>() {
                            email.set_label(&account.email);
                            email.set_visible(account.short_label() != account.email);
                        }
                    }
                }
            }
        ));
        factory
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
                window.rustlePost();
            }})()",
            serde_json::to_string(&block).unwrap_or_default()
        );
        webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
    }

    // --- the assistant ----------------------------------------------------

    fn setup_assistant(&self) {
        let imp = self.imp();
        let settings = crate::settings::load();
        settings.connect_changed(
            Some(keys::ASSISTANT),
            glib::clone!(
                #[weak(rename_to = composer)]
                self,
                move |_, _| composer.update_assist_button()
            ),
        );
        imp.settings.replace(Some(settings));
        self.update_assist_button();

        let styles: Vec<String> = RewriteStyle::ALL
            .iter()
            .map(|style| style_label(*style))
            .collect();
        let styles: Vec<&str> = styles.iter().map(String::as_str).collect();
        imp.rewrite_style
            .set_model(Some(&gtk::StringList::new(&styles)));
        imp.review_button.connect_clicked(glib::clone!(
            #[weak(rename_to = composer)]
            self,
            move |_| composer.ask_assistant(None)
        ));
        imp.rewrite_button.connect_clicked(glib::clone!(
            #[weak(rename_to = composer)]
            self,
            move |_| {
                let index = composer.imp().rewrite_style.selected() as usize;
                composer.ask_assistant(RewriteStyle::ALL.get(index).copied());
            }
        ));
    }

    fn harness(&self) -> Option<Harness> {
        let settings = self.imp().settings.borrow();
        Harness::parse(&settings.as_ref()?.string(keys::ASSISTANT))
    }

    /// The panel's button is there only while a tool is picked.
    fn update_assist_button(&self) {
        let button = &self.imp().assist_button;
        let harness = self.harness();
        button.set_visible(harness.is_some());
        match harness {
            Some(harness) => button.set_tooltip_text(Some(&i18n::format(
                &gettext("Review or rewrite with {tool}"),
                &[("tool", harness.label())],
            ))),
            None => button.set_active(false),
        }
    }

    /// Review the draft (`style` None) or rewrite it, with what the user
    /// wrote and nothing else: not the quote, the signature or the subject.
    fn ask_assistant(&self, style: Option<RewriteStyle>) {
        let Some(webview) = self.imp().webview.borrow().clone() else {
            return;
        };
        editor::own_html(
            &webview,
            glib::clone!(
                #[weak(rename_to = composer)]
                self,
                move |html| composer.send_to_assistant(style, &html)
            ),
        );
    }

    fn send_to_assistant(&self, style: Option<RewriteStyle>, html: &str) {
        let Some(harness) = self.harness() else {
            return;
        };
        let imp = self.imp();
        clear_box(&imp.assist_results);
        let draft = rustle_core::html::html_to_text(html).trim().to_string();
        if draft.is_empty() {
            imp.assist_status
                .set_label(&gettext("Write something first."));
            return;
        }
        let generation = imp.assist_generation.get() + 1;
        imp.assist_generation.set(generation);
        self.set_assist_busy(true);
        imp.assist_status.set_label(&i18n::format(
            &gettext("Asking {tool}…"),
            &[("tool", harness.label())],
        ));
        let instructions = imp.assist_instructions.text().to_string();
        let model = imp
            .settings
            .borrow()
            .as_ref()
            .map(|settings| settings.string(keys::ASSISTANT_MODEL).to_string())
            .unwrap_or_default();
        workers::run(
            move || -> Result<Answer, String> {
                match style {
                    None => {
                        let prompt = assistant::review_prompt(&draft, &instructions);
                        let answer = assistant::ask(harness, &model, &prompt)?;
                        assistant::parse_review(&answer, &draft)
                            .map(Answer::Review)
                            .ok_or_else(|| unusable(&answer))
                    }
                    Some(style) => {
                        let prompt = assistant::rewrite_prompt(&draft, style, &instructions);
                        let answer = assistant::ask(harness, &model, &prompt)?;
                        assistant::parse_rewrite(&answer)
                            .map(Answer::Rewrite)
                            .ok_or_else(|| unusable(&answer))
                    }
                }
            },
            glib::clone!(
                #[weak(rename_to = composer)]
                self,
                move |result: Result<Answer, String>| {
                    let imp = composer.imp();
                    if imp.assist_generation.get() != generation {
                        return;
                    }
                    composer.set_assist_busy(false);
                    match result {
                        Ok(Answer::Review(suggestions)) => composer.show_review(suggestions),
                        Ok(Answer::Rewrite(text)) => composer.show_rewrite(&text),
                        Err(message) => {
                            log::warn!("draft review with {} failed: {message}", harness.id());
                            imp.assist_status.set_label(&i18n::format(
                                &gettext("Couldn't ask {tool}: {msg}"),
                                &[("tool", harness.label()), ("msg", &message)],
                            ));
                        }
                    }
                }
            ),
        );
    }

    fn set_assist_busy(&self, is_busy: bool) {
        let imp = self.imp();
        imp.assist_spinner.set_visible(is_busy);
        imp.assist_spinner.set_spinning(is_busy);
        imp.review_button.set_sensitive(!is_busy);
        imp.rewrite_button.set_sensitive(!is_busy);
    }

    /// One card per suggestion: why, what changes, and Apply.
    fn show_review(&self, suggestions: Vec<Suggestion>) {
        let imp = self.imp();
        imp.assist_status.set_label(&if suggestions.is_empty() {
            gettext("Looks good: nothing to change.")
        } else {
            i18n::plural(
                "{n} suggestion",
                "{n} suggestions",
                suggestions.len() as u64,
                &[],
            )
        });
        for suggestion in suggestions {
            let reason = suggestion.reason.trim().to_string();
            let change = gtk::Label::builder()
                .use_markup(true)
                .label(format!(
                    "<s>{}</s>  →  <b>{}</b>",
                    glib::markup_escape_text(&suggestion.original),
                    glib::markup_escape_text(&suggestion.replacement)
                ))
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build();
            let apply = gtk::Button::builder()
                .label(gettext("Apply"))
                .halign(gtk::Align::End)
                .build();
            apply.connect_clicked(glib::clone!(
                #[weak(rename_to = composer)]
                self,
                move |button| composer.apply_suggestion(button, &suggestion)
            ));
            let card = assist_card();
            if !reason.is_empty() {
                card.append(
                    &gtk::Label::builder()
                        .label(reason)
                        .wrap(true)
                        .xalign(0.0)
                        .css_classes(["caption", "dim-label"])
                        .build(),
                );
            }
            card.append(&change);
            card.append(&apply);
            imp.assist_results.append(&card);
        }
    }

    fn apply_suggestion(&self, button: &gtk::Button, suggestion: &Suggestion) {
        let Some(webview) = self.imp().webview.borrow().clone() else {
            return;
        };
        editor::replace_text(
            &webview,
            &suggestion.original,
            &suggestion.replacement,
            glib::clone!(
                #[weak(rename_to = composer)]
                self,
                #[weak]
                button,
                move |is_found| {
                    if is_found {
                        button.set_label(&gettext("Applied"));
                        button.set_sensitive(false);
                    } else {
                        composer
                            .imp()
                            .toast_overlay
                            .add_toast(adw::Toast::new(&gettext(
                                "That text isn't in the draft any more.",
                            )));
                    }
                }
            ),
        );
    }

    /// The rewrite to read before it replaces anything.
    fn show_rewrite(&self, text: &str) {
        let imp = self.imp();
        imp.assist_status.set_label(&gettext(
            "Replacing keeps your signature and the quoted message.",
        ));
        let card = assist_card();
        card.append(
            &gtk::Label::builder()
                .label(text)
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build(),
        );
        let replace = gtk::Button::builder()
            .label(gettext("Replace Draft"))
            .halign(gtk::Align::End)
            .css_classes(["suggested-action"])
            .build();
        let html = rustle_core::html::to_editor_html(text);
        replace.connect_clicked(glib::clone!(
            #[weak(rename_to = composer)]
            self,
            move |button| {
                let Some(webview) = composer.imp().webview.borrow().clone() else {
                    return;
                };
                button.set_label(&gettext("Replaced"));
                button.set_sensitive(false);
                editor::replace_own(
                    &webview,
                    &html,
                    glib::clone!(
                        #[weak]
                        composer,
                        #[weak]
                        webview,
                        #[weak]
                        button,
                        move |previous| composer.offer_undo(&webview, &button, previous)
                    ),
                );
            }
        ));
        card.append(&replace);
        imp.assist_results.append(&card);
    }

    /// A toast whose Undo puts the text from before the rewrite back.
    fn offer_undo(&self, webview: &webkit::WebView, button: &gtk::Button, previous: String) {
        let toast = adw::Toast::builder()
            .title(gettext("Draft rewritten"))
            .button_label(gettext("Undo"))
            .build();
        toast.connect_button_clicked(glib::clone!(
            #[weak]
            webview,
            #[weak]
            button,
            move |_| {
                editor::replace_own(&webview, &previous, |_| {});
                button.set_label(&gettext("Replace Draft"));
                button.set_sensitive(true);
            }
        ));
        self.imp().toast_overlay.add_toast(toast);
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
        editor::connect_image_clicked(
            &webview,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |info| window.on_image_clicked(info)
            ),
        );
        editor::connect_link_clicked(
            &webview,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |info| window.on_link_in_editor_clicked(info)
            ),
        );
        imp.body_container.append(&webview);
        imp.webview.replace(Some(webview));
        self.build_image_actions();
        self.build_color_menus();
        self.build_link_actions();
    }

    // --- colours ----------------------------------------------------------

    fn build_color_menus(&self) {
        let imp = self.imp();
        let text = ColorMenu::attach(
            &imp.text_color_button,
            TEXT_PALETTE,
            &gettext("Default Colour"),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |color| window.set_color(ColorKind::Text, color)
            ),
        );
        let highlight = ColorMenu::attach(
            &imp.highlight_button,
            HIGHLIGHT_PALETTE,
            &gettext("No Highlight"),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |color| window.set_color(ColorKind::Highlight, color)
            ),
        );
        imp.text_color_menu.replace(Some(text));
        imp.highlight_menu.replace(Some(highlight));
    }

    fn set_color(&self, kind: ColorKind, color: Option<gdk::RGBA>) {
        if let Some(webview) = self.imp().webview.borrow().as_ref() {
            editor::set_color(webview, kind, color.as_ref());
            webview.grab_focus();
        }
    }

    // --- links ------------------------------------------------------------

    /// The `link.` actions behind the menu on a clicked link.
    fn build_link_actions(&self) {
        let group = gio::SimpleActionGroup::new();
        let edit = gio::SimpleAction::new("edit", None);
        edit.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                let link = window.imp().clicked_link.borrow().clone();
                if let Some(link) = link {
                    window.edit_link(link);
                }
            }
        ));
        group.add_action(&edit);
        let remove = gio::SimpleAction::new("remove", None);
        remove.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                let imp = window.imp();
                let link = imp.clicked_link.borrow().clone();
                if let (Some(link), Some(webview)) = (link, imp.webview.borrow().as_ref()) {
                    editor::remove_link(webview, link.index);
                }
            }
        ));
        group.add_action(&remove);
        self.insert_action_group("link", Some(&group));
    }

    /// Pop a menu up under a link the user clicked: where it goes, and the
    /// choice to change or remove it.
    fn on_link_in_editor_clicked(&self, info: LinkInfo) {
        let imp = self.imp();
        let Some(webview) = imp.webview.borrow().clone() else {
            return;
        };
        let Some(rect) = info.rect.clone() else {
            return;
        };
        let mut menu = imp.link_menu.borrow_mut();
        let (popover, model) = menu.get_or_insert_with(|| {
            let model = gio::Menu::new();
            let popover = gtk::PopoverMenu::from_model(Some(&model));
            popover.set_parent(&webview);
            popover.set_position(gtk::PositionType::Bottom);
            popover.set_has_arrow(true);
            (popover, model)
        });
        model.remove_all();
        let actions = gio::Menu::new();
        actions.append(Some(&gettext("Edit Link…")), Some("link.edit"));
        actions.append(Some(&gettext("Remove Link")), Some("link.remove"));
        model.append_section(Some(&editor::elide(&info.href, 48)), &actions);
        popover.set_pointing_to(Some(&rect.to_gdk()));
        imp.clicked_link.replace(Some(info));
        popover.popup();
    }

    /// Change `link`'s text or address through the link dialog.
    fn edit_link(&self, link: LinkInfo) {
        let index = link.index;
        editor::link_dialog(
            self,
            Some(&link),
            "",
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |text, href| {
                    if let Some(webview) = window.imp().webview.borrow().as_ref() {
                        editor::update_link(webview, index, &href, &text);
                        webview.grab_focus();
                    }
                }
            ),
        );
    }

    // --- inline images ----------------------------------------------------

    /// The `image.` actions behind the picture menu: `size` is a radio over
    /// the preset names, `remove` takes the picture out.
    fn build_image_actions(&self) {
        let group = gio::SimpleActionGroup::new();
        let size = gio::SimpleAction::new_stateful(
            "size",
            Some(glib::VariantTy::STRING),
            &"original".to_variant(),
        );
        size.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |action, parameter| {
                let Some(name) = parameter.and_then(|p| p.get::<String>()) else {
                    return;
                };
                action.set_state(&name.to_variant());
                let imp = window.imp();
                if let (Some(index), Some(webview)) =
                    (imp.selected_image.get(), imp.webview.borrow().as_ref())
                {
                    editor::resize_image(webview, index, &name);
                }
            }
        ));
        group.add_action(&size);
        let remove = gio::SimpleAction::new("remove", None);
        remove.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                let imp = window.imp();
                if let (Some(index), Some(webview)) =
                    (imp.selected_image.get(), imp.webview.borrow().as_ref())
                {
                    editor::remove_image(webview, index);
                }
            }
        ));
        group.add_action(&remove);
        self.insert_action_group("image", Some(&group));
        self.imp().image_size_action.replace(Some(size));
    }

    /// Pop the size menu up under a picture the user clicked. Each preset is
    /// labelled with the width it scales to and what the picture would then
    /// weigh, the way Outlook offers to shrink a picture to save data.
    fn on_image_clicked(&self, info: editor::ImageInfo) {
        let imp = self.imp();
        let Some(webview) = imp.webview.borrow().clone() else {
            return;
        };
        imp.selected_image.set(Some(info.index));
        if let Some(action) = imp.image_size_action.borrow().as_ref() {
            let current = info.current.clone().unwrap_or_default();
            action.set_state(&current.to_variant());
        }

        let mut menu = imp.image_menu.borrow_mut();
        let (popover, model) = menu.get_or_insert_with(|| {
            let model = gio::Menu::new();
            let popover = gtk::PopoverMenu::from_model(Some(&model));
            popover.set_parent(&webview);
            popover.set_position(gtk::PositionType::Bottom);
            popover.set_has_arrow(true);
            (popover, model)
        });

        model.remove_all();
        let sizes = gio::Menu::new();
        for option in &info.options {
            let label = match option.name.as_str() {
                "small" => gettext("Small"),
                "medium" => gettext("Medium"),
                "large" => gettext("Large"),
                _ => gettext("Original"),
            };
            let item = gio::MenuItem::new(
                Some(&i18n::format(
                    &gettext("{label} ({width} px, {size})"),
                    &[
                        ("label", &label),
                        ("width", &option.width.to_string()),
                        ("size", &glib::format_size(option.bytes)),
                    ],
                )),
                None,
            );
            item.set_action_and_target_value(Some("image.size"), Some(&option.name.to_variant()));
            sizes.append_item(&item);
        }
        model.append_section(
            Some(&i18n::format(
                &gettext("Image, {size}"),
                &[("size", &glib::format_size(info.bytes))],
            )),
            &sizes,
        );
        let actions = gio::Menu::new();
        actions.append(Some(&gettext("Remove Image")), Some("image.remove"));
        model.append_section(None, &actions);

        popover.set_pointing_to(Some(&info.rect.to_gdk()));
        popover.popup();
    }

    fn on_editor_changed(&self, json: &str) {
        let Ok(payload) = serde_json::from_str::<editor::EditorPayload>(json) else {
            return;
        };
        let imp = self.imp();
        imp.body_html.replace(payload.html);
        imp.selection.replace(payload.selection);
        imp.current_link.replace(payload.link);
        imp.is_syncing_buttons.set(true);
        for (name, command) in FORMAT_COMMANDS {
            self.format_button(name)
                .set_active(payload.states.get(command).copied().unwrap_or(false));
        }
        imp.is_syncing_buttons.set(false);
        if let Some(menu) = imp.text_color_menu.borrow().as_ref() {
            menu.set_current(payload.colors.text());
        }
        if let Some(menu) = imp.highlight_menu.borrow().as_ref() {
            menu.set_current(payload.colors.highlight());
        }
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

    /// The toolbar's link button: edits the link under the caret, or links
    /// the selection (or fresh text) somewhere new.
    fn on_link_clicked(&self) {
        let imp = self.imp();
        let current = imp.current_link.borrow().clone();
        if let Some(link) = current {
            self.edit_link(link);
            return;
        }
        let selection = imp.selection.borrow().clone();
        editor::link_dialog(
            self,
            None,
            &selection,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |text, href| {
                    if let Some(webview) = window.imp().webview.borrow().as_ref() {
                        editor::insert_link(webview, &href, &text);
                        webview.grab_focus();
                    }
                }
            ),
        );
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
            self.root().and_downcast_ref::<gtk::Window>(),
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
                imp.attachments_list
                    .set_visible(!imp.attachments.borrow().is_empty());
            }
        ));
        row.add_suffix(&remove_button);
        imp.attachments_list.append(&row);
        imp.attachments_list.set_visible(true);
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

    fn snapshot(&self) -> Snapshot {
        let imp = self.imp();
        Snapshot {
            fields: [&imp.to_row, &imp.cc_row, &imp.bcc_row, &imp.subject_row]
                .map(|row| row.text().trim().to_string()),
            body: self.preview_text(),
            attachments: imp.attachments.borrow().len(),
        }
    }

    /// Whether anything was written since it opened. A reply nobody typed
    /// in, or a draft reopened and left alone, closes without asking.
    fn is_changed(&self) -> bool {
        self.has_content() && self.snapshot() != *self.imp().opened_with.borrow()
    }

    fn on_cancel_clicked(&self) {
        if self.is_changed() {
            self.ask_to_save();
        } else {
            self.close();
        }
    }

    /// Closing with something written: keep it as a draft, or throw it away.
    fn ask_to_save(&self) {
        // Once: a quit can ask while its window's close already has.
        if self.imp().is_asking.replace(true) {
            return;
        }
        let is_resumed = self.imp().resumed.borrow().is_some();
        let (heading, body, delete) = if is_resumed {
            (
                gettext("Save Changes to Draft?"),
                gettext("The draft in this account's Drafts folder will be updated."),
                gettext("Delete Draft"),
            )
        } else {
            (
                gettext("Save as Draft?"),
                gettext(
                    "The message will be kept in this account's Drafts folder to finish later.",
                ),
                gettext("Delete"),
            )
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(body)
            .build();
        dialog.add_response("keep", &gettext("Keep Editing"));
        dialog.add_response("delete", &delete);
        dialog.add_response("save", &gettext("Save Draft"));
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("keep");
        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, response| {
                    match response {
                        "save" => window.save_draft(),
                        "delete" => window.delete_draft(),
                        _ => {}
                    }
                    window.on_answered();
                }
            ),
        );
        dialog.present(Some(self));
    }

    /// Tell the hosts waiting on the question whether it has gone. A save
    /// that failed keeps it open, as Keep Editing does.
    fn on_answered(&self) {
        let imp = self.imp();
        imp.is_asking.set(false);
        let has_gone = imp.is_done.get();
        let waiting = imp.waiting_hosts.take();
        if waiting.is_empty() {
            return;
        }
        // Once the dialog is through: a host may close the window under it.
        glib::idle_add_local_once(move || {
            for then in waiting {
                then(has_gone);
            }
        });
    }

    /// Keep it in the account's Drafts: in the database at once, so it is
    /// never lost, then on the server from a worker, replacing the copy an
    /// earlier save left there.
    fn save_draft(&self) {
        let account = self.account();
        let raw = match self.build_raw(&account, true) {
            Ok(raw) => raw,
            Err(error) => {
                log::error!("could not build the draft for {}: {error}", account.email);
                self.imp()
                    .toast_overlay
                    .add_toast(adw::Toast::new(&i18n::format(
                        &gettext("Couldn't save the draft: {msg}"),
                        &[("msg", &error.to_string())],
                    )));
                return;
            }
        };
        let message_id = self.imp().message_id.borrow().clone();
        let subject = self.imp().subject_row.text().trim().to_string();
        let subject = if subject.is_empty() {
            NO_SUBJECT.to_string()
        } else {
            subject
        };
        let resumed = self.imp().resumed.take();
        let saved = {
            let db = self.db();
            let db = db.borrow();
            if let Some(resumed) = &resumed {
                // The new copy takes its place, here and on the server.
                if let Err(error) = db.delete_email(resumed.email_id) {
                    log::warn!("could not remove the earlier copy of a draft: {error}");
                }
            }
            db.drafts_folder(account.id).and_then(|folder| {
                let mut header = self.local_header(&self.recipients_display(), &subject);
                header.message_id = message_id.clone();
                let row = db.save_email(folder.id, &header)?;
                db.save_raw_message(row.id, &raw)?;
                Ok((row.id, folder.name))
            })
        };
        let (email_id, folder_name) = match saved {
            Ok(saved) => saved,
            Err(error) => {
                log::error!("could not save the draft for {}: {error}", account.email);
                self.imp()
                    .toast_overlay
                    .add_toast(adw::Toast::new(&i18n::format(
                        &gettext("Couldn't save the draft: {msg}"),
                        &[("msg", &error.to_string())],
                    )));
                return;
            }
        };
        // Reopened from one account and saved from another: the old
        // account's copy goes.
        let moved_from = resumed
            .filter(|resumed| resumed.account_id != account.id)
            .and_then(|resumed| self.account_by_id(resumed.account_id));
        // Strong: the composer has gone by the time the server answers,
        // and its host still hears what became of the draft.
        // A message to be encrypted isn't put on the server in the clear,
        // even as a draft.
        if self.imp().pgp_encrypt.get() {
            self.notice(&gettext(
                "The draft is kept on this device only, as the message is to be encrypted.",
            ));
            self.finish();
            return;
        }
        let composer = self.clone();
        // Saved on the way out, the last window closes right after: the
        // hold keeps the app up until the server has its copy.
        let hold = gio::Application::default().map(|app| app.hold());
        workers::run(
            move || {
                if let Some(old) = &moved_from {
                    discard_draft_job(old, &message_id);
                }
                save_draft_job(&account, &raw, &message_id)
            },
            move |result| {
                composer.on_draft_saved(result, email_id, &folder_name);
                drop(hold);
            },
        );
        self.finish();
    }

    fn on_draft_saved(
        &self,
        result: Result<Option<sync::SavedDraft>, String>,
        email_id: i64,
        folder_name: &str,
    ) {
        let saved = match result {
            Ok(Some(saved)) => saved,
            Ok(None) => {
                self.notice(&gettext(
                    "This account has no Drafts folder on the server, so the draft is kept on this device only.",
                ));
                return;
            }
            Err(message) => {
                self.notice(&i18n::format(
                    &gettext(
                        "Couldn't save the draft to the server: {msg}. It is kept on this device.",
                    ),
                    &[("msg", &message)],
                ));
                return;
            }
        };
        // The row becomes the server copy, which the next sync then finds.
        // Filed in a stand-in folder before the folder list synced, it gives
        // way to the server copy instead.
        let db = self.db();
        let db = db.borrow();
        let adopted = match saved.uid {
            _ if saved.mailbox != folder_name => db.delete_email(email_id),
            Some(uid) => db.adopt_server_uid(email_id, &uid),
            None => Ok(()),
        };
        if let Err(error) = adopted {
            log::warn!("could not match the draft to its server copy: {error}");
        }
    }

    /// Throw it away, and with it the saved draft it was finishing.
    fn delete_draft(&self) {
        if let Some(resumed) = self.imp().resumed.take() {
            if let Err(error) = self.db().borrow().delete_email(resumed.email_id) {
                log::warn!("could not delete a draft: {error}");
            }
            if let Some(account) = self.account_by_id(resumed.account_id) {
                let message_id = resumed.message_id;
                workers::run(move || discard_draft_job(&account, &message_id), |()| {});
            }
            self.finish();
            return;
        }
        self.close();
    }

    /// It is going for good: sent, saved, thrown away or never written in.
    fn close(&self) {
        self.imp().is_done.set(true);
        self.emit_by_name::<()>("closed", &[]);
    }

    /// Close, then tell the host the folders changed. Closed first: the
    /// refresh may move the selection, which would pop an inline composer
    /// still there out into a window.
    fn finish(&self) {
        self.close();
        self.emit_by_name::<()>("finished", &[]);
    }

    fn notice(&self, text: &str) {
        self.emit_by_name::<()>("notice", &[&text.to_string()]);
    }

    fn account_by_id(&self, account_id: i64) -> Option<Account> {
        self.imp()
            .accounts
            .borrow()
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
    }

    /// The message as it stands: for the wire, or as a draft, which keeps
    /// its Bcc and whatever the address fields hold so far.
    fn build_raw(
        &self,
        account: &Account,
        is_draft: bool,
    ) -> Result<Vec<u8>, compose::ComposeError> {
        let imp = self.imp();
        let (to, cc, bcc) = (self.to_addrs(), self.cc_addrs(), self.bcc_addrs());
        let subject = imp.subject_row.text().trim().to_string();
        let body_html = imp.body_html.borrow();
        let attachments = self.attachments();
        let message_id = imp.message_id.borrow();
        let (in_reply_to, references) = (imp.in_reply_to.borrow(), imp.references.borrow());
        let message = compose::Outgoing {
            from: &account.email,
            to: &to,
            cc: &cc,
            bcc: &bcc,
            subject: &subject,
            body_html: &body_html,
            attachments: &attachments,
            message_id: Some(&message_id),
            in_reply_to: &in_reply_to,
            references: &references,
            plain_text: imp.plain_text.get(),
        };
        if is_draft {
            compose::build_draft_message(&message)
        } else {
            compose::build_mime_message(&message)
        }
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
        if let Err(error) = self.db().borrow_mut().record_sent_contacts(&contacts) {
            log::warn!("could not remember the recipients: {error}");
        }

        let protection = self.protection();
        if protection.sign || protection.encrypt {
            self.send_protected(
                account,
                subject,
                (to_addrs, cc_addrs, bcc_addrs),
                protection,
            );
            return;
        }
        let raw = match self.build_raw(&account, false) {
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

        self.send_built(account, subject, to_addrs, cc_addrs, bcc_addrs, raw);
    }

    /// Sign and/or encrypt off the main thread -- gpg may wait on its
    /// passphrase prompt -- then send as usual.
    fn send_protected(
        &self,
        account: Account,
        subject: String,
        (to_addrs, cc_addrs, bcc_addrs): (Vec<String>, Vec<String>, Vec<String>),
        protection: compose::Protection,
    ) {
        let imp = self.imp();
        let body_html = imp.body_html.borrow().clone();
        let attachments = self.attachments();
        let message_id = imp.message_id.borrow().clone();
        let in_reply_to = imp.in_reply_to.borrow().clone();
        let references = imp.references.borrow().clone();
        let plain_text = imp.plain_text.get();
        let recipients: Vec<String> = to_addrs
            .iter()
            .chain(&cc_addrs)
            .chain(&bcc_addrs)
            .map(|text| rustle_core::address::first_address(text))
            .filter(|address| !address.is_empty())
            .collect();
        let (from, job_to, job_cc, job_bcc, job_subject) = (
            account.email.clone(),
            to_addrs.clone(),
            cc_addrs.clone(),
            bcc_addrs.clone(),
            subject.clone(),
        );
        self.set_sending(true);
        workers::run(
            move || -> Result<Vec<u8>, String> {
                let message = compose::Outgoing {
                    from: &from,
                    to: &job_to,
                    cc: &job_cc,
                    bcc: &job_bcc,
                    subject: &job_subject,
                    body_html: &body_html,
                    attachments: &attachments,
                    message_id: Some(&message_id),
                    in_reply_to: &in_reply_to,
                    references: &references,
                    plain_text,
                };
                let gpg = rustle_core::pgp::Gpg {
                    sender: rustle_core::address::first_address(&from),
                    recipients,
                };
                compose::build_protected_message(&message, protection, &gpg).map_err(|error| {
                    log::error!("could not protect a message from {from}: {error}");
                    error.to_string()
                })
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result: Result<Vec<u8>, String>| {
                    window.set_sending(false);
                    match result {
                        Ok(raw) => {
                            window.send_built(account, subject, to_addrs, cc_addrs, bcc_addrs, raw)
                        }
                        Err(message) => {
                            window
                                .imp()
                                .toast_overlay
                                .add_toast(adw::Toast::new(&i18n::format(
                                    &gettext("Couldn't send: {msg}"),
                                    &[("msg", &message)],
                                )))
                        }
                    }
                }
            ),
        );
    }

    /// The rest of a send, once the message is built: the Outbox, then
    /// sending now or holding it for later.
    fn send_built(
        &self,
        account: Account,
        subject: String,
        to_addrs: Vec<String>,
        cc_addrs: Vec<String>,
        bcc_addrs: Vec<String>,
        raw: Vec<u8>,
    ) {
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

        // Finishing a saved draft: the Outbox holds the message now, so the
        // draft goes, here at once and on the server once the send has run.
        let finished_draft = self.imp().resumed.take().map(|resumed| {
            if let Err(error) = self.db().borrow().delete_email(resumed.email_id) {
                log::warn!("could not remove the draft being sent: {error}");
            }
            (self.account_by_id(resumed.account_id), resumed.message_id)
        });

        // Held back for Undo, or until the time Send Later picked, when the
        // host can do that; the Outbox sends it then.
        let delay = self
            .imp()
            .settings
            .borrow()
            .as_ref()
            .map_or(0, |settings| settings.int(keys::UNDO_SEND_SECONDS).max(0));
        let scheduled = self.imp().scheduled_for.take();
        let handler = self.imp().queue_handler.borrow().clone();
        let send_at = scheduled.or_else(|| {
            (delay > 0).then(|| chrono::Local::now() + chrono::Duration::seconds(delay.into()))
        });
        let entry = OutboxEntry {
            send_at: send_at.map(dates::to_utc_iso).unwrap_or_default(),
            recipients: recipients.clone(),
        };
        if let Err(error) = self.db().borrow().set_outbox_entry(email_id, &entry) {
            log::error!("could not record when to send queued message {email_id}: {error}");
        }
        let is_queued = match (handler, send_at) {
            (Some(handler), Some(send_at)) => handler(QueuedSend {
                account: account.clone(),
                email_id,
                send_at,
                is_scheduled: scheduled.is_some(),
            }),
            _ => false,
        };
        if is_queued {
            if let Some((Some(draft_account), message_id)) = finished_draft {
                workers::run(
                    move || discard_draft_job(&draft_account, &message_id),
                    |_| {},
                );
            }
            self.finish();
            return;
        }
        if send_at.is_some() {
            // The host is gone and nothing will wake for it: it goes now.
            let now = OutboxEntry {
                send_at: String::new(),
                recipients: recipients.clone(),
            };
            if let Err(error) = self.db().borrow().set_outbox_entry(email_id, &now) {
                log::error!("could not reschedule message {email_id} to go now: {error}");
            }
        }

        // Sent from here, it's claimed app-wide like a drained one, so no
        // drain sends it alongside, and it's filed the same way -- whether
        // or not this composer is still around to hear back (an inline one
        // goes with its window). The hold keeps the app up until then.
        let app = gio::Application::default().and_downcast::<RustleApplication>();
        let in_flight = app
            .as_ref()
            .map(RustleApplication::in_flight)
            .unwrap_or_default();
        let hold = app.as_ref().map(|app| app.hold());
        in_flight.borrow_mut().claim(email_id);
        let mut sent_header = self.local_header(&account.email, &subject);
        sent_header.sender_address = account.email.clone();
        sent_header.preview = subject.clone();
        let job = outbox::Job {
            email_id,
            recipients,
            raw,
            sent_header,
        };
        self.set_sending(true);
        let job_account = account.clone();
        let db = self.db();
        let composer = self.downgrade();
        workers::run(
            move || {
                let attempt = send_job(&job_account, job);
                if let Some((Some(draft_account), message_id)) = &finished_draft {
                    discard_draft_job(draft_account, message_id);
                }
                attempt
            },
            move |attempt: outbox::Attempt| {
                let settled = outbox::settle(
                    &db.borrow(),
                    &mut in_flight.borrow_mut(),
                    account.id,
                    vec![attempt],
                );
                drop(hold);
                let Some(composer) = composer.upgrade() else {
                    return;
                };
                match settled.errors.first() {
                    Some(error) => composer.on_send_failed(&i18n::failure_message(error)),
                    None => composer.finish(),
                }
            },
        );
    }

    fn on_send_failed(&self, message: &str) {
        self.set_sending(false);
        self.imp()
            .toast_overlay
            .add_toast(adw::Toast::new(&i18n::format(
                &gettext("Couldn't send: {msg}. Saved to Outbox."),
                &[("msg", message)],
            )));
        self.finish();
    }

    fn set_sending(&self, is_sending: bool) {
        let imp = self.imp();
        imp.send_button.set_sensitive(!is_sending);
        imp.from_dropdown.set_sensitive(!is_sending);
        imp.cancel_button.set_sensitive(!is_sending);
        imp.send_spinner.set_visible(is_sending);
        imp.send_spinner.set_spinning(is_sending);
    }
}

/// What the assistant came back with.
enum Answer {
    Review(Vec<Suggestion>),
    Rewrite(String),
}

fn unusable(answer: &str) -> String {
    log::debug!("unusable assistant answer: {answer}");
    gettext("the answer wasn't in the expected form")
}

fn style_label(style: RewriteStyle) -> String {
    match style {
        RewriteStyle::Shorter => gettext("Shorter"),
        RewriteStyle::Friendlier => gettext("Friendlier"),
        RewriteStyle::Formal => gettext("More Formal"),
        RewriteStyle::Clearer => gettext("Clearer"),
    }
}

fn assist_card() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .css_classes(["card", "assist-card"])
        .build()
}

fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// Runs on the worker thread: network only, no widgets, no database.
fn send_job(account: &Account, job: outbox::Job) -> outbox::Attempt {
    let Some(credential) = secrets::smtp_credential_for(account) else {
        log::warn!("could not sign in to account {}", account.email);
        return outbox::Attempt {
            job,
            error: Some(Failure::NoCredential),
        };
    };
    let error = sync::send_message(
        account,
        &credential,
        &account.email,
        &job.recipients,
        &job.raw,
    )
    .err()
    .map(|error: NetError| {
        log::error!(
            "could not send message {} ({:?}) to {} via {} (account {}): {error}",
            job.email_id,
            job.sent_header.subject,
            job.recipients.join(", "),
            account.smtp_host,
            account.email
        );
        classify(&error, &account.smtp_host)
    });
    outbox::Attempt { job, error }
}

/// Runs on the worker thread: file a draft on the server.
fn save_draft_job(
    account: &Account,
    raw: &[u8],
    message_id: &str,
) -> Result<Option<sync::SavedDraft>, String> {
    let Some(credential) = secrets::credential_for(account) else {
        log::warn!("could not sign in to account {}", account.email);
        return Err(gettext("Could not sign in to this account."));
    };
    sync::save_draft(account, &credential, raw, message_id).map_err(|error: NetError| {
        log::error!(
            "could not save a draft to Drafts on {} (account {}): {error}",
            account.imap_host,
            account.email
        );
        i18n::failure_message(&classify(&error, &account.imap_host))
    })
}

/// Runs on the worker thread: remove a draft's server copies. Nobody is
/// waiting on this, so a failure is only logged.
pub(crate) fn discard_draft_job(account: &Account, message_id: &str) {
    let Some(credential) = secrets::credential_for(account) else {
        log::warn!("could not sign in to account {}", account.email);
        return;
    };
    if let Err(error) = sync::discard_draft(account, &credential, message_id) {
        log::error!(
            "could not delete a draft from Drafts on {} (account {}): {error}",
            account.imap_host,
            account.email
        );
    }
}

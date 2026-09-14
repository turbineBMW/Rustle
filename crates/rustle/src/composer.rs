//! The composer: recipients, a contenteditable WebKit editor, attachments,
//! and sending through the Outbox so a crash mid-send never loses a message.

use crate::editor::{self, ColorKind, LinkInfo, FORMAT_COMMANDS};
use crate::i18n::{self, gettext};
use crate::widgets::color_menu::{ColorMenu, HIGHLIGHT_PALETTE, TEXT_PALETTE};
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
use rustle_core::models::NO_SUBJECT;
use rustle_core::models::{Account, Attachment, MessageHeader};
use rustle_core::net::errors::{classify, NetError};
use rustle_core::secrets;
use rustle_core::sync;
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

        window.build_from_dropdown(account);
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
        imp.from_dropdown.set_sensitive(!is_sending);
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

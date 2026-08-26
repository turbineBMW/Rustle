//! The signature editor: the composer's WYSIWYG page in a dialog, with a
//! formatting toolbar, saved as HTML when the user confirms.

use crate::editor::{self, FORMAT_COMMANDS};
use crate::i18n::gettext;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

type SaveCallback = Box<dyn Fn(&str)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SignatureDialog {
        pub html: RefCell<String>,
        pub webview: RefCell<Option<webkit::WebView>>,
        pub buttons: RefCell<HashMap<&'static str, gtk::ToggleButton>>,
        pub is_syncing_buttons: Cell<bool>,
        pub on_save: RefCell<Option<SaveCallback>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SignatureDialog {
        const NAME: &'static str = "RustleSignatureDialog";
        type Type = super::SignatureDialog;
        type ParentType = adw::Dialog;
    }

    impl ObjectImpl for SignatureDialog {}
    impl WidgetImpl for SignatureDialog {}
    impl AdwDialogImpl for SignatureDialog {}
}

glib::wrapper! {
    pub struct SignatureDialog(ObjectSubclass<imp::SignatureDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SignatureDialog {
    /// Opens on `html`; `on_save` gets the edited fragment when Save is
    /// pressed, and nothing when the dialog is dismissed.
    pub fn new(title: &str, html: &str, on_save: impl Fn(&str) + 'static) -> Self {
        let dialog: Self = glib::Object::builder()
            .property("title", gettext("Signature"))
            .property("content-width", 560)
            .property("content-height", 420)
            .build();
        let imp = dialog.imp();
        imp.html.replace(html.to_string());
        imp.on_save.replace(Some(Box::new(on_save)));
        dialog.build(title);
        dialog
    }

    fn build(&self, subtitle: &str) {
        let imp = self.imp();

        let cancel = gtk::Button::with_label(&gettext("Cancel"));
        cancel.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| {
                dialog.close();
            }
        ));
        let save = gtk::Button::builder()
            .label(gettext("Save"))
            .css_classes(["suggested-action"])
            .build();
        save.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_save_clicked()
        ));
        let header = adw::HeaderBar::builder()
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .title_widget(
                &adw::WindowTitle::builder()
                    .title(gettext("Signature"))
                    .subtitle(subtitle)
                    .build(),
            )
            .build();
        header.pack_start(&cancel);
        header.pack_end(&save);

        let toolbar = gtk::Box::builder()
            .spacing(6)
            .css_classes(["toolbar"])
            .build();
        let icons = [
            ("bold", "format-text-bold-symbolic", gettext("Bold")),
            ("italic", "format-text-italic-symbolic", gettext("Italic")),
            (
                "underline",
                "format-text-underline-symbolic",
                gettext("Underline"),
            ),
            (
                "strike",
                "format-text-strikethrough-symbolic",
                gettext("Strikethrough"),
            ),
            (
                "bullets",
                "view-list-bullet-symbolic",
                gettext("Bulleted List"),
            ),
            (
                "numbers",
                "view-list-ordered-symbolic",
                gettext("Numbered List"),
            ),
        ];
        for (name, icon, tooltip) in icons {
            if name == "bullets" {
                toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
            }
            let button = gtk::ToggleButton::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .css_classes(["flat"])
                .build();
            let command = FORMAT_COMMANDS
                .iter()
                .find(|(each, _)| *each == name)
                .map(|(_, command)| *command)
                .unwrap_or(name);
            button.connect_toggled(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |_| dialog.on_format_toggled(command)
            ));
            toolbar.append(&button);
            imp.buttons.borrow_mut().insert(name, button);
        }
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));

        let link = gtk::Button::builder()
            .icon_name("insert-link-symbolic")
            .tooltip_text(gettext("Insert Link"))
            .css_classes(["flat"])
            .build();
        link.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_link_clicked()
        ));
        toolbar.append(&link);

        let color = gtk::ColorDialogButton::builder()
            .dialog(
                &gtk::ColorDialog::builder()
                    .title(gettext("Text Colour"))
                    .with_alpha(false)
                    .build(),
            )
            .tooltip_text(gettext("Text Colour"))
            .valign(gtk::Align::Center)
            .build();
        color.connect_rgba_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |button| dialog.on_color_picked(&button.rgba())
        ));
        toolbar.append(&color);

        let sizes = gtk::DropDown::from_strings(&[
            &gettext("Small"),
            &gettext("Normal"),
            &gettext("Large"),
            &gettext("Huge"),
        ]);
        sizes.set_selected(1);
        sizes.set_tooltip_text(Some(&gettext("Text Size")));
        sizes.set_valign(gtk::Align::Center);
        sizes.connect_selected_notify(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |dropdown| {
                // execCommand's 1-7 scale; 3 is the default size.
                let size = ["2", "3", "5", "6"][dropdown.selected().min(3) as usize];
                dialog.exec("fontSize", Some(size));
            }
        ));
        toolbar.append(&sizes);

        let clear = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text(gettext("Clear Formatting"))
            .css_classes(["flat"])
            .hexpand(true)
            .halign(gtk::Align::End)
            .build();
        clear.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.exec("removeFormat", None)
        ));
        toolbar.append(&clear);

        let webview = editor::build_webview(
            &imp.html.borrow(),
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |json| dialog.on_editor_changed(json)
            ),
        );
        let card = gtk::Box::builder()
            .overflow(gtk::Overflow::Hidden)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .vexpand(true)
            .css_classes(["card"])
            .build();
        card.append(&webview);
        imp.webview.replace(Some(webview));

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.add_top_bar(&toolbar);
        view.set_content(Some(&card));
        self.set_child(Some(&view));
        self.set_default_widget(Some(&save));
    }

    fn on_editor_changed(&self, json: &str) {
        let Ok(payload) = serde_json::from_str::<editor::EditorPayload>(json) else {
            return;
        };
        let imp = self.imp();
        imp.html.replace(payload.html);
        imp.is_syncing_buttons.set(true);
        for (name, command) in FORMAT_COMMANDS {
            if let Some(button) = imp.buttons.borrow().get(name) {
                button.set_active(payload.states.get(command).copied().unwrap_or(false));
            }
        }
        imp.is_syncing_buttons.set(false);
    }

    fn exec(&self, command: &str, argument: Option<&str>) {
        if let Some(webview) = self.imp().webview.borrow().as_ref() {
            editor::exec(webview, command, argument);
            webview.grab_focus();
        }
    }

    fn on_format_toggled(&self, command: &str) {
        if self.imp().is_syncing_buttons.get() {
            return;
        }
        self.exec(command, None);
    }

    fn on_color_picked(&self, rgba: &gdk::RGBA) {
        self.exec("foreColor", Some(&crate::accent::rgba_hex(rgba)));
    }

    fn on_link_clicked(&self) {
        let entry = gtk::Entry::builder()
            .placeholder_text("https://")
            .activates_default(true)
            .build();
        let alert = adw::AlertDialog::builder()
            .heading(gettext("Insert Link"))
            .extra_child(&entry)
            .build();
        alert.add_response("cancel", &gettext("Cancel"));
        alert.add_response("insert", &gettext("Insert"));
        alert.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
        alert.set_default_response(Some("insert"));
        alert.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |_, response| {
                    let url = entry.text().trim().to_string();
                    if response == "insert" && !url.is_empty() {
                        dialog.exec("createLink", Some(&url));
                    }
                }
            ),
        );
        alert.present(Some(self));
    }

    fn on_save_clicked(&self) {
        let html = self.imp().html.borrow().clone();
        // An "empty" editor still holds a <br> or an empty block.
        let html = if rustle_core::html::html_to_text(&html).trim().is_empty() {
            String::new()
        } else {
            html
        };
        if let Some(on_save) = self.imp().on_save.borrow().as_ref() {
            on_save(&html);
        }
        let _ = self.close();
    }
}

//! One message in the reading pane: its header and body,
//! rendered in a sandboxed WebKit view when it is HTML.

use crate::accent;
use crate::account_colors;
use crate::avatar_loader::AvatarLoader;
use crate::i18n::{self, gettext};
use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use rustle_core::darkmode;
use rustle_core::mime::{self, ParsedMessage, Unsubscribe};
use rustle_core::models::{Account, Attachment, Email};
use std::cell::RefCell;
use std::rc::Rc;
use webkit::prelude::*;

/// Delivered by the window once the raw message is in hand, or with the
/// reason it isn't.
pub type LoadCallback = Box<dyn FnOnce(Option<Vec<u8>>, Option<String>)>;
/// Asks the window for a message's raw bytes.
pub type LoadHandler = Rc<dyn Fn(&Email, LoadCallback)>;
/// Offers an unsubscribe target; the second argument hides the banner once done.
pub type UnsubscribeHandler = Rc<dyn Fn(&Unsubscribe, Box<dyn Fn()>)>;
/// Called once the selected message has rendered.
pub type RenderedCallback = Box<dyn Fn()>;

/// What the window does for a view: fetch bodies, save/open attachments, and
/// unsubscribe (the second argument hides the banner once the list confirmed).
pub struct Handlers {
    pub on_load: LoadHandler,
    pub on_save_attachment: Rc<dyn Fn(&Attachment)>,
    pub on_open_attachment: Rc<dyn Fn(&Attachment)>,
    pub on_unsubscribe: UnsubscribeHandler,
}

/// A mail body may name any scheme, and a registered handler will happily
/// take file:, smb: or tel: from a stranger. Only these are worth honouring.
const EXTERNAL_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

const GUTTER: i32 = 12;
const SMALL_GUTTER: i32 = 6;
/// Headers and message content line up with the subject. HTML keeps this
/// inset inside its own canvas so the padding shares the body's background.
const EDGE: i32 = 24;
const AVATAR_SIZE: i32 = 40;
const ATTACHMENT_WIDTH: i32 = 260;

thread_local! {
    // An unrelated WebView costs its own web process: ~300 MB and up to
    // 1.5 s to start. Related views share one, so every message body hangs
    // off this anchor, which belongs to no email and so survives
    // closing one. The composer's WebView stays unrelated on purpose -- it
    // runs JavaScript and must not share a process with untrusted mail HTML.
    static ANCHOR: RefCell<Option<webkit::WebView>> = const { RefCell::new(None) };
}

fn ensure_anchor() -> webkit::WebView {
    ANCHOR.with(|anchor| {
        if let Some(existing) = anchor.borrow().as_ref() {
            return existing.clone();
        }
        // A mail body is rendered once and never navigated back to, so it
        // needs none of the caches WEB_BROWSER (the default) keeps.
        if let Some(context) = webkit::WebContext::default() {
            context.set_cache_model(webkit::CacheModel::DocumentViewer);
        }
        let view = webkit::WebView::new();
        // The process starts on the first load, not on construction.
        view.load_html("", None);
        anchor.replace(Some(view.clone()));
        view
    })
}

/// Shut the shared web process down; the next message body starts a new one.
/// Its memory is worth holding while the user is reading and not while the
/// window is hidden.
pub fn release_anchor() {
    ANCHOR.with(|anchor| {
        if let Some(view) = anchor.borrow_mut().take() {
            view.terminate_web_process();
        }
    });
}

struct Inner {
    email: Email,
    handlers: Rc<Handlers>,
    on_rendered: Option<RenderedCallback>,
    should_load_remote_images: bool,
    is_released: bool,
    placeholder: Option<gtk::Label>,
    webview: Option<webkit::WebView>,
    images_banner: Option<adw::Banner>,
    html: Option<String>,
    pub raw: Option<Vec<u8>>,
    pub parsed: Option<ParsedMessage>,
}

#[derive(Clone)]
pub struct MessageView {
    root: gtk::Box,
    recipients: gtk::Box,
    body: gtk::Box,
    inner: Rc<RefCell<Inner>>,
}

impl MessageView {
    pub fn new(
        email: Email,
        handlers: Rc<Handlers>,
        on_rendered: RenderedCallback,
        should_load_remote_images: bool,
        avatars: &AvatarLoader,
        account: Option<&Account>,
    ) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .vexpand(true)
            .css_classes(["message-view"])
            .build();

        let header = gtk::Box::builder()
            .spacing(GUTTER)
            .margin_top(GUTTER)
            .margin_bottom(GUTTER)
            .margin_start(EDGE)
            .margin_end(EDGE)
            .build();
        let avatar = adw::Avatar::new(AVATAR_SIZE, Some(&email.sender), true);
        avatar.set_valign(gtk::Align::Start);
        header.append(&avatar);
        if !email.sender_address.is_empty() {
            let avatar = avatar.clone();
            avatars.load(&email.sender_address, move |texture| {
                avatar.set_custom_image(Some(texture))
            });
        }

        let names = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let sender_line = gtk::Box::builder().spacing(SMALL_GUTTER).build();
        let sender = gtk::Label::builder()
            .label(&email.sender)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        sender_line.append(&sender);
        if !email.sender_address.is_empty() {
            let address = gtk::Label::builder()
                .label(&email.sender_address)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .hexpand(true)
                .selectable(true)
                .tooltip_text(&email.sender_address)
                .css_classes(["caption", "sender-address"])
                .build();
            sender_line.append(&address);
        }
        names.append(&sender_line);
        let recipients = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(2)
            .visible(false)
            .build();
        names.append(&recipients);
        header.append(&names);
        // Read from the unified inbox, the message names its account above
        // the date, in that account's colour.
        let meta = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        if let Some(account) = account {
            let name = gtk::Label::builder()
                .label(account.short_label())
                .tooltip_text(&account.email)
                .xalign(1.0)
                .ellipsize(pango::EllipsizeMode::End)
                .max_width_chars(24)
                .css_classes([
                    "caption",
                    "account-name",
                    &account_colors::css_class(account.id),
                ])
                .build();
            meta.append(&name);
        }
        let date = gtk::Label::builder()
            .label(i18n::date_label(&email.date))
            .xalign(1.0)
            .css_classes(["dim-label", "caption"])
            .build();
        meta.append(&date);
        header.append(&meta);

        root.append(&header);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .vexpand(true)
            .build();
        root.append(&body);

        let view = MessageView {
            root,
            recipients,
            body,
            inner: Rc::new(RefCell::new(Inner {
                email,
                handlers,
                on_rendered: Some(on_rendered),
                should_load_remote_images,
                is_released: false,
                placeholder: None,
                webview: None,
                images_banner: None,
                html: None,
                raw: None,
                parsed: None,
            })),
        };

        view.load();
        view
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn raw(&self) -> Option<Vec<u8>> {
        self.inner.borrow().raw.clone()
    }

    pub fn parsed(&self) -> Option<ParsedMessage> {
        self.inner.borrow().parsed.clone()
    }

    fn load(&self) {
        let (email, handlers) = {
            let mut inner = self.inner.borrow_mut();
            let placeholder = gtk::Label::builder()
                .label(gettext("Loading…"))
                .margin_top(GUTTER)
                .css_classes(["dim-label"])
                .build();
            self.body.append(&placeholder);
            inner.placeholder = Some(placeholder);
            (inner.email.clone(), inner.handlers.clone())
        };
        let this = self.clone();
        (handlers.on_load)(&email, Box::new(move |raw, error| this.on_raw(raw, error)));
    }

    fn on_raw(&self, raw: Option<Vec<u8>>, error: Option<String>) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.is_released {
                return;
            }
            if let Some(placeholder) = inner.placeholder.take() {
                self.body.remove(&placeholder);
            }
        }
        let Some(raw) = raw else {
            let label = gtk::Label::builder()
                .label(error.unwrap_or_else(|| gettext("Couldn't load this message.")))
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label"])
                .build();
            self.body.append(&label);
            return;
        };

        let parsed = mime::parse_message(&raw);
        {
            let mut inner = self.inner.borrow_mut();
            inner.raw = Some(raw);
            inner.parsed = Some(parsed.clone());
        }
        self.show_recipients(&parsed);
        self.populate_attachments(&parsed.attachments);
        self.show_unsubscribe(parsed.unsubscribe.as_ref());
        match &parsed.html_body {
            Some(html) => self.show_html(html),
            None => self.show_text(parsed.text_body.as_deref().unwrap_or("")),
        }

        let on_rendered = self.inner.borrow_mut().on_rendered.take();
        if let Some(on_rendered) = on_rendered {
            on_rendered();
        }
    }

    /// Recipient information lives alongside the sender in the header.
    fn show_recipients(&self, parsed: &ParsedMessage) {
        // GtkGrid can report the height at the label's minimum width as its
        // minimum height here, making a long recipient list consume the pane.
        // Box rows measure wrapping labels using the actual available width.
        let label_widths = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        for (label, value) in [
            (gettext("To"), parsed.to.join(", ")),
            (gettext("Cc"), parsed.cc.join(", ")),
            (gettext("Bcc"), parsed.bcc.join(", ")),
        ] {
            if value.is_empty() {
                continue;
            }
            let row = gtk::Box::builder().spacing(SMALL_GUTTER).build();
            let name = gtk::Label::builder()
                .label(label)
                .xalign(1.0)
                .valign(gtk::Align::Start)
                .css_classes(["caption", "dim-label"])
                .build();
            label_widths.add_widget(&name);
            row.append(&name);
            let content = gtk::Label::builder()
                .label(value)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .max_width_chars(1)
                .selectable(true)
                .hexpand(true)
                .css_classes(["caption", "recipient-address"])
                .build();
            row.append(&content);
            self.recipients.append(&row);
        }
        self.recipients
            .set_visible(self.recipients.first_child().is_some());
    }

    fn show_unsubscribe(&self, target: Option<&Unsubscribe>) {
        let Some(target) = target else { return };
        let banner = adw::Banner::builder()
            .title(gettext("You're subscribed to this mailing list."))
            .button_label(gettext("Unsubscribe"))
            .revealed(true)
            .build();
        let handlers = self.inner.borrow().handlers.clone();
        let target = target.clone();
        banner.connect_button_clicked(move |banner| {
            let banner = banner.clone();
            (handlers.on_unsubscribe)(&target, Box::new(move || banner.set_revealed(false)));
        });
        self.body.append(&banner);
    }

    fn show_text(&self, text: &str) {
        let label = gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .yalign(0.0)
            .margin_top(GUTTER)
            .margin_start(EDGE)
            .margin_end(EDGE)
            .margin_bottom(EDGE)
            .wrap(true)
            .selectable(true)
            .build();
        self.body.append(&label);
    }

    fn show_html(&self, html: &str) {
        let should_load_remote_images = {
            let mut inner = self.inner.borrow_mut();
            inner.html = Some(html.to_string());
            inner.should_load_remote_images
        };
        if !should_load_remote_images {
            let banner = adw::Banner::builder()
                .title(gettext(
                    "Remote images are blocked to protect your privacy.",
                ))
                .button_label(gettext("Show Images"))
                .revealed(true)
                .build();
            let this = self.clone();
            banner.connect_button_clicked(move |_| this.on_show_images());
            self.body.append(&banner);
            self.inner.borrow_mut().images_banner = Some(banner);
        }

        let webview = webkit::WebView::builder()
            .related_view(&ensure_anchor())
            .hexpand(true)
            .vexpand(true)
            .build();
        let root = self.root.clone();
        webview.connect_decide_policy(move |_, decision, _| decide_policy(&root, decision));
        if let Some(settings) = WebViewExt::settings(&webview) {
            settings.set_enable_javascript(false);
            // A message body needs none of these, and each one carries buffers.
            settings.set_enable_page_cache(false);
            settings.set_enable_media(false);
            settings.set_enable_webaudio(false);
            settings.set_enable_webgl(false);
            settings.set_enable_back_forward_navigation_gestures(false);
        }
        // Clearing the accelerated surface avoids a black frame before WebKit
        // paints; the CSS class supplies the white canvas email HTML expects.
        webview.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
        webview.add_css_class(if is_dark() {
            "message-html-dark"
        } else {
            "message-html"
        });
        webview.load_html(&self.sandboxed_html(), None);
        self.inner.borrow_mut().webview = Some(webview.clone());
        self.body.append(&webview);
    }

    fn sandboxed_html(&self) -> String {
        let inner = self.inner.borrow();
        // Keep the inset inside the HTML canvas: its background then paints
        // both the content and padding, even when the email sets a body colour.
        // Border-box includes that padding in the viewport's minimum height.
        // Links inherit the system accent unless they carry their own style.
        let mut style = format!(
            "html {{ margin: 0; padding: 0; }} \
             body {{ margin: 0 !important; padding: {GUTTER}px {EDGE}px {EDGE}px !important; \
             box-sizing: border-box; min-height: 100vh; }} \
             a:not([style]) {{ color: {}; }}",
            accent::accent_hex()
        );
        let html = inner.html.as_deref().unwrap_or("");
        // In dark mode the body is rewritten the way Outlook does it: light
        // canvases go dark, dark ink goes light, hues stay. The defaults an
        // unstyled message inherits follow suit.
        let html = if is_dark() {
            style.push_str(&format!(
                " :root {{ color-scheme: dark; }} body {{ background-color: {}; color: {}; }}",
                darkmode::CANVAS,
                darkmode::TEXT
            ));
            std::borrow::Cow::Owned(darkmode::adapt(html))
        } else {
            std::borrow::Cow::Borrowed(html)
        };
        mime::sandbox_html(&html, inner.should_load_remote_images, &style)
    }

    fn on_show_images(&self) {
        let webview = {
            let mut inner = self.inner.borrow_mut();
            if inner.webview.is_none() || inner.html.is_none() {
                return;
            }
            inner.should_load_remote_images = true;
            if let Some(banner) = &inner.images_banner {
                banner.set_revealed(false);
            }
            inner.webview.clone()
        };
        if let Some(webview) = webview {
            webview.load_html(&self.sandboxed_html(), None);
        }
    }

    fn populate_attachments(&self, attachments: &[Attachment]) {
        if attachments.is_empty() {
            return;
        }
        let section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(SMALL_GUTTER)
            .margin_start(EDGE)
            .margin_end(EDGE)
            .margin_bottom(GUTTER)
            .build();
        let heading = gtk::Label::builder()
            .label(gettext("Attachments"))
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build();
        section.append(&heading);
        let cards = adw::WrapBox::builder()
            .child_spacing(GUTTER)
            .line_spacing(SMALL_GUTTER)
            .justify(adw::JustifyMode::None)
            .align(0.0)
            .build();
        section.append(&cards);
        self.body.append(&section);
        let handlers = self.inner.borrow().handlers.clone();
        for attachment in attachments {
            let card = gtk::Box::builder()
                .width_request(ATTACHMENT_WIDTH)
                .hexpand(false)
                .css_classes(["attachment-card"])
                .build();
            let content = gtk::Box::builder().spacing(SMALL_GUTTER).build();
            content.append(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
            let labels = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .build();
            let filename = gtk::Label::builder()
                .label(&attachment.filename)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Middle)
                // Keep the natural width below the card's fixed request,
                // even for long filenames; the label fills available space.
                .max_width_chars(1)
                .css_classes(["heading"])
                .build();
            labels.append(&filename);
            labels.append(
                &gtk::Label::builder()
                    .label(glib::format_size(attachment.size() as u64))
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            content.append(&labels);
            let open_button = gtk::Button::builder()
                .child(&content)
                .hexpand(true)
                .tooltip_text(format!(
                    "{}\n{}",
                    attachment.filename,
                    gettext("Open with the default app")
                ))
                .css_classes(["flat", "attachment-open"])
                .build();
            let open = attachment.clone();
            let open_handlers = handlers.clone();
            open_button.connect_clicked(move |_| (open_handlers.on_open_attachment)(&open));
            card.append(&open_button);

            let save_button = gtk::Button::builder()
                .icon_name("document-save-symbolic")
                .valign(gtk::Align::Center)
                .margin_end(SMALL_GUTTER)
                .tooltip_text(gettext("Save Attachment"))
                .css_classes(["flat"])
                .build();
            let save = attachment.clone();
            let save_handlers = handlers.clone();
            save_button.connect_clicked(move |_| (save_handlers.on_save_attachment)(&save));
            card.append(&save_button);
            cards.append(&card);
        }
    }

    /// Drop the body's widgets and bytes; the view is dead after this. The
    /// web process stays up -- it belongs to the anchor, which every other
    /// message shares.
    pub fn release(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.is_released = true;
        if let Some(webview) = inner.webview.take() {
            self.body.remove(&webview);
        }
        inner.html = None;
        inner.raw = None;
        inner.parsed = None;
    }
}

/// The webview only ever renders the message body: the one navigation it may
/// perform is the load_html document itself. A click goes to the browser, and
/// anything else the body asks for is refused outright.
fn decide_policy(root: &gtk::Box, decision: &webkit::PolicyDecision) -> bool {
    let Some(navigation) = decision.downcast_ref::<webkit::NavigationPolicyDecision>() else {
        return false;
    };
    let Some(action) = navigation.navigation_action() else {
        return false;
    };
    let uri = action
        .request()
        .and_then(|request| request.uri())
        .map(|uri| uri.to_string())
        .unwrap_or_default();
    let scheme = uri.split(':').next().unwrap_or("").to_lowercase();

    if action.navigation_type() != webkit::NavigationType::LinkClicked {
        // load_html has no base URI, so its own document arrives as
        // about:blank -- or with no URI at all. Neither can leak anything.
        if scheme == "about" || scheme.is_empty() {
            return false;
        }
        decision.ignore();
        log::warn!("blocked navigation from a message body to {uri}");
        return true;
    }

    decision.ignore();
    if !EXTERNAL_SCHEMES.contains(&scheme.as_str()) {
        log::warn!("blocked a message body link with scheme {scheme:?}: {uri}");
        return true;
    }
    let window = root.root().and_downcast::<gtk::Window>();
    gtk::UriLauncher::new(&uri).launch(window.as_ref(), gtk::gio::Cancellable::NONE, |_| {});
    true
}

fn is_dark() -> bool {
    adw::StyleManager::default().is_dark()
}

#[cfg(test)]
#[path = "message_view_tests.rs"]
mod tests;

//! One message in the reading pane: a header row that expands to the body,
//! rendered in a sandboxed WebKit view when it is HTML.

use crate::accent;
use crate::account_colors;
use crate::avatar_loader::AvatarLoader;
use crate::i18n::{self, gettext};
use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use gtk::pango;
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
/// Called once the newest message has rendered.
pub type RenderedCallback = Box<dyn Fn(&MessageView)>;

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
/// Content sits on the reader's own left edge, lined up with the subject.
const EDGE: i32 = 24;
const AVATAR_SIZE: i32 = 40;
/// Tall enough that most messages need no inner scrolling; the WebView can't
/// report its content height until after layout.
const BODY_HEIGHT: i32 = 800;

thread_local! {
    // An unrelated WebView costs its own web process: ~300 MB and up to
    // 1.5 s to start. Related views share one, so every message body hangs
    // off this anchor, which belongs to no conversation and so survives
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
    is_loaded: bool,
    is_loading: bool,
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
    body: gtk::Box,
    revealer: gtk::Revealer,
    inner: Rc<RefCell<Inner>>,
}

impl MessageView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        email: Email,
        handlers: Rc<Handlers>,
        on_rendered: Option<RenderedCallback>,
        is_expanded: bool,
        should_load_remote_images: bool,
        avatars: &AvatarLoader,
        account: Option<&Account>,
    ) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
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
        let sender = gtk::Label::builder()
            .label(&email.sender)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        names.append(&sender);
        if !email.sender_address.is_empty() {
            let address = gtk::Label::builder()
                .label(&email.sender_address)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .selectable(true)
                .css_classes(["caption", "sender-address"])
                .build();
            names.append(&address);
        }
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

        let toggle = gtk::Button::builder()
            .child(&header)
            .css_classes(["flat", "message-header"])
            .build();
        root.append(&toggle);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(GUTTER)
            .margin_start(EDGE)
            .margin_end(EDGE)
            .margin_bottom(EDGE)
            .build();
        let revealer = gtk::Revealer::builder().child(&body).build();
        root.append(&revealer);

        let view = MessageView {
            root,
            body,
            revealer,
            inner: Rc::new(RefCell::new(Inner {
                email,
                handlers,
                on_rendered,
                should_load_remote_images,
                is_loaded: false,
                is_loading: false,
                is_released: false,
                placeholder: None,
                webview: None,
                images_banner: None,
                html: None,
                raw: None,
                parsed: None,
            })),
        };

        let this = view.clone();
        toggle.connect_clicked(move |_| this.on_toggle());
        if is_expanded {
            view.expand();
        }
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

    fn on_toggle(&self) {
        if self.revealer.reveals_child() {
            self.revealer.set_reveal_child(false);
        } else {
            self.expand();
        }
    }

    fn expand(&self) {
        self.revealer.set_reveal_child(true);
        let (email, handlers) = {
            let mut inner = self.inner.borrow_mut();
            if inner.is_loaded || inner.is_loading {
                return;
            }
            inner.is_loading = true;
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
            inner.is_loading = false;
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
            inner.is_loaded = true;
            inner.raw = Some(raw);
            inner.parsed = Some(parsed.clone());
        }
        self.show_details(&parsed);
        self.show_unsubscribe(parsed.unsubscribe.as_ref());
        match &parsed.html_body {
            Some(html) => self.show_html(html),
            None => self.show_text(parsed.text_body.as_deref().unwrap_or("")),
        }
        self.populate_attachments(&parsed.attachments);

        let on_rendered = self.inner.borrow_mut().on_rendered.take();
        if let Some(on_rendered) = on_rendered {
            on_rendered(self);
        }
    }

    /// A collapsed Details section with the full From/To/Cc/Bcc/Date.
    fn show_details(&self, parsed: &ParsedMessage) {
        let grid = gtk::Grid::builder()
            .row_spacing(4)
            .column_spacing(GUTTER)
            .margin_bottom(SMALL_GUTTER)
            .build();
        let mut row = 0;
        for (label, value) in [
            (gettext("From"), parsed.from_display.clone()),
            (gettext("To"), parsed.to.join(", ")),
            (gettext("Cc"), parsed.cc.join(", ")),
            (gettext("Bcc"), parsed.bcc.join(", ")),
            (gettext("Date"), parsed.date.clone()),
        ] {
            if value.is_empty() {
                continue;
            }
            let name = gtk::Label::builder()
                .label(label)
                .xalign(1.0)
                .valign(gtk::Align::Start)
                .css_classes(["dim-label"])
                .build();
            grid.attach(&name, 0, row, 1, 1);
            let content = gtk::Label::builder()
                .label(value)
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .hexpand(true)
                .build();
            grid.attach(&content, 1, row, 1, 1);
            row += 1;
        }
        if row == 0 {
            return;
        }
        let expander = gtk::Expander::builder()
            .label(gettext("Details"))
            .child(&grid)
            .build();
        self.body.append(&expander);
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
            .build();
        webview.set_size_request(-1, BODY_HEIGHT);
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
        webview.add_css_class("message-html");
        webview.load_html(&self.sandboxed_html(), None);
        self.inner.borrow_mut().webview = Some(webview.clone());
        self.body.append(&webview);
    }

    fn sandboxed_html(&self) -> String {
        let inner = self.inner.borrow();
        // Links take the system accent, the one place the message's own
        // styling is overridden -- and only where it set none itself.
        let style = format!("a:not([style]) {{ color: {}; }}", accent::accent_hex());
        mime::sandbox_html(
            inner.html.as_deref().unwrap_or(""),
            inner.should_load_remote_images,
            &style,
        )
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
        let heading = gtk::Label::builder()
            .label(gettext("Attachments"))
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        self.body.append(&heading);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        self.body.append(&list);
        let handlers = self.inner.borrow().handlers.clone();
        for attachment in attachments {
            let row = adw::ActionRow::builder()
                .title(&attachment.filename)
                .subtitle(glib::format_size(attachment.size() as u64).as_str())
                .activatable(true)
                .tooltip_text(gettext("Open with the default app"))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
            let open = attachment.clone();
            let open_handlers = handlers.clone();
            row.connect_activated(move |_| (open_handlers.on_open_attachment)(&open));

            let save_button = gtk::Button::builder()
                .icon_name("document-save-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Save Attachment"))
                .css_classes(["flat"])
                .build();
            let save = attachment.clone();
            let save_handlers = handlers.clone();
            save_button.connect_clicked(move |_| (save_handlers.on_save_attachment)(&save));
            row.add_suffix(&save_button);
            list.append(&row);
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

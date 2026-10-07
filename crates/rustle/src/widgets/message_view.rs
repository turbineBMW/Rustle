//! One message in the reading pane: its header and body,
//! rendered in a sandboxed WebKit view when it is HTML.

use crate::accent;
use crate::account_colors;
use crate::avatar_loader::AvatarLoader;
use crate::i18n::{self, gettext};
use crate::widgets::invitation;
use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use rustle_core::darkmode;
use rustle_core::invite::{Invitation, Response};
use rustle_core::mime::{self, ParsedMessage, Unsubscribe};
use rustle_core::models::{Account, Attachment, Email};
use rustle_core::pgp;
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
/// The rest of a message's conversation, each with where it's filed.
pub type RelatedHandler = Rc<dyn Fn(&Email) -> Vec<(Email, String)>>;
/// Answers an invitation; true once the reply is on its way.
pub type RespondHandler = Rc<dyn Fn(&Email, &Invitation, Response) -> bool>;
/// Called once the selected message has rendered.
pub type RenderedCallback = Box<dyn Fn()>;

/// What the window does for a view: fetch bodies, save/open attachments, and
/// unsubscribe (the second argument hides the banner once the list confirmed).
#[derive(Clone)]
pub struct Handlers {
    pub on_load: LoadHandler,
    pub on_save_attachment: Rc<dyn Fn(&Attachment)>,
    pub on_open_attachment: Rc<dyn Fn(&Attachment)>,
    pub on_unsubscribe: UnsubscribeHandler,
    pub on_respond: RespondHandler,
    /// The rest of a message's conversation, each with where it's filed.
    pub on_related: RelatedHandler,
    pub on_open_related: Rc<dyn Fn(&Email)>,
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
    text_label: Option<gtk::Label>,
    /// The reader's zoom: the WebKit zoom level, or the text's scale.
    zoom: f64,
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
                text_label: None,
                zoom: 1.0,
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

        match pgp::detect(&raw) {
            pgp::Protection::Encrypted | pgp::Protection::InlineEncrypted => {
                self.decrypt_then_render(raw);
            }
            pgp::Protection::Signed => {
                // Only the signed part is shown: the signature speaks for
                // nothing else, such as a part appended after it.
                let shown = pgp::signed_message(&raw).unwrap_or_else(|| raw.clone());
                self.render(shown, &raw, |_| None);
                self.verify(raw);
            }
            pgp::Protection::None => self.render(raw.clone(), &raw, |_| None),
        }
    }

    /// Decrypt on a worker (gpg may wait for a passphrase), then show what
    /// was inside -- held in memory only; the stored copy stays encrypted.
    fn decrypt_then_render(&self, raw: Vec<u8>) {
        let placeholder = gtk::Label::builder()
            .label(gettext("Decrypting…"))
            .margin_top(GUTTER)
            .css_classes(["dim-label"])
            .build();
        self.body.append(&placeholder);
        self.inner.borrow_mut().placeholder = Some(placeholder);
        let job = raw.clone();
        let this = self.clone();
        crate::workers::run(
            move || pgp::decrypt(&job).map_err(|error| error.to_string()),
            move |result: Result<pgp::Decrypted, String>| {
                {
                    let mut inner = this.inner.borrow_mut();
                    if inner.is_released {
                        return;
                    }
                    if let Some(placeholder) = inner.placeholder.take() {
                        this.body.remove(&placeholder);
                    }
                }
                match result {
                    Ok(decrypted) => {
                        let pgp::Decrypted {
                            raw: shown,
                            signature,
                            is_encrypted,
                        } = decrypted;
                        this.render(shown, &raw, |parsed| {
                            pgp_note(is_encrypted, signature.as_ref(), &parsed.from_header)
                        });
                    }
                    Err(message) => {
                        log::warn!("could not decrypt an OpenPGP message: {message}");
                        let note = (
                            i18n::format(
                                &gettext(
                                    "This message is encrypted, and couldn't be decrypted: {msg}",
                                ),
                                &[("msg", &message)],
                            ),
                            true,
                        );
                        this.render(raw.clone(), &raw, |_| Some(note));
                    }
                }
            },
        );
    }

    /// Check a signed message's signature on a worker, and say what it found.
    fn verify(&self, raw: Vec<u8>) {
        let this = self.clone();
        crate::workers::run(
            move || pgp::verify(&raw).map_err(|error| error.to_string()),
            move |result: Result<pgp::SignatureStatus, String>| {
                if this.inner.borrow().is_released {
                    return;
                }
                let from = this
                    .inner
                    .borrow()
                    .parsed
                    .as_ref()
                    .map(|parsed| parsed.from_header.clone())
                    .unwrap_or_default();
                let note = match result {
                    Ok(status) => pgp_note(false, Some(&status), &from),
                    Err(message) => {
                        log::warn!("could not check an OpenPGP signature: {message}");
                        Some((
                            i18n::format(
                                &gettext("This message is signed, but the signature couldn't be checked: {msg}"),
                                &[("msg", &message)],
                            ),
                            true,
                        ))
                    }
                };
                if let Some(note) = note {
                    this.body.prepend(&pgp_banner(&note));
                }
            },
        );
    }

    /// Show `shown` (the message, or what was decrypted out of it) while
    /// `raw` -- what's stored, saved and shown as source -- stays as it came.
    /// `note` reads the parsed message for the OpenPGP banner, if any.
    fn render(
        &self,
        shown: Vec<u8>,
        raw: &[u8],
        note: impl FnOnce(&ParsedMessage) -> Option<(String, bool)>,
    ) {
        let parsed = mime::parse_message(&shown);
        {
            let mut inner = self.inner.borrow_mut();
            inner.raw = Some(raw.to_vec());
            inner.parsed = Some(parsed.clone());
        }
        if let Some(note) = &note(&parsed) {
            self.body.append(&pgp_banner(note));
        }
        self.show_recipients(&parsed);
        self.show_related();
        self.show_authentication(&parsed);
        self.show_invitation(&parsed);
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

    fn show_invitation(&self, parsed: &ParsedMessage) {
        let Some(invitation) = &parsed.invitation else {
            return;
        };
        let (email, handlers) = {
            let inner = self.inner.borrow();
            (inner.email.clone(), inner.handlers.clone())
        };
        let on_save = handlers.on_save_attachment.clone();
        let answered = invitation.clone();
        let on_respond: Rc<dyn Fn(Response) -> bool> =
            Rc::new(move |response| (handlers.on_respond)(&email, &answered, response));
        let card = invitation::card(invitation, &parsed.subject, on_save, on_respond);
        card.set_margin_start(EDGE);
        card.set_margin_end(EDGE);
        card.set_margin_bottom(GUTTER);
        self.body.append(&card);
    }

    /// The rest of the conversation, folded away under a count: the list
    /// shows each message on its own, and this is where they meet.
    fn show_related(&self) {
        let (email, handlers) = {
            let inner = self.inner.borrow();
            (inner.email.clone(), inner.handlers.clone())
        };
        let related = (handlers.on_related)(&email);
        if related.is_empty() {
            return;
        }
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (message, place) in related {
            let when = i18n::time_label(&message.date);
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&message.subject))
                .subtitle(glib::markup_escape_text(&format!(
                    "{} · {when} · {place}",
                    message.sender
                )))
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let open = handlers.on_open_related.clone();
            row.connect_activated(move |_| open(&message));
            list.append(&row);
        }
        let count = list.observe_children().n_items() as u64;
        let expander = gtk::Expander::builder()
            .label(i18n::plural(
                "{n} related message",
                "{n} related messages",
                count,
                &[],
            ))
            .child(&list)
            .margin_start(EDGE)
            .margin_end(EDGE)
            .margin_bottom(GUTTER)
            .css_classes(["related-messages"])
            .build();
        self.body.append(&expander);
    }

    /// The provider couldn't verify the sender: the From line may be a lie.
    /// (A pass gets no badge: see `rustle_core::verify`.)
    fn show_authentication(&self, parsed: &ParsedMessage) {
        if parsed.authentication != rustle_core::verify::Verdict::Fail {
            return;
        }
        let banner = adw::Banner::builder()
            .title(gettext(
                "Your mail provider couldn't verify who sent this. Be careful with its links, attachments and requests.",
            ))
            .revealed(true)
            .css_classes(["unverified-banner"])
            .build();
        self.body.append(&banner);
    }

    fn show_unsubscribe(&self, target: Option<&Unsubscribe>) {
        let Some(target) = target else { return };
        let title = if target.from_body {
            gettext("This looks like a newsletter.")
        } else {
            gettext("You're subscribed to this mailing list.")
        };
        let banner = adw::Banner::builder()
            .title(title)
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
        let zoom = {
            let mut inner = self.inner.borrow_mut();
            inner.text_label = Some(label.clone());
            inner.zoom
        };
        set_label_scale(&label, zoom);
    }

    /// Zoom the body (not the header): 1.0 is the normal size.
    pub fn set_zoom(&self, zoom: f64) {
        let (webview, label) = {
            let mut inner = self.inner.borrow_mut();
            inner.zoom = zoom;
            (inner.webview.clone(), inner.text_label.clone())
        };
        if let Some(webview) = webview {
            webview.set_zoom_level(zoom);
        }
        if let Some(label) = label {
            set_label_scale(&label, zoom);
        }
    }

    /// Print the message, headers first, through the print dialog. It's laid
    /// out in a view of its own, never shown: the one on screen may be dark
    /// and has no headers in it.
    pub fn print(&self, parent: &gtk::Window) {
        let (parsed, allows_images) = {
            let inner = self.inner.borrow();
            let Some(parsed) = inner.parsed.clone() else {
                return;
            };
            (parsed, inner.should_load_remote_images)
        };
        let page = mime::sandbox_html(&mime::print_html(&parsed), allows_images, mime::PRINT_STYLE);
        let view = webkit::WebView::builder()
            .related_view(&ensure_anchor())
            .build();
        if let Some(settings) = WebViewExt::settings(&view) {
            settings.set_enable_javascript(false);
        }
        // Nothing may navigate this view but its own load.
        view.connect_decide_policy(|_, decision, _| {
            let is_link = decision
                .downcast_ref::<webkit::NavigationPolicyDecision>()
                .and_then(|navigation| navigation.navigation_action())
                .is_some_and(|action| {
                    action.navigation_type() == webkit::NavigationType::LinkClicked
                });
            if is_link {
                decision.ignore();
            }
            is_link
        });
        // The view has no parent to keep it, so this does until it's printed.
        let keep: Rc<RefCell<Option<webkit::WebView>>> = Rc::new(RefCell::new(Some(view.clone())));
        let parent = parent.clone();
        view.connect_load_changed(move |view, event| {
            if event != webkit::LoadEvent::Finished {
                return;
            }
            let operation = webkit::PrintOperation::new(view);
            let done = keep.clone();
            operation.connect_finished(move |_| {
                done.borrow_mut().take();
            });
            operation.connect_failed(|_, error| log::error!("could not print a message: {error}"));
            if operation.run_dialog(Some(&parent)) == webkit::PrintOperationResponse::Cancel {
                keep.borrow_mut().take();
            }
        });
        view.load_html(&page, None);
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
        // In dark mode the surface takes the scheme's canvas outright: an
        // Omarchy theme's isn't the stylesheet's fixed one.
        let canvas = is_dark()
            .then(|| gdk::RGBA::parse(accent::reader_scheme().canvas_css()).ok())
            .flatten();
        webview.set_background_color(&canvas.unwrap_or(gdk::RGBA::new(0.0, 0.0, 0.0, 0.0)));
        webview.add_css_class(if is_dark() {
            "message-html-dark"
        } else {
            "message-html"
        });
        webview.set_zoom_level(self.inner.borrow().zoom);
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
            let scheme = accent::reader_scheme();
            style.push_str(&format!(
                " :root {{ color-scheme: dark; }} body {{ background-color: {}; color: {}; }}",
                scheme.canvas_css(),
                scheme.text_css()
            ));
            std::borrow::Cow::Owned(darkmode::adapt_with(html, &scheme))
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

/// What OpenPGP found, and whether it's a warning: (text, is_warning).
/// None when there's nothing to say. `from_header` is the message's From,
/// which a good signature has to be by.
fn pgp_note(
    was_encrypted: bool,
    signature: Option<&pgp::SignatureStatus>,
    from_header: &str,
) -> Option<(String, bool)> {
    let (signed, is_warning) = match signature {
        None => (String::new(), false),
        Some(pgp::SignatureStatus::Good { signer, .. })
            if !pgp::signer_matches_sender(signer, from_header) =>
        {
            (
                i18n::format(
                    &gettext("Signed by {signer}, who isn't the sender."),
                    &[("signer", signer)],
                ),
                true,
            )
        }
        Some(pgp::SignatureStatus::Good {
            signer,
            is_trusted: true,
        }) => (
            i18n::format(&gettext("Signed by {signer}."), &[("signer", signer)]),
            false,
        ),
        Some(pgp::SignatureStatus::Good {
            signer,
            is_trusted: false,
        }) => (
            i18n::format(
                &gettext("Signed by {signer}, with a key you haven't confirmed is theirs."),
                &[("signer", signer)],
            ),
            false,
        ),
        Some(pgp::SignatureStatus::Bad { .. }) => (
            gettext("The signature doesn't match: the message changed after it was signed."),
            true,
        ),
        Some(pgp::SignatureStatus::UnknownKey { key_id }) => (
            i18n::format(
                &gettext("Signed with a key you don't have ({key})."),
                &[("key", key_id)],
            ),
            false,
        ),
    };
    let text = match (was_encrypted, signed.is_empty()) {
        (true, true) => gettext("Encrypted with OpenPGP."),
        (true, false) => format!("{} {signed}", gettext("Encrypted with OpenPGP.")),
        (false, true) => return None,
        (false, false) => signed,
    };
    Some((text, is_warning))
}

fn pgp_banner((text, is_warning): &(String, bool)) -> adw::Banner {
    let banner = adw::Banner::builder()
        .title(text.as_str())
        .revealed(true)
        .build();
    if *is_warning {
        banner.add_css_class("unverified-banner");
    }
    banner
}

/// Scale a plain-text body the way WebKit zooms an HTML one.
fn set_label_scale(label: &gtk::Label, zoom: f64) {
    let attributes = pango::AttrList::new();
    attributes.insert(pango::AttrFloat::new_scale(zoom));
    label.set_attributes(Some(&attributes));
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
    invitation::open_link(window.as_ref(), &uri);
    true
}

fn is_dark() -> bool {
    adw::StyleManager::default().is_dark()
}

#[cfg(test)]
#[path = "message_view_tests.rs"]
mod tests;

use super::*;
use std::time::{Duration, Instant};

// Run explicitly with a display and WebKit available:
// GDK_BACKEND=x11 GDK_SCALE=1 xvfb-run -a -s '-screen 0 1920x1080x24' \
//     cargo test -p rustle reader_layout -- --ignored
#[gtk::test]
#[ignore = "requires a GTK display and WebKit"]
fn reader_layout_keeps_header_compact_and_body_visible() {
    adw::init().unwrap();
    let source = gtk::gio::SettingsSchemaSource::from_directory(
        crate::config::BUILT_SCHEMA_DIR,
        None,
        false,
    )
    .unwrap();
    let schema = source.lookup(crate::config::APP_ID, false).unwrap();
    let settings = gtk::gio::Settings::new_full(
        &schema,
        Some(&gtk::gio::memory_settings_backend_new()),
        None,
    );
    settings
        .set_boolean(crate::settings::LOAD_SENDER_AVATARS, false)
        .unwrap();
    let avatars = AvatarLoader::new(settings);
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("../../resources/style.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Exercise the actual MessageView in the reader's scrolling container.
    // A long To line inside GtkGrid previously gave the header thousands of
    // pixels of minimum height and allocated zero height to WebKit.
    for width in [600, 1000, 1600] {
        for with_attachments in [false, true] {
            let email = Email {
                id: 1,
                folder_id: 1,
                server_id: None,
                sender: "Example Sender".into(),
                sender_address: "sender@example.com".into(),
                recipient: String::new(),
                recipient_address: String::new(),
                subject: "Layout regression".into(),
                preview: String::new(),
                date: "2026-09-10T10:48:00Z".into(),
                is_unread: false,
                is_starred: false,
                is_pinned: false,
                message_id: "layout@example.com".into(),
            };
            let recipients = (0..5)
                .map(|i| format!("Example Recipient {i} <recipient{i}@example.com>"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut raw = format!(
                "From: sender@example.com\r\nTo: {recipients}\r\n\
                 Cc: Copy Recipient <copy@example.com>\r\n\
                 MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=parts\r\n\r\n\
                 --parts\r\nContent-Type: text/html\r\n\r\n<p>Visible message body.</p>\r\n"
            );
            if with_attachments {
                for i in 0..4 {
                    raw.push_str(&format!(
                        "--parts\r\nContent-Type: text/plain\r\n\
                         Content-Disposition: attachment; filename=\"Long attachment filename {i}.txt\"\r\n\r\nattachment\r\n"
                    ));
                }
            }
            raw.push_str("--parts--\r\n");
            let handlers = Rc::new(Handlers {
                on_load: Rc::new(move |_, callback| callback(Some(raw.clone().into_bytes()), None)),
                on_open_attachment: Rc::new(|_| {}),
                on_save_attachment: Rc::new(|_| {}),
                on_unsubscribe: Rc::new(|_, _| {}),
            });
            let view = MessageView::new(email, handlers, Box::new(|| {}), true, &avatars, None);
            let messages = gtk::Box::new(gtk::Orientation::Vertical, 0);
            messages.append(view.widget());
            let scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&messages)
                .build();
            let window = gtk::Window::builder()
                .decorated(false)
                .default_width(width)
                .default_height(800)
                .child(&scroller)
                .build();
            window.present();
            let webview = view.inner.borrow().webview.clone().unwrap();
            let context = glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                while context.pending() {
                    context.iteration(false);
                }
                if window.is_mapped()
                    && window.width() == width
                    && window.height() == 800
                    && view.root.width() >= width - 20
                    && !webview.is_loading()
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "reader did not settle: window={}x{}, reader={}x{}, loading={}",
                    window.width(),
                    window.height(),
                    view.root.width(),
                    view.root.height(),
                    webview.is_loading()
                );
                std::thread::sleep(Duration::from_millis(10));
            }

            let header = view.root.first_child().unwrap();
            let body_bounds = webview.compute_bounds(&view.root).unwrap();
            assert!(
                header.height() < 200,
                "at width {width}, header consumed {} pixels",
                header.height()
            );
            assert!(body_bounds.y() < 400.0, "message starts below the viewport");
            assert!(
                webview.height() > 300,
                "message body lost its reading space"
            );
            assert!(
                view.root.width() <= width,
                "reader grew to {} pixels at requested width {width}",
                view.root.width()
            );

            if with_attachments {
                let section = view.body.first_child().unwrap();
                let cards = section.last_child().unwrap();
                let mut child = cards.first_child();
                let mut rows = Vec::new();
                while let Some(card) = child {
                    let bounds = card.compute_bounds(&view.root).unwrap();
                    assert_eq!(bounds.width() as i32, ATTACHMENT_WIDTH);
                    assert!(bounds.y() + bounds.height() <= body_bounds.y());
                    rows.push(bounds.y() as i32);
                    child = card.next_sibling();
                }
                assert_eq!(rows.len(), 4);
                rows.dedup();
                assert_eq!(rows.len(), if width == 1600 { 1 } else { 2 });
            }
            view.release();
            window.close();
        }
    }
    release_anchor();
}

//! The contenteditable WebKit page behind the composer and the signature
//! editor: one HTML template, the execCommand toolbar mapping, and the
//! message it posts back on every edit.

use crate::accent;
use crate::i18n::gettext;
use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use rustle_core::compose;
use std::collections::HashMap;
use webkit::prelude::*;

const EDITOR_PAGE: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
  html, body { margin: 0; height: 100%; }
  body {
    box-sizing: border-box;
    padding: 12px;
    font-family: __FAMILY__;
    font-size: __SIZE__pt;
    line-height: 1.5;
    color: #241f31;
    background: transparent;
    outline: none;
  }
  p { margin: 0; }
  img { max-width: 100%; height: auto; }
  a { color: __ACCENT__; }
  blockquote {
    margin: 0 0 0 2px;
    padding-left: 10px;
    border-left: 2px solid #c0bfbc;
    color: #5e5c64;
  }
  .signature { color: #5e5c64; }
  @media (prefers-color-scheme: dark) {
    body { color: #f6f5f4; }
    blockquote { border-left-color: #5e5c64; color: #c0bfbc; }
    .signature { color: #c0bfbc; }
  }
</style>
<script>
  // Lives in <head>: a script placed after </body> is hoisted into the
  // body by the parser and would leak into document.body.innerHTML,
  // i.e. into the saved signature or the sent message.
  document.addEventListener('DOMContentLoaded', function () {
    var COMMANDS = __COMMANDS__;

    function currentRange() {
      var selection = window.getSelection();
      return selection.rangeCount ? selection.getRangeAt(0) : null;
    }

    function linkAt(node) {
      while (node && node !== document.body) {
        if (node.nodeType === 1 && node.tagName === 'A') return node;
        node = node.parentNode;
      }
      return null;
    }

    function anchors() {
      return document.querySelectorAll('a');
    }

    function describeLink(a) {
      return {
        index: Array.prototype.indexOf.call(anchors(), a),
        href: a.getAttribute('href') || '',
        text: a.textContent
      };
    }

    function post() {
      var states = {};
      COMMANDS.forEach(function (name) {
        states[name] = document.queryCommandState(name);
      });
      var range = currentRange();
      var link = range ? linkAt(range.commonAncestorContainer) : null;
      window.webkit.messageHandlers.editor.postMessage(JSON.stringify({
        html: document.body.innerHTML,
        states: states,
        colors: {
          text: document.queryCommandValue('foreColor'),
          // Some engines only answer the older name for the same value.
          highlight: document.queryCommandValue('hiliteColor')
            || document.queryCommandValue('backColor')
        },
        selection: range ? range.toString() : '',
        link: link ? describeLink(link) : null
      }));
    }
    window.rustlePost = post;

    document.addEventListener('input', post);
    document.addEventListener('selectionchange', post);

    // --- colours ---------------------------------------------------------
    // Clearing a colour means applying the one the surroundings already
    // have: WebKit then strips the explicit colour from the selection and,
    // as the value matches the computed style, wraps nothing new. The text
    // keeps following the reader's theme instead of being pinned to
    // whatever the editor's default happened to be.
    function inheritedValue(property, setsIt) {
      var range = currentRange();
      var node = range ? range.commonAncestorContainer : document.body;
      if (node.nodeType !== 1) node = node.parentNode;
      while (node && node !== document.body && setsIt(node)) node = node.parentNode;
      return getComputedStyle(node || document.body)[property];
    }

    window.rustleSetColor = function (kind, value) {
      var command = kind === 'highlight' ? 'hiliteColor' : 'foreColor';
      if (value === null) {
        value = kind === 'highlight'
          ? inheritedValue('backgroundColor', function (el) {
              return el.style.backgroundColor !== '';
            })
          : inheritedValue('color', function (el) {
              return el.style.color !== '' || (el.tagName === 'FONT' && el.hasAttribute('color'));
            });
      }
      document.execCommand(command, false, value);
      post();
    };

    // --- links -----------------------------------------------------------
    function escapeHtml(text) {
      return String(text).replace(/[&<>"']/g, function (c) {
        return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
      });
    }

    function caretAfter(node) {
      var range = document.createRange();
      range.setStartAfter(node);
      range.collapse(true);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
    }

    // A link at the selection: over the selected text when that is the
    // text asked for, otherwise typed in fresh. Inside an existing link the
    // link is edited instead of nested.
    window.rustleInsertLink = function (href, text) {
      var range = currentRange();
      var existing = range ? linkAt(range.commonAncestorContainer) : null;
      if (existing) {
        window.rustleUpdateLink(describeLink(existing).index, href, text);
        return;
      }
      if (range && !range.collapsed && range.toString() === text) {
        document.execCommand('createLink', false, href);
      } else {
        document.execCommand('insertHTML', false,
          '<a href="' + escapeHtml(href) + '">' + escapeHtml(text || href) + '</a>');
      }
      post();
    };

    window.rustleUpdateLink = function (index, href, text) {
      var a = anchors()[index];
      if (!a) return;
      a.setAttribute('href', href);
      // Left alone when unchanged so formatting inside the link survives.
      if (a.textContent !== text) a.textContent = text;
      caretAfter(a);
      post();
    };

    window.rustleRemoveLink = function (index) {
      var a = anchors()[index];
      if (!a) return;
      var range = document.createRange();
      range.selectNodeContents(a);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      document.execCommand('unlink');
      post();
    };

    function reportLink(a) {
      var handler = window.webkit.messageHandlers.link;
      if (!handler) return;
      var info = describeLink(a);
      var rect = a.getBoundingClientRect();
      info.rect = { x: rect.left, y: rect.top, width: rect.width, height: rect.height };
      handler.postMessage(JSON.stringify(info));
    }

    // Pasted or dropped image files go in as data: URIs; the MIME builder
    // turns them into inline parts when the message is sent. A paste that
    // carries both an image and text/html (a browser copy of a picture)
    // takes the file: the HTML would reference a remote image the reader
    // blocks and the recipient may never load.
    function imageFiles(transfer) {
      var files = [];
      if (!transfer) return files;
      var items = transfer.items || [];
      for (var i = 0; i < items.length; i++) {
        if (items[i].kind === 'file' && /^image\//.test(items[i].type)) {
          var file = items[i].getAsFile();
          if (file) files.push(file);
        }
      }
      if (!files.length && transfer.files) {
        for (var j = 0; j < transfer.files.length; j++) {
          if (/^image\//.test(transfer.files[j].type)) files.push(transfer.files[j]);
        }
      }
      return files;
    }

    function insertImages(files) {
      files.forEach(function (file) {
        var reader = new FileReader();
        reader.onload = function () {
          document.execCommand('insertImage', false, reader.result);
          post();
        };
        reader.readAsDataURL(file);
      });
    }

    document.addEventListener('paste', function (event) {
      var files = imageFiles(event.clipboardData);
      if (!files.length) return;
      event.preventDefault();
      insertImages(files);
    });

    // WebKitGTK does not surface a clipboard picture through clipboardData;
    // it pastes it itself as a blob: image, which never loads here (only
    // data: escapes the images-off setting) and means nothing once sent.
    // Fold such images into data: URIs as soon as they appear.
    function inlineBlobImages() {
      var images = document.querySelectorAll('img[src^="blob:"]');
      Array.prototype.forEach.call(images, function (img) {
        var src = img.src;
        img.removeAttribute('src');
        fetch(src).then(function (response) {
          return response.blob();
        }).then(function (blob) {
          if (!/^image\//.test(blob.type)) throw new Error('not an image');
          var reader = new FileReader();
          reader.onload = function () {
            img.src = reader.result;
            post();
          };
          reader.readAsDataURL(blob);
        }).catch(function () {
          img.remove();
          post();
        });
      });
    }
    document.addEventListener('input', inlineBlobImages);

    // --- image sizing ----------------------------------------------------
    // Clicking a picture reports it, with what each preset width would cost,
    // to the "image" handler; the app answers with rustleResizeImage or
    // rustleRemoveImage. The full-size bytes are kept aside so a shrunken
    // picture can be restored.
    var PRESETS = [['small', 480], ['medium', 800], ['large', 1200]];
    var originals = new WeakMap();

    function bytesOf(dataUrl) {
      var comma = dataUrl.indexOf(',');
      if (dataUrl.indexOf(';base64,') < 0) return dataUrl.length - comma - 1;
      var body = dataUrl.length - comma - 1;
      var padding = dataUrl.endsWith('==') ? 2 : dataUrl.endsWith('=') ? 1 : 0;
      return Math.max(0, Math.floor(body * 3 / 4) - padding);
    }

    function originalOf(img) {
      if (!originals.has(img)) originals.set(img, img.src);
      return originals.get(img);
    }

    // Encode `source` scaled to `width`: PNG when anything is transparent,
    // otherwise whichever of PNG and JPEG comes out smaller.
    function encode(source, width) {
      var scale = width / source.naturalWidth;
      var canvas = document.createElement('canvas');
      canvas.width = width;
      canvas.height = Math.max(1, Math.round(source.naturalHeight * scale));
      var context = canvas.getContext('2d');
      context.drawImage(source, 0, 0, canvas.width, canvas.height);
      var pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
      for (var i = 3; i < pixels.length; i += 4) {
        if (pixels[i] < 255) return canvas.toDataURL('image/png');
      }
      var png = canvas.toDataURL('image/png');
      var jpeg = canvas.toDataURL('image/jpeg', 0.85);
      return jpeg.length < png.length ? jpeg : png;
    }

    function loadOriginal(img, then) {
      var source = new Image();
      source.onload = function () { then(source); };
      source.src = originalOf(img);
    }

    function reportImage(img) {
      var handler = window.webkit.messageHandlers.image;
      if (!handler || !/^data:image\//.test(img.src)) return;
      var index = Array.prototype.indexOf.call(document.images, img);
      loadOriginal(img, function (source) {
        var originalBytes = bytesOf(source.src);
        var options = [];
        PRESETS.forEach(function (preset) {
          if (preset[1] >= source.naturalWidth) return;
          var bytes = bytesOf(encode(source, preset[1]));
          // A re-encoded PNG can come out heavier than a well-compressed
          // original; a preset that saves nothing is not offered.
          if (bytes < originalBytes) options.push({ name: preset[0], width: preset[1], bytes: bytes });
        });
        options.push({ name: 'original', width: source.naturalWidth, bytes: originalBytes });
        var current = null;
        options.forEach(function (option) {
          if (option.width === img.naturalWidth) current = option.name;
        });
        var rect = img.getBoundingClientRect();
        handler.postMessage(JSON.stringify({
          index: index,
          rect: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
          bytes: bytesOf(img.src),
          current: current,
          options: options
        }));
      });
    }

    document.addEventListener('click', function (event) {
      var target = event.target;
      if (!target) return;
      if (target.tagName === 'IMG') {
        reportImage(target);
        return;
      }
      var a = linkAt(target);
      if (a) reportLink(a);
    });

    window.rustleResizeImage = function (index, name) {
      var img = document.images[index];
      if (!img) return;
      loadOriginal(img, function (source) {
        var preset = PRESETS.filter(function (p) { return p[0] === name; })[0];
        img.removeAttribute('width');
        img.removeAttribute('height');
        img.src = preset && preset[1] < source.naturalWidth ? encode(source, preset[1]) : source.src;
        post();
      });
    };

    window.rustleRemoveImage = function (index) {
      var img = document.images[index];
      if (!img) return;
      img.remove();
      post();
    };

    document.addEventListener('dragover', function (event) {
      if (imageFiles(event.dataTransfer).length) event.preventDefault();
    });

    document.addEventListener('drop', function (event) {
      var files = imageFiles(event.dataTransfer);
      if (!files.length) return;
      event.preventDefault();
      if (document.caretRangeFromPoint) {
        var range = document.caretRangeFromPoint(event.clientX, event.clientY);
        if (range) {
          var selection = window.getSelection();
          selection.removeAllRanges();
          selection.addRange(range);
        }
      }
      insertImages(files);
    });

    // <div> separators inherit no margin, so a sent message keeps the spacing
    // it was typed with even in clients that apply their own stylesheet.
    document.execCommand('defaultParagraphSeparator', false, 'div');
    document.body.focus();

    var first = document.body.firstChild;
    if (first) {
      var range = document.createRange();
      range.setStart(first, 0);
      range.collapse(true);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
    }
  });
</script>
</head>
<body contenteditable="true">__BODY__</body>
</html>
"#;

/// Toolbar toggle -> the execCommand it runs.
pub const FORMAT_COMMANDS: [(&str, &str); 6] = [
    ("bold", "bold"),
    ("italic", "italic"),
    ("underline", "underline"),
    ("strike", "strikeThrough"),
    ("bullets", "insertUnorderedList"),
    ("numbers", "insertOrderedList"),
];

/// The editor's default text colour in each scheme, as the page's stylesheet
/// sets it; text reading back as one of these carries no colour of its own.
const DEFAULT_TEXT_COLORS: [&str; 2] = ["#241f31", "#f6f5f4"];

#[derive(serde::Deserialize)]
pub struct EditorPayload {
    pub html: String,
    pub states: HashMap<String, bool>,
    /// The colours at the selection, as CSS values.
    #[serde(default)]
    pub colors: EditorColors,
    /// The selected text, empty for a caret.
    #[serde(default)]
    pub selection: String,
    /// The link the selection sits in, if any.
    #[serde(default)]
    pub link: Option<LinkInfo>,
}

#[derive(serde::Deserialize, Default)]
pub struct EditorColors {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub highlight: String,
}

impl EditorColors {
    /// The explicit text colour, `None` for the editor's own.
    pub fn text(&self) -> Option<gdk::RGBA> {
        let rgba = gdk::RGBA::parse(&self.text).ok()?;
        let hex = crate::accent::rgba_hex(&rgba);
        (!DEFAULT_TEXT_COLORS.contains(&hex.as_str())).then_some(rgba)
    }

    /// The highlight colour, `None` when there is none.
    pub fn highlight(&self) -> Option<gdk::RGBA> {
        let rgba = gdk::RGBA::parse(&self.highlight).ok()?;
        (rgba.alpha() > 0.0).then_some(rgba)
    }
}

/// Which colour a swatch sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorKind {
    Text,
    Highlight,
}

/// A link in the editor: reported on click, and with every payload when the
/// selection sits inside one.
#[derive(serde::Deserialize, Clone, Debug)]
pub struct LinkInfo {
    /// Position among the page's anchors, the handle the edit calls use.
    pub index: usize,
    pub href: String,
    pub text: String,
    /// Where it is drawn, in the WebView's coordinates; only on click.
    #[serde(default)]
    pub rect: Option<Rect>,
}

/// A picture the user clicked in the editor, as reported by the page.
#[derive(serde::Deserialize)]
pub struct ImageInfo {
    /// Position in `document.images`, the handle the resize calls use.
    pub index: usize,
    /// Where it is drawn, in the WebView's coordinates.
    pub rect: Rect,
    /// Its current encoded size.
    pub bytes: u64,
    /// The `ImageOption::name` matching its current width, if any.
    pub current: Option<String>,
    /// Preset widths narrower than the original, then the original itself.
    pub options: Vec<ImageOption>,
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    /// The rectangle a popover points at: rounded, never empty.
    pub fn to_gdk(&self) -> gdk::Rectangle {
        gdk::Rectangle::new(
            self.x.round() as i32,
            self.y.round() as i32,
            self.width.round().max(1.0) as i32,
            self.height.round().max(1.0) as i32,
        )
    }
}

#[derive(serde::Deserialize)]
pub struct ImageOption {
    /// `small`, `medium`, `large` or `original`.
    pub name: String,
    pub width: u32,
    pub bytes: u64,
}

/// Hear about pictures the user clicks in `webview`.
pub fn connect_image_clicked(webview: &webkit::WebView, on_click: impl Fn(ImageInfo) + 'static) {
    let Some(manager) = webview.user_content_manager() else {
        return;
    };
    manager.connect_script_message_received(Some("image"), move |_, value| {
        match serde_json::from_str::<ImageInfo>(&value.to_str()) {
            Ok(info) => on_click(info),
            Err(error) => log::warn!("editor sent an unreadable image report: {error}"),
        }
    });
}

/// Re-encode the `index`th picture at the preset `name` (`original` restores
/// the bytes it was pasted with).
pub fn resize_image(webview: &webkit::WebView, index: usize, name: &str) {
    let script = format!(
        "window.rustleResizeImage({index}, {})",
        serde_json::to_string(name).unwrap_or_default()
    );
    webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
}

/// Take the `index`th picture out of the message.
pub fn remove_image(webview: &webkit::WebView, index: usize) {
    let script = format!("window.rustleRemoveImage({index})");
    webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
}

/// Colour the selection, or with `None` hand it back to its surroundings.
pub fn set_color(webview: &webkit::WebView, kind: ColorKind, color: Option<&gdk::RGBA>) {
    let kind = match kind {
        ColorKind::Text => "text",
        ColorKind::Highlight => "highlight",
    };
    let value = color
        .map(|c| json(&accent::rgba_hex(c)))
        .unwrap_or_else(|| "null".into());
    let script = format!("window.rustleSetColor({}, {value})", json(kind));
    webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
}

/// Hear about links the user clicks in `webview`.
pub fn connect_link_clicked(webview: &webkit::WebView, on_click: impl Fn(LinkInfo) + 'static) {
    let Some(manager) = webview.user_content_manager() else {
        return;
    };
    manager.connect_script_message_received(
        Some("link"),
        move |_, value| match serde_json::from_str::<LinkInfo>(&value.to_str()) {
            Ok(info) => on_click(info),
            Err(error) => log::warn!("editor sent an unreadable link report: {error}"),
        },
    );
}

/// Link the selection to `href`, showing `text`.
pub fn insert_link(webview: &webkit::WebView, href: &str, text: &str) {
    let script = format!("window.rustleInsertLink({}, {})", json(href), json(text));
    webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
}

/// Point the `index`th link at `href`, showing `text`.
pub fn update_link(webview: &webkit::WebView, index: usize, href: &str, text: &str) {
    let script = format!(
        "window.rustleUpdateLink({index}, {}, {})",
        json(href),
        json(text)
    );
    webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
}

/// Turn the `index`th link back into plain text.
pub fn remove_link(webview: &webkit::WebView, index: usize) {
    let script = format!("window.rustleRemoveLink({index})");
    webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
}

fn json(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// `text` cut to `max` characters with an ellipsis, for a menu heading.
pub fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Ask for a link's text and address. `link` prefills an existing link to
/// edit; otherwise `text` is the selection the new link will cover.
/// `on_confirm` gets the text and the address, the latter made absolute.
pub fn link_dialog(
    parent: &impl IsA<gtk::Widget>,
    link: Option<&LinkInfo>,
    text: &str,
    on_confirm: impl Fn(String, String) + 'static,
) {
    let editing = link.is_some();
    let text_row = adw::EntryRow::builder()
        .title(gettext("Text"))
        .text(link.map(|l| l.text.as_str()).unwrap_or(text))
        .activates_default(true)
        .build();
    let url_row = adw::EntryRow::builder()
        .title(gettext("Link"))
        .text(link.map(|l| l.href.as_str()).unwrap_or_default())
        .input_purpose(gtk::InputPurpose::Url)
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&text_row);
    list.append(&url_row);

    let dialog = adw::AlertDialog::builder()
        .heading(if editing {
            gettext("Edit Link")
        } else {
            gettext("Insert Link")
        })
        .extra_child(&list)
        .build();
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response(
        "confirm",
        &if editing {
            gettext("Save")
        } else {
            gettext("Insert")
        },
    );
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("confirm"));
    dialog.set_response_enabled("confirm", !url_row.text().trim().is_empty());
    url_row.connect_changed(glib::clone!(
        #[weak]
        dialog,
        move |row| dialog.set_response_enabled("confirm", !row.text().trim().is_empty())
    ));
    dialog.connect_response(
        Some("confirm"),
        glib::clone!(
            #[weak]
            text_row,
            #[weak]
            url_row,
            move |_, _| {
                let href = compose::normalize_link(&url_row.text());
                if href.is_empty() {
                    return;
                }
                let text = text_row.text().trim().to_string();
                on_confirm(if text.is_empty() { href.clone() } else { text }, href);
            }
        ),
    );
    // Land in the address field when the text is already there.
    let has_text = editing || !text.is_empty();
    dialog.connect_map(move |_| {
        if has_text {
            url_row.grab_focus();
        } else {
            text_row.grab_focus();
        }
    });
    dialog.present(Some(parent));
}

/// A transparent editor loaded with `body_html`; `on_message` receives the
/// JSON `EditorPayload` after every edit or selection change. Transparent,
/// so the Adwaita "card" behind it supplies the background and the editor
/// tracks the theme without hardcoding.
pub fn build_webview(body_html: &str, on_message: impl Fn(&str) + 'static) -> webkit::WebView {
    let manager = webkit::UserContentManager::new();
    manager.register_script_message_handler("editor", None);
    manager.register_script_message_handler("image", None);
    manager.register_script_message_handler("link", None);
    manager.connect_script_message_received(Some("editor"), move |_, value| {
        on_message(&value.to_str())
    });
    let webview = webkit::WebView::builder()
        .user_content_manager(&manager)
        .hexpand(true)
        .vexpand(true)
        .build();
    if let Some(settings) = WebViewExt::settings(&webview) {
        settings.set_auto_load_images(false);
    }
    webview.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    let (family, size) = gtk_font();
    let commands: Vec<&str> = FORMAT_COMMANDS
        .iter()
        .map(|(_, command)| *command)
        .collect();
    let page = EDITOR_PAGE
        .replace("__FAMILY__", &family)
        .replace("__SIZE__", &size)
        .replace("__ACCENT__", &accent::accent_hex())
        .replace("__BODY__", body_html)
        .replace(
            "__COMMANDS__",
            &serde_json::to_string(&commands).unwrap_or_else(|_| "[]".into()),
        );
    webview.load_html(&page, None);
    webview
}

/// Run one `document.execCommand` in the editor.
pub fn exec(webview: &webkit::WebView, command: &str, argument: Option<&str>) {
    let argument = argument
        .map(|a| serde_json::to_string(a).unwrap_or_default())
        .unwrap_or_else(|| "null".into());
    let script = format!(
        "document.execCommand({}, false, {argument})",
        serde_json::to_string(command).unwrap_or_default()
    );
    webview.evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
}

/// Split GTK's "Cantarell 11" style font description into family and size.
pub fn gtk_font() -> (String, String) {
    let description = gtk::Settings::default()
        .and_then(|settings| settings.gtk_font_name())
        .map(|name| name.to_string())
        .unwrap_or_else(|| "Sans 11".to_string());
    match description.rsplit_once(' ') {
        Some((family, size)) if size.chars().all(|c| c.is_ascii_digit()) => {
            (family.to_string(), size.to_string())
        }
        _ => (description, "11".to_string()),
    }
}

//! The contenteditable WebKit page behind the composer and the signature
//! editor: one HTML template, the execCommand toolbar mapping, and the
//! message it posts back on every edit.

use crate::accent;
use gtk::gdk;
use gtk::gio;
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

    function post() {
      var states = {};
      COMMANDS.forEach(function (name) {
        states[name] = document.queryCommandState(name);
      });
      window.webkit.messageHandlers.editor.postMessage(JSON.stringify({
        html: document.body.innerHTML,
        states: states
      }));
    }

    document.addEventListener('input', post);
    document.addEventListener('selectionchange', post);

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
      var img = event.target;
      if (img && img.tagName === 'IMG') reportImage(img);
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

#[derive(serde::Deserialize)]
pub struct EditorPayload {
    pub html: String,
    pub states: HashMap<String, bool>,
}

/// A picture the user clicked in the editor, as reported by the page.
#[derive(serde::Deserialize)]
pub struct ImageInfo {
    /// Position in `document.images`, the handle the resize calls use.
    pub index: usize,
    /// Where it is drawn, in the WebView's coordinates.
    pub rect: ImageRect,
    /// Its current encoded size.
    pub bytes: u64,
    /// The `ImageOption::name` matching its current width, if any.
    pub current: Option<String>,
    /// Preset widths narrower than the original, then the original itself.
    pub options: Vec<ImageOption>,
}

#[derive(serde::Deserialize)]
pub struct ImageRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
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

/// A transparent editor loaded with `body_html`; `on_message` receives the
/// JSON `EditorPayload` after every edit or selection change. Transparent,
/// so the Adwaita "card" behind it supplies the background and the editor
/// tracks the theme without hardcoding.
pub fn build_webview(body_html: &str, on_message: impl Fn(&str) + 'static) -> webkit::WebView {
    let manager = webkit::UserContentManager::new();
    manager.register_script_message_handler("editor", None);
    manager.register_script_message_handler("image", None);
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

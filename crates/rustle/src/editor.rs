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

/// A transparent editor loaded with `body_html`; `on_message` receives the
/// JSON `EditorPayload` after every edit or selection change. Transparent,
/// so the Adwaita "card" behind it supplies the background and the editor
/// tracks the theme without hardcoding.
pub fn build_webview(body_html: &str, on_message: impl Fn(&str) + 'static) -> webkit::WebView {
    let manager = webkit::UserContentManager::new();
    manager.register_script_message_handler("editor", None);
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

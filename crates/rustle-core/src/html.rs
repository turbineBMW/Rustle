//! The little HTML handling the composer and previews need: escaping text
//! into markup, and flattening markup back into readable text. Not a
//! renderer -- WebKit does that -- just enough to make a preview line and a
//! plain-text alternative.

const BLOCK_TAGS: &[&str] = &[
    "p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// Escape text for inclusion in HTML, turning newlines into `<br>`.
pub fn to_html(text: &str) -> String {
    escape(text).replace('\n', "<br>")
}

/// Escape the characters that matter in text and attribute values.
pub fn escape(text: &str) -> String {
    html_escape::encode_double_quoted_attribute(text).into_owned()
}

/// Strip tags, drop scripts and styles, and collapse blank lines.
pub fn html_to_text(html: &str) -> String {
    let mut parts = String::with_capacity(html.len());
    let mut rest = html;
    let mut skip_depth = 0usize;

    while let Some(open) = rest.find('<') {
        let text = &rest[..open];
        if skip_depth == 0 {
            parts.push_str(text);
        }
        let after = &rest[open + 1..];
        // A "<" not starting a tag ("1 < 2") is text, as is one with no
        // closing bracket.
        let is_tag_start = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        let Some(close) = after.find('>').filter(|_| is_tag_start) else {
            if skip_depth == 0 {
                parts.push('<');
            }
            rest = after;
            continue;
        };
        let tag = &after[..close];
        rest = &after[close + 1..];
        if tag.starts_with("!--") {
            // Comments may contain ">" -- find the real end.
            if let Some(end) = after.find("-->") {
                rest = &after[end + 3..];
            }
            continue;
        }
        let is_closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match (name.as_str(), is_closing) {
            ("script" | "style", false) => skip_depth += 1,
            ("script" | "style", true) => skip_depth = skip_depth.saturating_sub(1),
            ("li", false) => parts.push_str("\n- "),
            (tag, false) if BLOCK_TAGS.contains(&tag) => parts.push('\n'),
            ("p" | "blockquote", true) => parts.push('\n'),
            _ => {}
        }
    }
    if skip_depth == 0 {
        parts.push_str(rest);
    }

    let decoded = html_escape::decode_html_entities(&parts);
    let mut out: Vec<&str> = Vec::new();
    for line in decoded.lines().map(str::trim) {
        if !line.is_empty() || out.last().is_some_and(|last| !last.is_empty()) {
            out.push(line);
        }
    }
    out.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_markup() {
        let html = "<div>Hello<br>there</div><style>p{}</style><p>a &amp; b</p><ul><li>x</li><li>y</li></ul>";
        assert_eq!(html_to_text(html), "Hello\nthere\na & b\n\n- x\n- y");
        assert_eq!(html_to_text("1 < 2 and <b>bold</b>"), "1 < 2 and bold");
        assert_eq!(html_to_text("<!-- a > b -->text"), "text");
    }

    #[test]
    fn escapes() {
        assert_eq!(to_html("a<b>\n\"c\""), "a&lt;b&gt;<br>&quot;c&quot;");
    }
}

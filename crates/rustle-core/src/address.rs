//! RFC 5322 address lists, the way Python's `email.utils.getaddresses` read
//! them: tolerant of unquoted names, comments and stray commas, because the
//! headers this is fed come from every mail client ever written.

/// One mailbox: a display name (possibly empty) and an address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mailbox {
    pub name: String,
    pub address: String,
}

impl Mailbox {
    /// `Name <addr>`, or the bare address when there is no name.
    pub fn display(&self) -> String {
        if self.name.is_empty() {
            self.address.clone()
        } else if self.address.is_empty() {
            self.name.clone()
        } else {
            format!("{} <{}>", self.name, self.address)
        }
    }
}

/// Every mailbox in a header value such as `"Ada" <ada@x>, bob@y`.
pub fn parse_list(header: &str) -> Vec<Mailbox> {
    let mut out = Vec::new();
    for group in split_top_level(header, ',') {
        let group = group.trim();
        if group.is_empty() {
            continue;
        }
        if let Some(mailbox) = parse_one(group) {
            out.push(mailbox);
        }
    }
    out
}

/// The first mailbox of a header, like Python's `parseaddr`: a header naming
/// several yields the first rather than nothing.
pub fn parse_first(header: &str) -> Option<Mailbox> {
    parse_list(header).into_iter().next()
}

/// The display name of the first mailbox, falling back to its address and
/// then to the raw text -- what a list row shows for a sender.
pub fn display_name(header: &str) -> String {
    match parse_first(header) {
        Some(mailbox) if !mailbox.name.is_empty() => mailbox.name,
        Some(mailbox) if !mailbox.address.is_empty() => mailbox.address,
        _ => header.trim().to_string(),
    }
}

/// The lowercased address of the first mailbox, "" if unparseable.
pub fn first_address(header: &str) -> String {
    parse_first(header)
        .map(|mailbox| mailbox.address.to_lowercase())
        .unwrap_or_default()
}

fn parse_one(text: &str) -> Option<Mailbox> {
    let text = strip_comments(text);
    let text = text.trim();
    if let Some(open) = text.rfind('<') {
        let close = text[open..]
            .find('>')
            .map(|i| open + i)
            .unwrap_or(text.len());
        let address = text[open + 1..close].trim().to_string();
        let name = unquote(text[..open].trim());
        return Some(Mailbox { name, address });
    }
    let bare = unquote(text);
    if bare.is_empty() {
        return None;
    }
    if bare.contains('@') {
        Some(Mailbox {
            name: String::new(),
            address: bare,
        })
    } else {
        Some(Mailbox {
            name: bare,
            address: String::new(),
        })
    }
}

fn unquote(text: &str) -> String {
    let text = text.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        let mut out = String::with_capacity(text.len());
        let mut chars = text[1..text.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        text.to_string()
    }
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            if depth == 0 {
                out.push(c);
            }
            escaped = false;
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                in_quotes = !in_quotes;
                out.push(c);
            }
            '(' if !in_quotes => depth += 1,
            ')' if !in_quotes && depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Split on a separator that sits outside quotes and angle brackets.
fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut in_brackets = false;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => in_brackets = true,
            '>' if !in_quotes => in_brackets = false,
            c if c == separator && !in_quotes && !in_brackets => {
                parts.push(&text[start..index]);
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_addresses() {
        let list = parse_list(
            r#""Lovelace, Ada" <ada@example.com>, bob@example.org, Carol (work) <carol@x.y>"#,
        );
        assert_eq!(list.len(), 3);
        assert_eq!(
            list[0],
            Mailbox {
                name: "Lovelace, Ada".into(),
                address: "ada@example.com".into()
            }
        );
        assert_eq!(
            list[1],
            Mailbox {
                name: String::new(),
                address: "bob@example.org".into()
            }
        );
        assert_eq!(
            list[2],
            Mailbox {
                name: "Carol".into(),
                address: "carol@x.y".into()
            }
        );
    }

    #[test]
    fn display_helpers() {
        assert_eq!(
            display_name("Ada Lovelace <ada@example.com>"),
            "Ada Lovelace"
        );
        assert_eq!(display_name("ada@example.com"), "ada@example.com");
        assert_eq!(first_address("Ada <ADA@Example.com>"), "ada@example.com");
        assert_eq!(first_address("nonsense"), "");
        assert!(parse_first("").is_none());
        assert_eq!(parse_first("a@b, c@d").unwrap().address, "a@b");
    }
}

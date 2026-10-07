//! Which messages belong to the same conversation, without merging them in
//! the list: the reader shows a message's relatives beside it, and grouping
//! is an option. Two keys say it:
//!
//! - the *root*: the first Message-ID in References (else In-Reply-To, else
//!   the message's own), which every standards-following reply carries;
//! - Outlook's *Thread-Index*: its first 22 bytes name the conversation, and
//!   Exchange keeps them on internal replies that carry no References.
//!
//! Messages are related when either key matches.

use std::collections::HashMap;
use std::sync::LazyLock;

/// A message's conversation keys. Either may be empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadKeys {
    pub root: String,
    pub outlook: String,
}

/// Work out a message's keys from its headers, as raw header text.
pub fn keys(
    message_id: &str,
    references: &str,
    in_reply_to: &str,
    thread_index: &str,
) -> ThreadKeys {
    let root = first_id(references)
        .or_else(|| first_id(in_reply_to))
        .or_else(|| first_id(message_id))
        .unwrap_or_default();
    ThreadKeys {
        root,
        outlook: outlook_key(thread_index),
    }
}

/// The keys of a whole downloaded message.
pub fn keys_from_raw(raw: &[u8]) -> ThreadKeys {
    let Some(message) = mail_parser::MessageParser::default().parse_headers(raw) else {
        return ThreadKeys::default();
    };
    let header = |name: &str| message.header_raw(name).unwrap_or("").trim().to_string();
    keys(
        &header("Message-ID"),
        &header("References"),
        &header("In-Reply-To"),
        &header("Thread-Index"),
    )
}

/// The first `<id>` in a header, lowercased so spellings agree.
fn first_id(header: &str) -> Option<String> {
    static ID: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"<[^<>\s]+>").expect("a valid pattern"));
    ID.find(header).map(|found| found.as_str().to_lowercase())
}

/// The conversation part of a Thread-Index: its first 22 bytes (a FILETIME
/// and a GUID), as hex. Each reply only appends to them.
fn outlook_key(thread_index: &str) -> String {
    use base64::Engine;
    let compact: String = thread_index
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    match base64::engine::general_purpose::STANDARD.decode(compact) {
        Ok(bytes) if bytes.len() >= 22 => bytes[..22].iter().map(|b| format!("{b:02x}")).collect(),
        _ => String::new(),
    }
}

/// Group messages into conversations: each id maps to its group's
/// representative. Messages sharing a root, or an Outlook key, share a
/// group -- and so does anything chained through either.
pub fn group(items: &[(i64, &str, &str)]) -> HashMap<i64, i64> {
    let mut parent: HashMap<i64, i64> = items.iter().map(|(id, ..)| (*id, *id)).collect();
    fn find(parent: &mut HashMap<i64, i64>, id: i64) -> i64 {
        let mut root = id;
        while parent[&root] != root {
            root = parent[&root];
        }
        let mut node = id;
        while parent[&node] != root {
            let next = parent[&node];
            parent.insert(node, root);
            node = next;
        }
        root
    }
    let mut first_by_key: HashMap<String, i64> = HashMap::new();
    for (id, root, outlook) in items {
        for key in [format!("r:{root}"), format!("o:{outlook}")] {
            if key.len() <= 2 {
                continue; // an empty key joins nothing
            }
            match first_by_key.get(&key) {
                Some(&other) => {
                    let (a, b) = (find(&mut parent, *id), find(&mut parent, other));
                    if a != b {
                        parent.insert(a, b);
                    }
                }
                None => {
                    first_by_key.insert(key, *id);
                }
            }
        }
    }
    let ids: Vec<i64> = items.iter().map(|(id, ..)| *id).collect();
    ids.into_iter()
        .map(|id| (id, find(&mut parent, id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_come_from_references_then_in_reply_to() {
        let keys = keys("<c@x>", "<A@x> <b@x>", "<b@x>", "");
        assert_eq!(keys.root, "<a@x>");
        assert_eq!(super::keys("<c@x>", "", "<B@x>", "").root, "<b@x>");
        assert_eq!(super::keys("<c@x>", "", "", "").root, "<c@x>");
        assert_eq!(super::keys("", "", "", ""), ThreadKeys::default());
    }

    #[test]
    fn outlook_keys_ignore_the_reply_blocks() {
        // 22 bytes of conversation, then a 5-byte reply block.
        let first = "AdkGhyT5uQ2fZ1F0TkK3B4hJcQmAkw==";
        let reply = "AdkGhyT5uQ2fZ1F0TkK3B4hJcQmAkwABAgME";
        let key = super::keys("", "", "", first).outlook;
        assert_eq!(key.len(), 44);
        assert_eq!(super::keys("", "", "", reply).outlook, key);
        assert_eq!(super::keys("", "", "", "not base64!").outlook, "");
    }

    #[test]
    fn groups_chain_through_either_key() {
        let groups = group(&[
            (1, "<a@x>", "aa"),
            (2, "<a@x>", ""),
            (3, "<b@x>", "aa"), // an Outlook reply without References
            (4, "<c@x>", ""),
            (5, "", ""),
        ]);
        assert_eq!(groups[&1], groups[&2]);
        assert_eq!(groups[&1], groups[&3]);
        assert_ne!(groups[&1], groups[&4]);
        assert_ne!(groups[&4], groups[&5]);
    }
}

//! Conversation threading: union-find over Message-ID / In-Reply-To /
//! References, with a normalized-subject fallback.

use crate::models::Email;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Stored, never translated. `normalize_subject` blanks it so that unrelated
/// subject-less messages don't all thread into one conversation -- which means
/// whatever writes it has to use this exact string.
pub const NO_SUBJECT: &str = "(no subject)";

static REPLY_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(re|fwd|fw)\s*:\s*").unwrap());

/// Map each email id to a stable conversation id (the smallest id in its group).
pub fn group(emails: &[Email]) -> HashMap<i64, i64> {
    let mut parents: HashMap<String, String> = HashMap::new();

    fn find(parents: &mut HashMap<String, String>, token: &str) -> String {
        let mut current = token.to_string();
        parents
            .entry(current.clone())
            .or_insert_with(|| current.clone());
        loop {
            let parent = parents[&current].clone();
            if parent == current {
                return current;
            }
            // path halving
            let grandparent = parents[&parent].clone();
            parents.insert(current.clone(), grandparent.clone());
            current = grandparent;
        }
    }

    fn union(parents: &mut HashMap<String, String>, left: &str, right: &str) {
        let left_root = find(parents, left);
        let right_root = find(parents, right);
        parents.insert(left_root, right_root);
    }

    // Every email gets a token: its Message-ID, or a synthetic one so a message
    // with no Message-ID still stands on its own.
    let mut tokens: HashMap<i64, String> = HashMap::new();
    for mail in emails {
        let trimmed = mail.message_id.trim();
        let token = if trimmed.is_empty() {
            format!("eid:{}", mail.id)
        } else {
            trimmed.to_string()
        };
        find(&mut parents, &token);
        tokens.insert(mail.id, token);
    }

    // Reference links: join a message to its parent and ancestors.
    for mail in emails {
        let token = tokens[&mail.id].clone();
        let reply_to = mail.in_reply_to.trim();
        if !reply_to.is_empty() {
            union(&mut parents, &token, reply_to);
        }
        for reference in mail.references.split_whitespace() {
            union(&mut parents, &token, reference);
        }
    }

    // Fallback: messages sharing a normalized subject join the same thread.
    let mut by_subject: HashMap<String, String> = HashMap::new();
    for mail in emails {
        let subject = normalize_subject(&mail.subject);
        if subject.is_empty() {
            continue;
        }
        match by_subject.get(&subject) {
            Some(existing) => {
                let existing = existing.clone();
                union(&mut parents, &tokens[&mail.id], &existing);
            }
            None => {
                by_subject.insert(subject, tokens[&mail.id].clone());
            }
        }
    }

    // Each group's conversation id is the smallest email id it contains.
    let mut root_min_id: HashMap<String, i64> = HashMap::new();
    for mail in emails {
        let root = find(&mut parents, &tokens[&mail.id]);
        let entry = root_min_id.entry(root).or_insert(mail.id);
        if mail.id < *entry {
            *entry = mail.id;
        }
    }

    emails
        .iter()
        .map(|mail| {
            let root = find(&mut parents, &tokens[&mail.id]);
            (mail.id, root_min_id[&root])
        })
        .collect()
}

fn normalize_subject(subject: &str) -> String {
    let mut text = subject.trim().to_string();
    if text.eq_ignore_ascii_case(NO_SUBJECT) {
        return String::new();
    }
    loop {
        let stripped = REPLY_PREFIX.replace(&text, "").into_owned();
        if stripped == text {
            break;
        }
        text = stripped;
    }
    text.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(
        id: i64,
        subject: &str,
        message_id: &str,
        in_reply_to: &str,
        references: &str,
    ) -> Email {
        Email {
            id,
            folder_id: 1,
            server_id: Some(id.to_string()),
            sender: String::new(),
            sender_address: String::new(),
            recipient: String::new(),
            recipient_address: String::new(),
            subject: subject.into(),
            preview: String::new(),
            date: String::new(),
            is_unread: false,
            is_starred: false,
            is_pinned: false,
            message_id: message_id.into(),
            in_reply_to: in_reply_to.into(),
            references: references.into(),
            conversation_id: None,
        }
    }

    #[test]
    fn links_by_reference_and_subject() {
        let emails = vec![
            email(1, "Hello", "<a>", "", ""),
            email(2, "Re: Hello", "<b>", "<a>", "<a>"),
            email(3, "Other", "<c>", "", ""),
            email(4, "RE: re: other", "<d>", "", ""),
            email(5, "(no subject)", "", "", ""),
            email(6, "(no subject)", "", "", ""),
        ];
        let groups = group(&emails);
        assert_eq!(groups[&1], 1);
        assert_eq!(groups[&2], 1);
        assert_eq!(groups[&3], 3);
        assert_eq!(groups[&4], 3);
        assert_eq!(groups[&5], 5);
        assert_eq!(groups[&6], 6);
    }
}

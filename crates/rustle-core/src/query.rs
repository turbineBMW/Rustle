//! What's typed in the search box, read into the same `SearchFilter` Smart
//! Search builds: plain words match anywhere, and a few operators narrow by
//! field -- `from:ada`, `to:bob`, `subject:"q3 plan"`, `is:unread`,
//! `is:starred`, `after:2026-09-01`, `before:2026-10-01`. A quoted phrase
//! stays one term. Anything that isn't a known operator is just words, so a
//! stray colon ("re: lunch") never turns a search into nothing.

use crate::assistant::SearchFilter;
use chrono::NaiveDate;

pub fn parse(text: &str) -> SearchFilter {
    let mut filter = SearchFilter::default();
    for token in tokens(text) {
        if !apply_operator(&mut filter, &token) {
            let word = token.replace('"', "");
            if !word.trim().is_empty() {
                filter.words.push(word.trim().to_string());
            }
        }
    }
    filter
}

/// Whitespace-separated, except inside double quotes: `from:"Ada L" plan`
/// is two tokens.
fn tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in text.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Fill in the field `token` names, if it is an operator with a usable value.
fn apply_operator(filter: &mut SearchFilter, token: &str) -> bool {
    let Some((key, value)) = token.split_once(':') else {
        return false;
    };
    let value = value.trim_matches('"').trim();
    if value.is_empty() {
        return false;
    }
    let value = value.to_string();
    match key.to_lowercase().as_str() {
        "from" => filter.from = Some(value),
        "to" => filter.to = Some(value),
        "subject" => filter.subject = Some(value),
        "is" => match value.to_lowercase().as_str() {
            "unread" => filter.unread = Some(true),
            "read" => filter.unread = Some(false),
            "starred" | "flagged" => filter.starred = Some(true),
            "unstarred" | "unflagged" => filter.starred = Some(false),
            _ => return false,
        },
        "after" | "since" if is_day(&value) => filter.after = Some(value),
        "before" if is_day(&value) => filter.before = Some(value),
        _ => return false,
    }
    true
}

fn is_day(value: &str) -> bool {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_operators_and_words() {
        let filter = parse(
            r#"from:ada to:"Bob B" subject:plan is:unread after:2026-09-01 budget "q3 numbers""#,
        );
        assert_eq!(filter.from.as_deref(), Some("ada"));
        assert_eq!(filter.to.as_deref(), Some("Bob B"));
        assert_eq!(filter.subject.as_deref(), Some("plan"));
        assert_eq!(filter.unread, Some(true));
        assert_eq!(filter.after.as_deref(), Some("2026-09-01"));
        assert_eq!(filter.words, ["budget", "q3 numbers"]);
    }

    #[test]
    fn unknown_operators_and_bad_values_are_words() {
        let filter = parse("re: lunch cc:x before:tomorrow is:shiny");
        assert_eq!(
            filter.words,
            ["re:", "lunch", "cc:x", "before:tomorrow", "is:shiny"]
        );
        assert!(filter.before.is_none() && filter.unread.is_none());
    }

    #[test]
    fn plain_text_is_all_words() {
        let filter = parse("  invoice  pdf ");
        assert_eq!(filter.words, ["invoice", "pdf"]);
        assert!(parse("").is_empty());
        assert_eq!(parse("is:read is:starred").starred, Some(true));
    }
}

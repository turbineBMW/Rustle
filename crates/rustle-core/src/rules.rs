//! Rules for new mail: when a message arriving in an inbox has some text in
//! its sender, recipients, subject or body, mark it read, star it, and/or
//! move it to a folder. Local, run as mail is synced -- not Sieve, so they
//! act only while Rustle runs.

use crate::models::MessageHeader;

/// Where a rule looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    From,
    To,
    Subject,
    Body,
    Anywhere,
}

impl Field {
    pub const ALL: [Field; 5] = [
        Field::From,
        Field::To,
        Field::Subject,
        Field::Body,
        Field::Anywhere,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Field::From => "from",
            Field::To => "to",
            Field::Subject => "subject",
            Field::Body => "body",
            Field::Anywhere => "anywhere",
        }
    }

    pub fn parse(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|field| field.id() == id)
            .unwrap_or(Field::Anywhere)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// 0 for one not saved yet.
    pub id: i64,
    pub account_id: i64,
    pub field: Field,
    /// What to look for: comma-separated alternatives, any of which matches,
    /// ignoring case.
    pub pattern: String,
    pub mark_read: bool,
    pub star: bool,
    /// The folder (of the same account) to move it to.
    pub move_to: Option<i64>,
    pub enabled: bool,
}

impl Rule {
    /// The pattern's alternatives, trimmed, lowercase, none empty.
    fn needles(&self) -> Vec<String> {
        self.pattern
            .split(',')
            .map(|part| part.trim().to_lowercase())
            .filter(|part| !part.is_empty())
            .collect()
    }

    pub fn matches(&self, header: &MessageHeader) -> bool {
        let needles = self.needles();
        if !self.enabled || needles.is_empty() {
            return false;
        }
        let from = format!("{} {}", header.sender, header.sender_address);
        let to = format!(
            "{} {} {}",
            header.recipient, header.recipient_address, header.recipients
        );
        let body = if header.body_text.is_empty() {
            &header.preview
        } else {
            &header.body_text
        };
        let haystacks: Vec<&str> = match self.field {
            Field::From => vec![&from],
            Field::To => vec![&to],
            Field::Subject => vec![&header.subject],
            Field::Body => vec![body],
            Field::Anywhere => vec![&from, &to, &header.subject, body],
        };
        let haystacks: Vec<String> = haystacks.iter().map(|text| text.to_lowercase()).collect();
        needles
            .iter()
            .any(|needle| haystacks.iter().any(|text| text.contains(needle.as_str())))
    }

    /// Whether it does anything at all.
    pub fn has_action(&self) -> bool {
        self.mark_read || self.star || self.move_to.is_some()
    }
}

/// What the rules decided for one message: the first rule with a move
/// decides where it goes; marks from every matching rule add up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    pub mark_read: bool,
    pub star: bool,
    pub move_to: Option<i64>,
}

pub fn apply(rules: &[Rule], header: &MessageHeader) -> Verdict {
    let mut verdict = Verdict::default();
    for rule in rules.iter().filter(|rule| rule.matches(header)) {
        verdict.mark_read |= rule.mark_read;
        verdict.star |= rule.star;
        if verdict.move_to.is_none() {
            verdict.move_to = rule.move_to;
        }
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(field: Field, pattern: &str) -> Rule {
        Rule {
            id: 1,
            account_id: 1,
            field,
            pattern: pattern.into(),
            mark_read: false,
            star: false,
            move_to: None,
            enabled: true,
        }
    }

    fn header() -> MessageHeader {
        MessageHeader {
            sender: "GitHub".into(),
            sender_address: "notifications@github.com".into(),
            recipients: "Team <team@x.y>".into(),
            subject: "[Rustle] PR #31 merged".into(),
            preview: "short".into(),
            body_text: "Merged by Brandon into main".into(),
            ..MessageHeader::default()
        }
    }

    #[test]
    fn matches_any_alternative_in_its_field() {
        let header = header();
        assert!(rule(Field::From, "github.com").matches(&header));
        assert!(rule(Field::From, "gitlab.com, GITHUB").matches(&header));
        assert!(!rule(Field::Subject, "github").matches(&header));
        assert!(rule(Field::To, "team@").matches(&header));
        assert!(rule(Field::Body, "into main").matches(&header));
        assert!(rule(Field::Anywhere, "pr #31").matches(&header));
        assert!(
            !rule(Field::Anywhere, " , ").matches(&header),
            "nothing to look for"
        );
        let off = Rule {
            enabled: false,
            ..rule(Field::From, "github")
        };
        assert!(!off.matches(&header));
    }

    #[test]
    fn the_first_move_wins_and_marks_add_up() {
        let rules = [
            Rule {
                mark_read: true,
                ..rule(Field::From, "github")
            },
            Rule {
                move_to: Some(7),
                ..rule(Field::Subject, "rustle")
            },
            Rule {
                move_to: Some(9),
                star: true,
                ..rule(Field::Anywhere, "merged")
            },
        ];
        assert_eq!(
            apply(&rules, &header()),
            Verdict {
                mark_read: true,
                star: true,
                move_to: Some(7),
            }
        );
        assert_eq!(apply(&rules, &MessageHeader::default()), Verdict::default());
    }
}

//! Message dates: parsed once at sync time into an ISO timestamp, and turned
//! into a short relative label whenever a row is drawn.

use chrono::{DateTime, Datelike, Local, NaiveDate};

/// "Wed, 16 Jul 2026 10:00:00 +0000" -> "2026-07-16T10:00:00+00:00". Stored as
/// a timestamp rather than a label because the label is relative to today,
/// and the column is written once at sync time and never revisited. An
/// unparseable header is stored verbatim.
pub fn to_iso(rfc822: &str) -> String {
    match mail_parser::DateTime::parse_rfc822(rfc822) {
        Some(parsed) => parsed.to_rfc3339(),
        None => rfc822.to_string(),
    }
}

/// The current local time as an ISO timestamp, for locally saved copies.
pub fn now_iso() -> String {
    Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// "Jul 16, 2026 10:00", the long form the reader's Details section shows.
pub fn long_label(rfc822: &str) -> String {
    match parse(rfc822) {
        Some(moment) => moment.format("%b %d, %Y %H:%M").to_string(),
        None => rfc822.to_string(),
    }
}

/// How a stored timestamp reads next to a row. The GTK layer translates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelativeLabel {
    /// Sent today: just the time, "10:00".
    Today(String),
    Yesterday,
    /// Within the last week: the weekday's short name, "Wed".
    Weekday(String),
    /// Beyond that: "Jul 16".
    Date(String),
    /// Neither a timestamp we wrote nor a date we can read: shown verbatim.
    Raw(String),
}

pub fn relative_label(value: &str) -> RelativeLabel {
    relative_label_on(value, Local::now().date_naive())
}

fn relative_label_on(value: &str, today: NaiveDate) -> RelativeLabel {
    let Some(moment) = parse(value) else {
        return RelativeLabel::Raw(value.to_string());
    };
    let days = today.signed_duration_since(moment.date_naive()).num_days();
    match days {
        0 => RelativeLabel::Today(moment.format("%H:%M").to_string()),
        1 => RelativeLabel::Yesterday,
        2..=6 => RelativeLabel::Weekday(moment.format("%a").to_string()),
        _ => RelativeLabel::Date(moment.format("%b %d").to_string()),
    }
}

/// Parses either our own ISO form or an RFC 2822 header, in local time.
fn parse(value: &str) -> Option<DateTime<Local>> {
    if let Ok(moment) = DateTime::parse_from_rfc3339(value) {
        return Some(moment.with_timezone(&Local));
    }
    let parsed = mail_parser::DateTime::parse_rfc822(value)?;
    let moment = DateTime::parse_from_rfc3339(&parsed.to_rfc3339()).ok()?;
    Some(moment.with_timezone(&Local))
}

/// Used by the sidebar's year grouping tests; kept public for the app.
pub fn year_of(value: &str) -> Option<i32> {
    parse(value).map(|moment| moment.year())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rfc822_to_iso() {
        assert_eq!(
            to_iso("Wed, 16 Jul 2026 10:00:00 +0000"),
            "2026-07-16T10:00:00Z"
        );
        assert_eq!(to_iso("not a date"), "not a date");
    }

    #[test]
    fn labels_are_relative() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap();
        let stamp = |day: u32| {
            Local
                .with_ymd_and_hms(2026, 7, day, 10, 0, 0)
                .unwrap()
                .to_rfc3339()
        };
        use chrono::TimeZone;
        assert_eq!(
            relative_label_on(&stamp(20), today),
            RelativeLabel::Today("10:00".into())
        );
        assert_eq!(
            relative_label_on(&stamp(19), today),
            RelativeLabel::Yesterday
        );
        assert!(matches!(
            relative_label_on(&stamp(16), today),
            RelativeLabel::Weekday(_)
        ));
        assert_eq!(
            relative_label_on(&stamp(1), today),
            RelativeLabel::Date("Jul 01".into())
        );
        assert_eq!(
            relative_label_on("garbage", today),
            RelativeLabel::Raw("garbage".into())
        );
    }
}

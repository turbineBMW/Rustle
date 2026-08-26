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

/// The clock time of a stored timestamp, "16:02", the only part a list row
/// shows once the day is carried by a section header. Unreadable: verbatim.
pub fn time_label(value: &str) -> String {
    match parse(value) {
        Some(moment) => moment.format("%H:%M").to_string(),
        None => value.to_string(),
    }
}

/// Seconds since the epoch, for ordering rows by when they were sent rather
/// than by the text of the header (whose zone offsets don't sort). Unreadable
/// dates sort oldest.
pub fn sort_key(value: &str) -> i64 {
    parse(value)
        .map(|moment| moment.timestamp())
        .unwrap_or(i64::MIN)
}

/// The calendar day a stored timestamp falls on, in local time; rows that
/// share one sit under the same section header. Unreadable dates share
/// `None`.
pub fn day_of(value: &str) -> Option<NaiveDate> {
    parse(value).map(|moment| moment.date_naive())
}

/// How a day's section header reads. The GTK layer translates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DayLabel {
    Today,
    Yesterday,
    /// Any other day: "Wednesday", "August", 26, and the year only when it
    /// isn't the current one.
    Date {
        weekday: String,
        month: String,
        day: u32,
        year: Option<i32>,
    },
    /// A date we can't read, shown verbatim.
    Raw(String),
}

pub fn day_label(value: &str) -> DayLabel {
    day_label_on(value, Local::now().date_naive())
}

fn day_label_on(value: &str, today: NaiveDate) -> DayLabel {
    let Some(moment) = parse(value) else {
        return DayLabel::Raw(value.to_string());
    };
    let day = moment.date_naive();
    match today.signed_duration_since(day).num_days() {
        0 => DayLabel::Today,
        1 => DayLabel::Yesterday,
        _ => DayLabel::Date {
            weekday: moment.format("%A").to_string(),
            month: moment.format("%B").to_string(),
            day: day.day(),
            year: (day.year() != today.year()).then_some(day.year()),
        },
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
    fn day_labels() {
        use chrono::TimeZone;
        let today = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap();
        let stamp = |y: i32, m: u32, d: u32| {
            Local
                .with_ymd_and_hms(y, m, d, 10, 5, 0)
                .unwrap()
                .to_rfc3339()
        };
        assert_eq!(day_label_on(&stamp(2026, 8, 26), today), DayLabel::Today);
        assert_eq!(
            day_label_on(&stamp(2026, 8, 25), today),
            DayLabel::Yesterday
        );
        assert_eq!(
            day_label_on(&stamp(2026, 8, 19), today),
            DayLabel::Date {
                weekday: "Wednesday".into(),
                month: "August".into(),
                day: 19,
                year: None,
            }
        );
        assert_eq!(
            day_label_on(&stamp(2025, 12, 31), today),
            DayLabel::Date {
                weekday: "Wednesday".into(),
                month: "December".into(),
                day: 31,
                year: Some(2025),
            }
        );
        assert_eq!(
            day_label_on("garbage", today),
            DayLabel::Raw("garbage".into())
        );
        assert_eq!(time_label(&stamp(2026, 8, 26)), "10:05");
        assert_eq!(time_label("garbage"), "garbage");
        assert_eq!(
            day_of(&stamp(2026, 8, 19)),
            NaiveDate::from_ymd_opt(2026, 8, 19)
        );
        assert_eq!(day_of("garbage"), None);
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

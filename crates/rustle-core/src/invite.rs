//! Meeting invitations: the `text/calendar` part Exchange, Google and most
//! calendars attach to an invite, update, cancellation or reply (iMIP,
//! RFC 6047). Read-only -- enough of iCalendar (RFC 5545) to say what the
//! message is about, when, and where to join.

use crate::models::Attachment;
use chrono::{
    DateTime, Datelike, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc,
    Weekday,
};
use std::collections::HashMap;
use std::fmt::Display;

/// What the sender wants done with the event (the calendar's METHOD).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// A new invitation, or an update to one.
    Request,
    Cancel,
    /// An attendee answering the organizer.
    Reply,
    /// PUBLISH, COUNTER or anything else: shown as a plain event.
    Other,
}

/// An attendee's answer, from a REPLY's PARTSTAT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    Accepted,
    Tentative,
    Declined,
    Other,
}

/// A point in time an event starts or ends at: an instant, or a whole day
/// for all-day events (which have no time zone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moment {
    At(DateTime<Utc>),
    Day(NaiveDate),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub email: String,
}

impl Person {
    /// The common name, else the address.
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.email
        } else {
            &self.name
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invitation {
    pub method: Method,
    pub summary: String,
    pub start: Moment,
    pub end: Option<Moment>,
    /// Where this occurrence used to be, when an update moved it: the
    /// RECURRENCE-ID names the slot it replaces.
    pub previous_start: Option<Moment>,
    pub previous_end: Option<Moment>,
    /// Part of a series (it repeats, or it is one changed occurrence).
    pub is_recurring: bool,
    pub is_cancelled: bool,
    /// How many times the organizer has revised it; 0 for a fresh invite.
    pub sequence: u32,
    pub organizer: Option<Person>,
    pub location: String,
    /// A REPLY's attendee and their answer.
    pub reply: Option<(Person, Response)>,
    /// The online meeting to join (Teams, Meet, Zoom), when there is one.
    pub meeting_url: Option<String>,
    /// The calendar part itself, for saving.
    pub ics: Attachment,
}

/// Read the first event of an iCalendar object. None when there is no event
/// or it has no start we can read.
pub fn parse(ics: &[u8]) -> Option<Invitation> {
    let text = String::from_utf8_lossy(ics);
    let calendar = components(&text)
        .into_iter()
        .find(|c| c.name == "VCALENDAR")?;
    let zones: HashMap<String, Zone> = calendar
        .children
        .iter()
        .filter(|c| c.name == "VTIMEZONE")
        .filter_map(|c| Some((c.value("TZID")?.to_string(), Zone::from(c))))
        .collect();
    let event = calendar.children.iter().find(|c| c.name == "VEVENT")?;
    let moment = |name: &str| event.prop(name).and_then(|p| p.moment(&zones));

    let start = moment("DTSTART")?;
    let end = moment("DTEND").or_else(|| {
        let duration = parse_duration(event.value("DURATION")?)?;
        Some(add(start, duration))
    });
    let method = match calendar
        .value("METHOD")
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("REQUEST") => Method::Request,
        Some("CANCEL") => Method::Cancel,
        Some("REPLY") => Method::Reply,
        _ => Method::Other,
    };
    let recurrence_id = moment("RECURRENCE-ID");
    let previous_start = recurrence_id.filter(|&previous| previous != start);
    let previous_end = previous_start.zip(end).map(|(previous, end)| {
        // The old slot's end isn't sent; assume it ran as long as the new one.
        add(previous, difference(start, end))
    });
    let reply = (method == Method::Reply)
        .then(|| event.prop("ATTENDEE"))
        .flatten()
        .map(|attendee| {
            let response = match attendee
                .param("PARTSTAT")
                .map(str::to_ascii_uppercase)
                .as_deref()
            {
                Some("ACCEPTED") => Response::Accepted,
                Some("TENTATIVE") => Response::Tentative,
                Some("DECLINED") => Response::Declined,
                _ => Response::Other,
            };
            (attendee.person(), response)
        });
    let text = |name: &str| event.value(name).map(unescape).unwrap_or_default();

    Some(Invitation {
        method,
        summary: text("SUMMARY"),
        start,
        end,
        previous_start,
        previous_end,
        is_recurring: recurrence_id.is_some() || event.prop("RRULE").is_some(),
        is_cancelled: method == Method::Cancel
            || event
                .value("STATUS")
                .is_some_and(|s| s.eq_ignore_ascii_case("CANCELLED")),
        sequence: event
            .value("SEQUENCE")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        organizer: event.prop("ORGANIZER").map(Prop::person),
        location: text("LOCATION"),
        reply,
        meeting_url: meeting_url(event),
        ics: Attachment {
            filename: "invite.ics".into(),
            mime_type: "text/calendar".into(),
            content: ics.to_vec(),
        },
    })
}

/// The Teams desktop app's link for a Teams web link: teams-for-linux and
/// Microsoft's own client both register `msteams://` and take the same
/// host and path behind it. None for anything that isn't a Teams link.
pub fn teams_app_uri(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    let is_teams = [
        "teams.microsoft.com",
        "teams.live.com",
        "teams.cloud.microsoft",
    ]
    .iter()
    .any(|teams| host.eq_ignore_ascii_case(teams));
    let is_meeting = path.starts_with("l/") || path.starts_with("meet/");
    (is_teams && is_meeting).then(|| format!("msteams://{rest}"))
}

/// "Thu, Oct 8 · 14:00 – 14:30" in the local time zone; the date carries its
/// year when it isn't this one.
pub fn span_label(start: Moment, end: Option<Moment>) -> String {
    span_label_in(start, end, &Local, Local::now().year())
}

fn span_label_in<Tz: TimeZone>(
    start: Moment,
    end: Option<Moment>,
    zone: &Tz,
    this_year: i32,
) -> String
where
    Tz::Offset: Display,
{
    let day = |date: NaiveDate| {
        if date.year() == this_year {
            date.format("%a, %b %-d").to_string()
        } else {
            date.format("%a, %b %-d, %Y").to_string()
        }
    };
    let local = |at: DateTime<Utc>| at.with_timezone(zone).naive_local();
    match (start, end) {
        (Moment::At(start), Some(Moment::At(end))) => {
            let (start, end) = (local(start), local(end));
            if start.date() == end.date() {
                format!("{} · {} – {}", day(start.date()), hm(start), hm(end))
            } else {
                format!(
                    "{} · {} – {} · {}",
                    day(start.date()),
                    hm(start),
                    day(end.date()),
                    hm(end)
                )
            }
        }
        (Moment::At(start), _) => {
            let start = local(start);
            format!("{} · {}", day(start.date()), hm(start))
        }
        (Moment::Day(start), Some(Moment::Day(end))) => {
            // DTEND of an all-day event is the day after the last one.
            let last = end.pred_opt().unwrap_or(end);
            if last <= start {
                day(start)
            } else {
                format!("{} – {}", day(start), day(last))
            }
        }
        (Moment::Day(start), _) => day(start),
    }
}

fn hm(at: NaiveDateTime) -> String {
    at.format("%H:%M").to_string()
}

fn add(moment: Moment, duration: Duration) -> Moment {
    match moment {
        Moment::At(at) => Moment::At(at + duration),
        Moment::Day(day) => Moment::Day(day + duration),
    }
}

fn difference(start: Moment, end: Moment) -> Duration {
    match (start, end) {
        (Moment::At(start), Moment::At(end)) => end - start,
        (Moment::Day(start), Moment::Day(end)) => end - start,
        _ => Duration::zero(),
    }
}

/// The meeting link: Exchange names the Teams one outright, Google names its
/// Meet; otherwise the first meeting-service link in the location or notes.
fn meeting_url(event: &Component) -> Option<String> {
    for name in ["X-MICROSOFT-SKYPETEAMSMEETINGURL", "X-GOOGLE-CONFERENCE"] {
        if let Some(url) = event.value(name).map(unescape) {
            if url.starts_with("https://") {
                return Some(url);
            }
        }
    }
    let pattern = regex::Regex::new(
        r#"https://(?:teams\.(?:microsoft\.com|live\.com|cloud\.microsoft)/(?:l/meetup-join|meet)/|meet\.google\.com/|[\w.-]*zoom\.us/j/)[^\s<>"]+"#,
    )
    .expect("meeting link pattern is valid");
    ["LOCATION", "DESCRIPTION"]
        .iter()
        .filter_map(|name| event.value(name).map(unescape))
        .find_map(|text| pattern.find(&text).map(|m| m.as_str().to_string()))
}

/// TEXT values escape commas, semicolons, backslashes and newlines.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out.trim().to_string()
}

/// "PT30M", "P1D", "-PT1H30M", "P2W".
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (sign, value) = match value.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, value.strip_prefix('+').unwrap_or(value)),
    };
    let value = value.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut number = String::new();
    for c in value.chars() {
        match c {
            '0'..='9' => number.push(c),
            'T' => {}
            unit => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                total += match unit {
                    'W' => Duration::weeks(n),
                    'D' => Duration::days(n),
                    'H' => Duration::hours(n),
                    'M' => Duration::minutes(n),
                    'S' => Duration::seconds(n),
                    _ => return None,
                };
            }
        }
    }
    Some(total * sign)
}

// --- Content lines and components -----------------------------------------

#[derive(Debug)]
struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// ORGANIZER/ATTENDEE: "CN=Name:mailto:address".
    fn person(&self) -> Person {
        let value = self.value.trim();
        let email = if value.len() >= 7 && value[..7].eq_ignore_ascii_case("mailto:") {
            &value[7..]
        } else {
            value
        };
        Person {
            name: self.param("CN").unwrap_or("").trim().to_string(),
            email: email.to_string(),
        }
    }

    /// A DATE or DATE-TIME, resolved against the calendar's time zones.
    fn moment(&self, zones: &HashMap<String, Zone>) -> Option<Moment> {
        let value = self.value.trim();
        let is_date = self
            .param("VALUE")
            .is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
            || value.len() == 8;
        if is_date {
            return NaiveDate::parse_from_str(value.get(..8)?, "%Y%m%d")
                .ok()
                .map(Moment::Day);
        }
        if let Some(utc) = value.strip_suffix(['Z', 'z']) {
            let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
            return Some(Moment::At(naive.and_utc()));
        }
        let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
        let zone = self.param("TZID").and_then(|tzid| zones.get(tzid));
        let at = match zone.and_then(|zone| zone.offset_at(naive)) {
            Some(offset) => offset.from_local_datetime(&naive).single()?.to_utc(),
            // Floating time, or a zone the calendar didn't define: read it
            // as the reader's own wall clock.
            None => Local.from_local_datetime(&naive).earliest()?.to_utc(),
        };
        Some(Moment::At(at))
    }
}

#[derive(Debug, Default)]
struct Component {
    name: String,
    props: Vec<Prop>,
    children: Vec<Component>,
}

impl Component {
    fn prop(&self, name: &str) -> Option<&Prop> {
        self.props
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.prop(name).map(|p| p.value.as_str())
    }
}

/// The top-level components of an iCalendar stream, with lines unfolded.
fn components(text: &str) -> Vec<Component> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        match (line.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(continuation), Some(last)) => last.push_str(continuation),
            _ => lines.push(line.to_string()),
        }
    }

    let mut stack: Vec<Component> = vec![Component::default()];
    for line in &lines {
        let Some(prop) = content_line(line) else {
            continue;
        };
        if prop.name.eq_ignore_ascii_case("BEGIN") {
            stack.push(Component {
                name: prop.value.trim().to_ascii_uppercase(),
                ..Component::default()
            });
        } else if prop.name.eq_ignore_ascii_case("END") {
            if stack.len() > 1 {
                let done = stack.pop().expect("stack holds more than the root");
                stack.last_mut().expect("root").children.push(done);
            }
        } else if let Some(current) = stack.last_mut() {
            current.props.push(prop);
        }
    }
    // An unterminated component still counts.
    while stack.len() > 1 {
        let done = stack.pop().expect("stack holds more than the root");
        stack.last_mut().expect("root").children.push(done);
    }
    stack.pop().map(|root| root.children).unwrap_or_default()
}

/// `NAME;PARAM=value;PARAM="quoted:value":value`.
fn content_line(line: &str) -> Option<Prop> {
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = split_unquoted(head, ';').into_iter();
    let name = parts.next()?.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = parts
        .filter_map(|param| {
            let (key, value) = param.split_once('=')?;
            Some((
                key.trim().to_string(),
                value.trim().trim_matches('"').to_string(),
            ))
        })
        .collect();
    Some(Prop {
        name,
        params,
        value: value.to_string(),
    })
}

fn split_unquoted(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == separator && !in_quotes {
            parts.push(&text[start..i]);
            start = i + c.len_utf8();
        }
    }
    parts.push(&text[start..]);
    parts
}

// --- Time zones -------------------------------------------------------------

/// A VTIMEZONE: the offsets it switches between and when. Exchange names
/// zones the Windows way ("Eastern Standard Time") but always sends the
/// rules along, so they are read from here rather than looked up.
#[derive(Debug)]
struct Zone {
    observances: Vec<Observance>,
}

#[derive(Debug)]
struct Observance {
    /// The first switch, in the wall-clock time in force before it.
    onset: NaiveDateTime,
    offset_to: i32,
    rule: Option<YearlyRule>,
}

/// FREQ=YEARLY;BYMONTH=3;BYDAY=2SU -- "the second Sunday of March".
#[derive(Debug)]
struct YearlyRule {
    month: u32,
    nth: i32,
    weekday: Weekday,
    until: Option<NaiveDate>,
}

impl From<&Component> for Zone {
    fn from(timezone: &Component) -> Self {
        let observances = timezone
            .children
            .iter()
            .filter(|c| c.name == "STANDARD" || c.name == "DAYLIGHT")
            .filter_map(|c| {
                Some(Observance {
                    onset: NaiveDateTime::parse_from_str(
                        c.value("DTSTART")?.trim(),
                        "%Y%m%dT%H%M%S",
                    )
                    .ok()?,
                    offset_to: parse_offset(c.value("TZOFFSETTO")?)?,
                    rule: c.value("RRULE").and_then(parse_yearly_rule),
                })
            })
            .collect();
        Zone { observances }
    }
}

impl Zone {
    /// The UTC offset in force at a wall-clock time: the observance whose
    /// latest switch came last before it.
    fn offset_at(&self, at: NaiveDateTime) -> Option<FixedOffset> {
        let latest = self
            .observances
            .iter()
            .flat_map(|o| {
                [at.year(), at.year() - 1]
                    .into_iter()
                    .filter_map(move |year| Some((o.onset_in(year)?, o.offset_to)))
            })
            .filter(|(onset, _)| *onset <= at)
            .max_by_key(|(onset, _)| *onset);
        let seconds = match latest {
            Some((_, offset)) => offset,
            // Before the zone's first recorded switch.
            None => self.observances.iter().min_by_key(|o| o.onset)?.offset_to,
        };
        FixedOffset::east_opt(seconds)
    }
}

impl Observance {
    fn onset_in(&self, year: i32) -> Option<NaiveDateTime> {
        if year < self.onset.year() {
            return None;
        }
        let Some(rule) = &self.rule else {
            return (year == self.onset.year()).then_some(self.onset);
        };
        let date = nth_weekday(year, rule.month, rule.weekday, rule.nth)?;
        if rule.until.is_some_and(|until| date > until) {
            return None;
        }
        Some(date.and_time(self.onset.time()))
    }
}

fn parse_yearly_rule(rrule: &str) -> Option<YearlyRule> {
    let parts: HashMap<String, &str> = rrule
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.trim().to_ascii_uppercase(), value.trim()))
        .collect();
    if !parts.get("FREQ")?.eq_ignore_ascii_case("YEARLY") {
        return None;
    }
    let month = parts.get("BYMONTH")?.parse().ok()?;
    let byday = parts.get("BYDAY")?;
    let split = byday.len().checked_sub(2)?;
    let (nth, day) = byday.split_at(split);
    let nth = match nth {
        "" => 1,
        nth => nth.trim_start_matches('+').parse().ok()?,
    };
    let weekday = match day.to_ascii_uppercase().as_str() {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    };
    let until = parts
        .get("UNTIL")
        .and_then(|until| NaiveDate::parse_from_str(until.get(..8)?, "%Y%m%d").ok());
    Some(YearlyRule {
        month,
        nth,
        weekday,
        until,
    })
}

/// The nth (or, counting back, -nth) given weekday of a month.
fn nth_weekday(year: i32, month: u32, weekday: Weekday, nth: i32) -> Option<NaiveDate> {
    if nth > 0 {
        return NaiveDate::from_weekday_of_month_opt(year, month, weekday, nth as u8);
    }
    let next_month = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    let last = next_month.pred_opt()?;
    let back = (7 + last.weekday().num_days_from_monday() - weekday.num_days_from_monday()) % 7;
    let date = last - Duration::days(back as i64) - Duration::weeks((-nth - 1) as i64);
    (date.month() == month).then_some(date)
}

/// "-0400" or "+053000" -> seconds east of UTC.
fn parse_offset(value: &str) -> Option<i32> {
    let value = value.trim();
    let (sign, digits) = match value.as_bytes().first()? {
        b'-' => (-1, &value[1..]),
        b'+' => (1, &value[1..]),
        _ => (1, value),
    };
    let field = |range: std::ops::Range<usize>| -> Option<i32> {
        match digits.get(range) {
            Some(text) if !text.is_empty() => text.parse().ok(),
            _ => Some(0),
        }
    };
    if digits.len() < 4 {
        return None;
    }
    Some(sign * (field(0..2)? * 3600 + field(2..4)? * 60 + field(4..6)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Teams reschedule from Exchange, trimmed of the X- noise.
    const EXCHANGE: &str = r#"BEGIN:VCALENDAR
METHOD:REQUEST
PRODID:Microsoft Exchange Server 2010
VERSION:2.0
BEGIN:VTIMEZONE
TZID:Eastern Standard Time
BEGIN:STANDARD
DTSTART:16010101T020000
TZOFFSETFROM:-0400
TZOFFSETTO:-0500
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=1SU;BYMONTH=11
END:STANDARD
BEGIN:DAYLIGHT
DTSTART:16010101T020000
TZOFFSETFROM:-0500
TZOFFSETTO:-0400
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=2SU;BYMONTH=3
END:DAYLIGHT
END:VTIMEZONE
BEGIN:VEVENT
ORGANIZER;CN=Alex Gruevski:mailto:alex.gruevski@example.com
ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE;CN=Brandon Wi
 lliams:mailto:brandon@example.com
DESCRIPTION;LANGUAGE=en-US:\nMicrosoft Teams Need help?<https://aka
 .ms/JoinTeamsMeeting?omkt=en-US>\nJoin the meeting now<https://teams.micro
 soft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0?context=%7b%7d>\nPri
 vileged\, confidential
UID:040000008200E00074C5B7101A82E008
RECURRENCE-ID;TZID=Eastern Standard Time:20261007T130000
SUMMARY;LANGUAGE=en-US:Weekly 1:1 Alex/Brandon
DTSTART;TZID=Eastern Standard Time:20261008T140000
DTEND;TZID=Eastern Standard Time:20261008T143000
STATUS:CONFIRMED
SEQUENCE:25
LOCATION;LANGUAGE=en-US:Microsoft Teams Meeting
X-MICROSOFT-SKYPETEAMSMEETINGURL:https://teams.microsoft.com/l/meetup-join/
 19%3ameeting_abc%40thread.v2/0?context=%7b%22Tid%22%3a%22x%22%7d
BEGIN:VALARM
TRIGGER;RELATED=START:-PT15M
ACTION:DISPLAY
END:VALARM
END:VEVENT
END:VCALENDAR
"#;

    fn utc(text: &str) -> Moment {
        Moment::At(DateTime::parse_from_rfc3339(text).unwrap().to_utc())
    }

    #[test]
    fn reads_an_exchange_reschedule() {
        // As sent: CRLF line ends, long lines folded.
        let invite = parse(EXCHANGE.replace('\n', "\r\n").as_bytes()).unwrap();
        assert_eq!(invite.method, Method::Request);
        assert_eq!(invite.summary, "Weekly 1:1 Alex/Brandon");
        // October is daylight time in Eastern: UTC-4.
        assert_eq!(invite.start, utc("2026-10-08T18:00:00Z"));
        assert_eq!(invite.end, Some(utc("2026-10-08T18:30:00Z")));
        assert_eq!(invite.previous_start, Some(utc("2026-10-07T17:00:00Z")));
        assert_eq!(invite.previous_end, Some(utc("2026-10-07T17:30:00Z")));
        assert!(invite.is_recurring);
        assert!(!invite.is_cancelled);
        assert_eq!(invite.sequence, 25);
        assert_eq!(
            invite.organizer,
            Some(Person {
                name: "Alex Gruevski".into(),
                email: "alex.gruevski@example.com".into(),
            })
        );
        assert_eq!(invite.location, "Microsoft Teams Meeting");
        assert_eq!(
            invite.meeting_url.as_deref(),
            Some(
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0\
                 ?context=%7b%22Tid%22%3a%22x%22%7d"
            )
        );
        assert_eq!(invite.ics.filename, "invite.ics");
    }

    #[test]
    fn winter_times_use_the_standard_offset() {
        let ics = EXCHANGE
            .replace("20261008T140000", "20261210T140000")
            .replace("20261008T143000", "20261210T143000");
        let invite = parse(ics.as_bytes()).unwrap();
        assert_eq!(invite.start, utc("2026-12-10T19:00:00Z"));
    }

    #[test]
    fn falls_back_to_a_meeting_link_in_the_description() {
        let ics = EXCHANGE.replace("X-MICROSOFT-SKYPETEAMSMEETINGURL", "X-IGNORED");
        let invite = parse(ics.as_bytes()).unwrap();
        assert_eq!(
            invite.meeting_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0?context=%7b%7d")
        );
    }

    #[test]
    fn reads_cancellations_and_replies() {
        let cancel = EXCHANGE.replace("METHOD:REQUEST", "METHOD:CANCEL");
        assert!(parse(cancel.as_bytes()).unwrap().is_cancelled);

        let reply = EXCHANGE
            .replace("METHOD:REQUEST", "METHOD:REPLY")
            .replace("PARTSTAT=NEEDS-ACTION", "PARTSTAT=ACCEPTED");
        let invite = parse(reply.as_bytes()).unwrap();
        let (who, response) = invite.reply.unwrap();
        assert_eq!(who.label(), "Brandon Williams");
        assert_eq!(response, Response::Accepted);
    }

    #[test]
    fn reads_all_day_events_and_durations() {
        let ics = "BEGIN:VCALENDAR\nMETHOD:PUBLISH\nBEGIN:VEVENT\n\
                   DTSTART;VALUE=DATE:20261008\nDTEND;VALUE=DATE:20261010\n\
                   SUMMARY:Offsite\\, day one\nEND:VEVENT\nEND:VCALENDAR\n";
        let invite = parse(ics.as_bytes()).unwrap();
        assert_eq!(invite.method, Method::Other);
        assert_eq!(invite.summary, "Offsite, day one");
        let day = |d| Moment::Day(NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap());
        assert_eq!(invite.start, day("2026-10-08"));
        assert_eq!(invite.end, Some(day("2026-10-10")));
        assert!(invite.previous_start.is_none());

        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nDTSTART:20261008T140000Z\n\
                   DURATION:PT1H30M\nEND:VEVENT\nEND:VCALENDAR\n";
        let invite = parse(ics.as_bytes()).unwrap();
        assert_eq!(invite.end, Some(utc("2026-10-08T15:30:00Z")));
    }

    #[test]
    fn without_an_event_there_is_no_invitation() {
        assert!(parse(b"BEGIN:VCALENDAR\nBEGIN:VTODO\nEND:VTODO\nEND:VCALENDAR\n").is_none());
        assert!(parse(b"not a calendar").is_none());
    }

    #[test]
    fn quoted_parameters_may_hold_colons_and_semicolons() {
        let prop =
            content_line(r#"ORGANIZER;CN="Gruevski; Alex: PM":mailto:a@example.com"#).unwrap();
        assert_eq!(prop.person().name, "Gruevski; Alex: PM");
        assert_eq!(prop.person().email, "a@example.com");
    }

    #[test]
    fn last_weekday_rules() {
        // Europe: last Sunday of March and October.
        let date = |d| NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap();
        assert_eq!(
            nth_weekday(2026, 3, Weekday::Sun, -1),
            Some(date("2026-03-29"))
        );
        assert_eq!(
            nth_weekday(2026, 10, Weekday::Sun, -1),
            Some(date("2026-10-25"))
        );
        assert_eq!(
            nth_weekday(2026, 11, Weekday::Sun, 1),
            Some(date("2026-11-01"))
        );
    }

    #[test]
    fn teams_links_open_in_the_app() {
        assert_eq!(
            teams_app_uri("https://teams.microsoft.com/l/meetup-join/19%3a/0?context=x").as_deref(),
            Some("msteams://teams.microsoft.com/l/meetup-join/19%3a/0?context=x")
        );
        assert_eq!(
            teams_app_uri("https://teams.microsoft.com/meet/27981061141993?p=abc").as_deref(),
            Some("msteams://teams.microsoft.com/meet/27981061141993?p=abc")
        );
        assert_eq!(
            teams_app_uri("https://teams.microsoft.com/meetingOptions/?x"),
            None
        );
        assert_eq!(teams_app_uri("https://aka.ms/JoinTeamsMeeting"), None);
        assert_eq!(
            teams_app_uri("https://teams.microsoft.com.evil.example/l/x"),
            None
        );
        assert_eq!(
            teams_app_uri("http://teams.microsoft.com/l/meetup-join/x"),
            None
        );
    }

    #[test]
    fn span_labels() {
        let east = FixedOffset::west_opt(4 * 3600).unwrap();
        assert_eq!(
            span_label_in(
                utc("2026-10-08T18:00:00Z"),
                Some(utc("2026-10-08T18:30:00Z")),
                &east,
                2026
            ),
            "Thu, Oct 8 · 14:00 – 14:30"
        );
        assert_eq!(
            span_label_in(
                utc("2027-01-08T02:00:00Z"),
                Some(utc("2027-01-08T06:00:00Z")),
                &east,
                2026
            ),
            "Thu, Jan 7, 2027 · 22:00 – Fri, Jan 8, 2027 · 02:00"
        );
        let day = |d| Moment::Day(NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap());
        assert_eq!(
            span_label_in(day("2026-10-08"), Some(day("2026-10-09")), &east, 2026),
            "Thu, Oct 8"
        );
        assert_eq!(
            span_label_in(day("2026-10-08"), Some(day("2026-10-10")), &east, 2026),
            "Thu, Oct 8 – Fri, Oct 9"
        );
    }
}

//! Parsing published opening and duty hours.
//!
//! Hospital rotas use precise ranges (`14:30 – 08:00 επομένης`), while pharmacies
//! publish colloquial ones (`8 ΠΡΩΙ - 11 ΒΡΑΔΥ`). Both are common, and both frequently
//! run past midnight, which the resulting range records explicitly.

use super::strip_accents_upper;
use jiff::civil::{Date, DateTime, Time};
use regex::Regex;
use std::sync::LazyLock;

/// Characters used as a range separator across these sources.
const DASHES: [char; 5] = ['-', '\u{2010}', '\u{2013}', '\u{2014}', '\u{2212}'];

/// Marks the end of the range as falling on the following day: "επομένης" ("of the next").
const NEXT_DAY: &str = "ΕΠΟΜΕΝΗΣ";

static CLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{1,2})[:.](\d{2})").expect("valid clock pattern"));
static HOUR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{1,2})").expect("valid hour pattern"));

/// A duty window, which may end on the day after it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub start: Time,
    pub end: Time,
    pub ends_next_day: bool,
}

impl TimeRange {
    /// Anchors the range to a calendar day. Returns `None` only if the following day
    /// would fall outside the supported calendar range.
    pub fn on(&self, date: Date) -> Option<(DateTime, DateTime)> {
        let end_date = if self.ends_next_day {
            date.tomorrow().ok()?
        } else {
            date
        };
        Some((date.to_datetime(self.start), end_date.to_datetime(self.end)))
    }
}

/// Parses every window in a published shift, in the order written.
///
/// Pharmacies commonly work a split shift — `8 ΠΡΩΙ - 2 ΜΕΣΗΜΕΡΙ & 5 ΑΠΟΓΕΥΜΑ - 8 ΠΡΩΙ
/// ΕΠΟΜΕΝΗΣ` is two separate duty windows with the afternoon off in between — so a shift
/// is a list of ranges, not one range. Windows that cannot be read are left out; the
/// result is empty when nothing could be understood.
pub fn parse_ranges(text: &str) -> Vec<TimeRange> {
    let normalized = strip_accents_upper(text);
    normalized
        .split(['&', ','])
        .flat_map(|segment| segment.split(" ΚΑΙ "))
        .filter_map(parse_segment)
        .collect()
}

/// Parses a single time range, returning `None` if it cannot be understood.
///
/// A range whose end is not after its start is treated as running overnight, which is
/// how a 24-hour pharmacy shift (`08:00 – 08:00`) is written.
pub fn parse_range(text: &str) -> Option<TimeRange> {
    parse_segment(&strip_accents_upper(text))
}

/// Parses one `start - end` window from already-normalized text.
fn parse_segment(normalized: &str) -> Option<TimeRange> {
    let dash_at = normalized.find(DASHES)?;
    let dash = normalized[dash_at..].chars().next()?;
    let (left, rest) = normalized.split_at(dash_at);
    let right = rest.get(dash.len_utf8()..)?;

    let start = parse_time(left)?;
    let end = parse_time(right)?;
    let ends_next_day = right.contains(NEXT_DAY) || end <= start;

    Some(TimeRange {
        start,
        end,
        ends_next_day,
    })
}

/// Parses one side of a range: `08:00`, `8.30`, `8 ΠΡΩΙ` or `11 ΒΡΑΔΥ`.
fn parse_time(text: &str) -> Option<Time> {
    if let Some(captures) = CLOCK.captures(text) {
        let hour: i8 = captures.get(1)?.as_str().parse().ok()?;
        let minute: i8 = captures.get(2)?.as_str().parse().ok()?;
        return Time::new(hour, minute, 0, 0).ok();
    }

    let hour: i8 = HOUR.captures(text)?.get(1)?.as_str().parse().ok()?;
    let hour = match Meridiem::of(text) {
        // "12 ΠΡΩΙ" is midnight; "12 ΒΡΑΔΥ" is midnight at the other end of the day.
        Some(Meridiem::Morning | Meridiem::Night) if hour == 12 => 0,
        Some(Meridiem::Night | Meridiem::Afternoon) if hour < 12 => hour + 12,
        _ => hour,
    };

    Time::new(hour, 0, 0, 0).ok()
}

/// The colloquial part-of-day qualifier attached to a bare hour.
enum Meridiem {
    /// πρωί — morning.
    Morning,
    /// μεσημέρι / απόγευμα — midday and afternoon.
    Afternoon,
    /// βράδυ / νύχτα — evening and night.
    Night,
}

impl Meridiem {
    fn of(text: &str) -> Option<Self> {
        if text.contains("ΠΡΩΙ") {
            Some(Self::Morning)
        } else if text.contains("ΒΡΑΔΥ") || text.contains("ΝΥΧΤΑ") {
            Some(Self::Night)
        } else if text.contains("ΜΕΣΗΜΕΡΙ") || text.contains("ΑΠΟΓΕΥΜΑ") {
            Some(Self::Afternoon)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(text: &str) -> TimeRange {
        parse_range(text).unwrap_or_else(|| panic!("should parse: {text}"))
    }

    fn time(hour: i8, minute: i8) -> Time {
        Time::new(hour, minute, 0, 0).expect("valid time")
    }

    #[test]
    fn reads_a_plain_clock_range() {
        let parsed = range("08:00 – 14:30");
        assert_eq!(parsed.start, time(8, 0));
        assert_eq!(parsed.end, time(14, 30));
        assert!(!parsed.ends_next_day);
    }

    #[test]
    fn an_explicit_next_day_marker_is_honoured() {
        let parsed = range("14:30 – 08:00 επομένης");
        assert_eq!(parsed.start, time(14, 30));
        assert_eq!(parsed.end, time(8, 0));
        assert!(parsed.ends_next_day);
    }

    #[test]
    fn a_full_day_shift_ends_the_following_morning() {
        assert!(range("08:00 – 08:00 επομένης").ends_next_day);
        // Even without the marker, an end that does not follow the start is overnight.
        assert!(range("22:00-06:00").ends_next_day);
    }

    #[test]
    fn reads_the_colloquial_pharmacy_wording() {
        let parsed = range("8 ΠΡΩΙ - 11 ΒΡΑΔΥ");
        assert_eq!(parsed.start, time(8, 0));
        assert_eq!(parsed.end, time(23, 0));
        assert!(!parsed.ends_next_day);
    }

    #[test]
    fn midnight_is_understood_from_either_side_of_the_clock() {
        let parsed = range("8 πρωί - 12 βράδυ");
        assert_eq!(parsed.start, time(8, 0));
        assert_eq!(parsed.end, time(0, 0));
        assert!(parsed.ends_next_day);
    }

    #[test]
    fn afternoon_hours_move_past_noon() {
        assert_eq!(range("8 ΠΡΩΙ - 2 ΜΕΣΗΜΕΡΙ").end, time(14, 0));
    }

    #[test]
    fn a_split_shift_yields_one_window_per_period() {
        // A quarter of Attica's duty pharmacies work this pattern.
        let windows = parse_ranges("8 ΠΡΩΙ - 2 ΜΕΣΗΜΕΡΙ & 5 ΑΠΟΓΕΥΜΑ - 8 ΠΡΩΙ ΕΠΟΜΕΝΗΣ");
        assert_eq!(windows.len(), 2);

        assert_eq!(windows[0].start, time(8, 0));
        assert_eq!(windows[0].end, time(14, 0));
        assert!(!windows[0].ends_next_day);

        assert_eq!(windows[1].start, time(17, 0));
        assert_eq!(windows[1].end, time(8, 0));
        assert!(windows[1].ends_next_day);
    }

    #[test]
    fn the_other_shift_patterns_attica_publishes_all_read_correctly() {
        let cases: [(&str, i8, i8, bool); 4] = [
            ("8 ΠΡΩΙ - 11 ΒΡΑΔΥ", 8, 23, false),
            ("8 ΠΡΩΙ - 9 ΒΡΑΔΥ", 8, 21, false),
            ("8 ΠΡΩΙ - 8 ΠΡΩΙ ΕΠΟΜΕΝΗΣ ΗΜΕΡΑΣ", 8, 8, true),
            ("9 ΒΡΑΔΥ - 8 ΠΡΩΙ ΕΠΟΜΕΝΗΣ", 21, 8, true),
        ];

        for (text, start, end, overnight) in cases {
            let windows = parse_ranges(text);
            assert_eq!(windows.len(), 1, "{text}");
            assert_eq!(windows[0].start, time(start, 0), "{text}");
            assert_eq!(windows[0].end, time(end, 0), "{text}");
            assert_eq!(windows[0].ends_next_day, overnight, "{text}");
        }
    }

    #[test]
    fn a_shift_that_cannot_be_read_yields_no_windows() {
        assert!(parse_ranges("ΚΑΤΟΠΙΝ ΣΥΝΕΝΝΟΗΣΗΣ").is_empty());
        assert!(parse_ranges("").is_empty());
    }

    #[test]
    fn unparseable_text_is_rejected_rather_than_guessed() {
        assert_eq!(parse_range("ΚΑΤΟΠΙΝ ΣΥΝΕΝΝΟΗΣΗΣ"), None);
        assert_eq!(parse_range("08:00"), None);
        assert_eq!(parse_range(""), None);
    }

    #[test]
    fn anchoring_an_overnight_range_lands_on_the_next_date() {
        let date = Date::new(2026, 8, 17).expect("valid date");
        let (start, end) = range("14:30 – 08:00 επομένης").on(date).expect("anchored");
        assert_eq!(start.to_string(), "2026-08-17T14:30:00");
        assert_eq!(end.to_string(), "2026-08-18T08:00:00");
    }
}

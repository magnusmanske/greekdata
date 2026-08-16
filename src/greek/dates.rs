//! Tolerant date parsing for text written by people, not machines.
//!
//! The Ministry of Health publishes one file per day whose date lives only in the
//! filename, written in Greek and misspelled often enough that exact matching is
//! useless: `ΣΕΠΤΕΒΡΙΟΥ` for `ΣΕΠΤΕΜΒΡΙΟΥ`, missing or doubled spaces, and Orthodox
//! calendar prefixes such as `Μ.ΤΡΙΤΗ`. Machine-readable forms are preferred when
//! present, and the Greek wording is parsed forgivingly otherwise.

use super::{edit_distance_within, fold};
use jiff::civil::Date;
use regex::Regex;
use std::sync::LazyLock;

/// Month names in the genitive (as dates are written) and nominative, already folded.
const MONTHS: [&[&str]; 12] = [
    &["ΙΑΝΟΥΑΡΙΟΥ", "ΙΑΝΟΥΑΡΙΟΣ"],
    &["ΦΕΒΡΟΥΑΡΙΟΥ", "ΦΕΒΡΟΥΑΡΙΟΣ"],
    &["ΜΑΡΤΙΟΥ", "ΜΑΡΤΙΟΣ"],
    &["ΑΠΡΙΛΙΟΥ", "ΑΠΡΙΛΙΟΣ"],
    &["ΜΑΙΟΥ", "ΜΑΙΟΣ"],
    &["ΙΟΥΝΙΟΥ", "ΙΟΥΝΙΟΣ"],
    &["ΙΟΥΛΙΟΥ", "ΙΟΥΛΙΟΣ"],
    &["ΑΥΓΟΥΣΤΟΥ", "ΑΥΓΟΥΣΤΟΣ"],
    &["ΣΕΠΤΕΜΒΡΙΟΥ", "ΣΕΠΤΕΜΒΡΙΟΣ"],
    &["ΟΚΤΩΒΡΙΟΥ", "ΟΚΤΩΒΡΙΟΣ"],
    &["ΝΟΕΜΒΡΙΟΥ", "ΝΟΕΜΒΡΙΟΣ"],
    &["ΔΕΚΕΜΒΡΙΟΥ", "ΔΕΚΕΜΒΡΙΟΣ"],
];

/// Weekday names, skipped before fuzzy matching so they can never be mistaken for a
/// misspelled month.
const WEEKDAYS: [&str; 7] = [
    "ΔΕΥΤΕΡΑ",
    "ΤΡΙΤΗ",
    "ΤΕΤΑΡΤΗ",
    "ΠΕΜΠΤΗ",
    "ΠΑΡΑΣΚΕΥΗ",
    "ΣΑΒΒΑΤΟ",
    "ΚΥΡΙΑΚΗ",
];

const MIN_YEAR: i16 = 1900;
const MAX_YEAR: i16 = 2100;

static ISO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\D|^)(\d{4})-(\d{2})-(\d{2})(?:\D|$)").expect("valid ISO pattern")
});
static COMPACT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\D|^)(\d{8})(?:\D|$)").expect("valid compact pattern"));
static NUMERIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\D|^)(\d{1,2})[./](\d{1,2})[./](\d{4})(?:\D|$)").expect("valid numeric pattern")
});

/// Extracts a date from arbitrary text, preferring unambiguous machine-written forms.
///
/// Tries, in order: ISO `YYYY-MM-DD`, compact `YYYYMMDD` (as in `efimeria_20260817.doc`),
/// Greek wording such as `ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026`, and finally `DD/MM/YYYY`.
pub fn parse_date(text: &str) -> Option<Date> {
    parse_iso(text)
        .or_else(|| parse_compact(text))
        .or_else(|| parse_greek_words(text))
        .or_else(|| parse_numeric(text))
}

fn parse_iso(text: &str) -> Option<Date> {
    let captures = ISO.captures(text)?;
    build_date(
        captures.get(1)?.as_str().parse().ok()?,
        captures.get(2)?.as_str().parse().ok()?,
        captures.get(3)?.as_str().parse().ok()?,
    )
}

fn parse_compact(text: &str) -> Option<Date> {
    let digits = COMPACT.captures(text)?.get(1)?.as_str();
    build_date(
        digits.get(0..4)?.parse().ok()?,
        digits.get(4..6)?.parse().ok()?,
        digits.get(6..8)?.parse().ok()?,
    )
}

fn parse_numeric(text: &str) -> Option<Date> {
    let captures = NUMERIC.captures(text)?;
    build_date(
        captures.get(3)?.as_str().parse().ok()?,
        captures.get(2)?.as_str().parse().ok()?,
        captures.get(1)?.as_str().parse().ok()?,
    )
}

/// Parses `ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026` and its many malformed cousins.
fn parse_greek_words(text: &str) -> Option<Date> {
    let folded = fold(text);
    let tokens = tokenize(&folded);

    let month_at = tokens
        .iter()
        .position(|token| month_number(token).is_some())?;
    let month = month_number(tokens[month_at])?;

    // The day precedes the month; the year normally follows it, but tolerate the
    // occasional file that puts it elsewhere.
    let day = tokens[..month_at]
        .iter()
        .rev()
        .find_map(|token| number_in_range(token, 1, 31))?;
    let year = tokens[month_at + 1..]
        .iter()
        .chain(tokens[..month_at].iter().rev())
        .find_map(|token| year_value(token))?;

    build_date(year, month, day as i8)
}

/// Splits folded text into runs of letters and runs of digits, so `11ΙΟΥΛΙΟΥ` and
/// `efimeria_20260817` yield usable tokens even without separators.
fn tokenize(folded: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut in_digits = false;

    for (index, c) in folded.char_indices() {
        let (keep, digit) = (c.is_alphanumeric(), c.is_ascii_digit());
        match start {
            Some(begin) if !keep || digit != in_digits => {
                tokens.push(&folded[begin..index]);
                start = keep.then_some(index);
                in_digits = digit;
            }
            None if keep => {
                start = Some(index);
                in_digits = digit;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        tokens.push(&folded[begin..]);
    }

    tokens
}

/// Resolves a token to a month, allowing unambiguous abbreviations and small typos.
fn month_number(token: &str) -> Option<i8> {
    if token.chars().any(|c| c.is_ascii_digit()) || is_weekday(token) {
        return None;
    }

    if let Some(index) = MONTHS
        .iter()
        .position(|spellings| spellings.contains(&token))
    {
        return Some(index as i8 + 1);
    }

    let length = token.chars().count();
    if length < 3 {
        return None;
    }

    // An abbreviation is only usable if exactly one month starts with it.
    let prefixed: Vec<usize> = MONTHS
        .iter()
        .enumerate()
        .filter(|(_, spellings)| spellings.iter().any(|name| name.starts_with(token)))
        .map(|(index, _)| index)
        .collect();
    if let [only] = prefixed[..] {
        return Some(only as i8 + 1);
    }

    // Otherwise accept a near-miss, but only when one month is strictly closest:
    // ΙΟΥΝΙΟΥ and ΙΟΥΛΙΟΥ are one edit apart, so a tie must never guess.
    if length < 5 {
        return None;
    }
    let mut best: Option<(usize, i8)> = None;
    let mut tied = false;
    for (index, spellings) in MONTHS.iter().enumerate() {
        let Some(distance) = spellings
            .iter()
            .filter_map(|name| edit_distance_within(token, name, 2))
            .min()
        else {
            continue;
        };
        match best {
            Some((best_distance, _)) if distance > best_distance => {}
            Some((best_distance, _)) if distance == best_distance => tied = true,
            _ => {
                best = Some((distance, index as i8 + 1));
                tied = false;
            }
        }
    }

    match best {
        Some((_, month)) if !tied => Some(month),
        _ => None,
    }
}

fn is_weekday(token: &str) -> bool {
    WEEKDAYS
        .iter()
        .any(|weekday| token == *weekday || token.ends_with(weekday))
}

fn number_in_range(token: &str, low: u32, high: u32) -> Option<u32> {
    let value: u32 = token.parse().ok()?;
    (token.len() <= 2 && (low..=high).contains(&value)).then_some(value)
}

fn year_value(token: &str) -> Option<i16> {
    let value: i16 = token.parse().ok()?;
    (token.len() == 4 && (MIN_YEAR..=MAX_YEAR).contains(&value)).then_some(value)
}

fn build_date(year: i16, month: i8, day: i8) -> Option<Date> {
    (MIN_YEAR..=MAX_YEAR)
        .contains(&year)
        .then(|| Date::new(year, month, day).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i16, month: i8, day: i8) -> Option<Date> {
        Date::new(year, month, day).ok()
    }

    #[test]
    fn reads_the_machine_written_forms() {
        assert_eq!(parse_date("2026-08-17"), date(2026, 8, 17));
        assert_eq!(parse_date("efimeria_20260817.doc"), date(2026, 8, 17));
        assert_eq!(parse_date("17/08/2026"), date(2026, 8, 17));
    }

    #[test]
    fn reads_plain_greek_dates() {
        assert_eq!(parse_date("ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026"), date(2026, 8, 17));
        assert_eq!(parse_date("ΚΥΡΙΑΚΗ 05 ΙΟΥΛΙΟΥ 2026"), date(2026, 7, 5));
        assert_eq!(parse_date("Τρίτη 3 Μαΐου 2022"), date(2022, 5, 3));
    }

    #[test]
    fn survives_the_typos_the_ministry_actually_publishes() {
        // Missing Μ in ΣΕΠΤΕΜΒΡΙΟΥ.
        assert_eq!(parse_date("ΚΥΡΙΑΚΗ 14 ΣΕΠΤΕΒΡΙΟΥ 2025"), date(2025, 9, 14));
        // Doubled space, and a stray Greek question mark inside a later word.
        assert_eq!(
            parse_date("ΣΑΒΒΑΤΟ 11 ΙΟΥΛΙΟΥ 2026 -ΟΡΘΗ ΕΠΑΝΑ;ΚΟΙΝΟΠΟΙΗΣΗ"),
            date(2026, 7, 11)
        );
        assert_eq!(parse_date("ΤΡΙΤΗ 16  ΙΟΥΝΙΟΥ 2026"), date(2026, 6, 16));
        // Holy Tuesday, written with the Orthodox calendar prefix.
        assert_eq!(parse_date("Μ.ΤΡΙΤΗ 14 ΑΠΡΙΛΙΟΥ 2026"), date(2026, 4, 14));
    }

    #[test]
    fn a_reissue_suffix_does_not_disturb_the_date() {
        assert_eq!(
            parse_date("ΠΑΡΑΣΚΕΥΗ 14 ΝΟΕΜΒΡΙΟΥ 2025 - Β' ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ.pdf"),
            date(2025, 11, 14)
        );
    }

    #[test]
    fn the_compact_form_wins_over_greek_wording() {
        // The Word sibling carries a reliable date; its stale document title does not.
        assert_eq!(
            parse_date("efimeria_20260817 ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ.doc"),
            date(2026, 8, 17)
        );
    }

    #[test]
    fn ambiguous_or_absent_dates_yield_nothing_rather_than_a_guess() {
        assert_eq!(parse_date("ΕΦΗΜΕΡΙΕΣ ΝΟΣΟΚΟΜΕΙΩΝ"), None);
        assert_eq!(parse_date(""), None);
        // ΙΟΥ could be June or July, so it must not resolve.
        assert_eq!(parse_date("ΔΕΥΤΕΡΑ 17 ΙΟΥ 2026"), None);
        // Impossible calendar day.
        assert_eq!(parse_date("ΔΕΥΤΕΡΑ 31 ΦΕΒΡΟΥΑΡΙΟΥ 2026"), None);
    }

    #[test]
    fn unambiguous_abbreviations_still_resolve() {
        assert_eq!(parse_date("ΔΕΥΤΕΡΑ 17 ΑΥΓ 2026"), date(2026, 8, 17));
        assert_eq!(parse_date("ΤΕΤΑΡΤΗ 2 ΣΕΠΤ 2026"), date(2026, 9, 2));
    }

    #[test]
    fn tokenizer_splits_letters_from_digits() {
        assert_eq!(
            tokenize("ΣΑΒΒΑΤΟ 11ΙΟΥΛΙΟΥ 2026"),
            ["ΣΑΒΒΑΤΟ", "11", "ΙΟΥΛΙΟΥ", "2026"]
        );
    }
}

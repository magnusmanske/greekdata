//! Normalizing messy Greek source text.
//!
//! Documents from these sources are typed by hand and carry accents, mixed casing,
//! inconsistent punctuation and outright typos. Everything here turns that variation
//! into something comparable, without ever throwing the original away.

pub mod dates;
pub mod revision;
pub mod times;

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Uppercases and strips accents, leaving punctuation and spacing untouched.
///
/// Use this when the layout of a string still matters — a time like `08:00` must not
/// lose its colon. Greek final sigma folds into sigma as a side effect of uppercasing.
pub fn strip_accents_upper(text: &str) -> String {
    text.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_uppercase)
        .collect()
}

/// Reduces text to words: uppercase, unaccented, every run of punctuation or whitespace
/// collapsed to a single space.
///
/// All separators are treated alike, including the abbreviation dots in `Γ.Ν.Α.` and the
/// slash in `ΚΩΝ/ΠΟΥΛΕΙΟ`. That uniformity is what makes `Γ. ΓΕΝΝΗΜΑΤΑΣ` and
/// `Γ.ΓΕΝΝΗΜΑΤΑΣ` — the same hospital, spaced differently — fold to the same words.
pub fn fold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    let mut separator_pending = false;

    for c in strip_accents_upper(text).chars() {
        if c.is_alphanumeric() {
            if separator_pending && !folded.is_empty() {
                folded.push(' ');
            }
            separator_pending = false;
            folded.push(c);
        } else {
            separator_pending = true;
        }
    }

    folded
}

/// The key used to decide whether two names refer to the same entity.
///
/// This is [`fold`] with the spaces removed as well, because sources disagree about
/// spacing even more often than about punctuation. Never display this to anyone: it is
/// a comparison key, and the original name is always stored alongside it.
pub fn matching_key(text: &str) -> String {
    fold(text).replace(' ', "")
}

/// Levenshtein distance in characters, used to recognize misspelled month names.
///
/// Bails out early once the distance is known to exceed `limit`, since callers only
/// ever care about near-matches.
pub fn edit_distance_within(a: &str, b: &str, limit: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > limit {
        return None;
    }

    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];

    for (i, &ca) in a.iter().enumerate() {
        current[0] = i + 1;
        let mut row_best = current[0];
        for (j, &cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
            row_best = row_best.min(current[j + 1]);
        }
        if row_best > limit {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }

    let distance = previous[b.len()];
    (distance <= limit).then_some(distance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_and_case_fold_away() {
        assert_eq!(strip_accents_upper("Αύγουστος"), "ΑΥΓΟΥΣΤΟΣ");
        assert_eq!(strip_accents_upper("Μάιος"), "ΜΑΙΟΣ");
        // Final sigma folds into a plain sigma.
        assert_eq!(strip_accents_upper("εφημερίες"), "ΕΦΗΜΕΡΙΕΣ");
    }

    #[test]
    fn strip_accents_keeps_layout_characters() {
        assert_eq!(strip_accents_upper("08:00 – 14:30"), "08:00 – 14:30");
    }

    #[test]
    fn fold_turns_every_separator_into_one_space() {
        assert_eq!(fold("Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»"), "Γ Ν Α Γ ΓΕΝΝΗΜΑΤΑΣ");
        assert_eq!(
            fold("Γ.Ν.Ν.ΙΩΝΙΑΣ «ΚΩΝ/ΠΟΥΛΕΙΟ»"),
            "Γ Ν Ν ΙΩΝΙΑΣ ΚΩΝ ΠΟΥΛΕΙΟ"
        );
        assert_eq!(fold("  ΤΡΙΤΗ   16  ΙΟΥΝΙΟΥ 2026 "), "ΤΡΙΤΗ 16 ΙΟΥΝΙΟΥ 2026");
    }

    #[test]
    fn the_same_hospital_written_differently_gets_one_matching_key() {
        // Spacing, punctuation, quote style and accents all vary between documents.
        let spellings = [
            "Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»",
            "Γ.Ν.Α «Γ.ΓΕΝΝΗΜΑΤΑΣ»",
            "ΓΝΑ \"Γ ΓΕΝΝΗΜΑΤΑΣ\"",
            "Γ.Ν.Α.  Γ.  Γεννηματάς",
        ];
        let keys: Vec<String> = spellings.iter().map(|name| matching_key(name)).collect();
        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "spellings disagreed: {keys:?}"
        );
    }

    #[test]
    fn matching_key_still_separates_genuinely_different_names() {
        assert_ne!(
            matching_key("Γ.Ν.Α. «ΣΩΤΗΡΙΑ»"),
            matching_key("Γ.Ν.Α. «ΑΛΕΞΑΝΔΡΑ»")
        );
    }

    #[test]
    fn edit_distance_measures_near_misses_and_gives_up_on_far_ones() {
        assert_eq!(
            edit_distance_within("ΣΕΠΤΕΒΡΙΟΥ", "ΣΕΠΤΕΜΒΡΙΟΥ", 2),
            Some(1)
        );
        assert_eq!(edit_distance_within("ΙΟΥΝΙΟΥ", "ΙΟΥΛΙΟΥ", 2), Some(1));
        assert_eq!(edit_distance_within("ΜΑΙΟΥ", "ΔΕΚΕΜΒΡΙΟΥ", 2), None);
    }
}

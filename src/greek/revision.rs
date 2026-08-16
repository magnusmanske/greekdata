//! Working out which reissue of a document we are looking at.
//!
//! The Ministry of Health republishes a rota when it changes, marking the file
//! `ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ` ("corrected reissue"). A second correction is numbered with a
//! Greek letter, written variously as `Β' ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ` or `ΟΡΘΗ Α`. The highest
//! revision for a date is the one that counts.

use super::fold;
use crate::model::Revision;

const CORRECTION: &str = "ΟΡΘΗ";

/// Greek letters in their traditional use as numerals.
const NUMERALS: [(&str, u32); 5] = [("Α", 1), ("Β", 2), ("Γ", 3), ("Δ", 4), ("Ε", 5)];

/// Reads the revision out of a filename or label. Anything unmarked is the original.
pub fn parse_revision(text: &str) -> Revision {
    let folded = fold(text);
    if !folded.contains(CORRECTION) {
        return Revision::ORIGINAL;
    }

    let tokens: Vec<&str> = folded.split(' ').collect();
    let ordinal = tokens
        .iter()
        .position(|token| *token == CORRECTION)
        .and_then(|at| {
            let before = at.checked_sub(1).and_then(|index| tokens.get(index));
            let after = tokens.get(at + 1);
            before
                .into_iter()
                .chain(after)
                .find_map(|token| numeral(token))
        });

    // An unnumbered correction is the first one.
    Revision(ordinal.unwrap_or(1))
}

fn numeral(token: &str) -> Option<u32> {
    NUMERALS
        .iter()
        .find(|(letter, _)| *letter == token)
        .map(|(_, value)| *value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmarked_document_is_the_original() {
        assert_eq!(
            parse_revision("ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026.pdf"),
            Revision::ORIGINAL
        );
    }

    #[test]
    fn a_plain_correction_is_the_first_revision() {
        assert_eq!(
            parse_revision("ΠΕΜΠΤΗ 06 ΑΥΓΟΥΣΤΟΥ 2026 ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ.pdf"),
            Revision(1)
        );
        // Even with the Greek question mark the ministry once typed mid-word.
        assert_eq!(
            parse_revision("ΣΑΒΒΑΤΟ 11 ΙΟΥΛΙΟΥ 2026 -ΟΡΘΗ ΕΠΑΝΑ;ΚΟΙΝΟΠΟΙΗΣΗ.pdf"),
            Revision(1)
        );
    }

    #[test]
    fn numbered_corrections_are_ordered() {
        assert_eq!(
            parse_revision("ΠΑΡΑΣΚΕΥΗ 14 ΝΟΕΜΒΡΙΟΥ 2025 - Β' ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ.pdf"),
            Revision(2)
        );
        assert_eq!(
            parse_revision("efimeria_20260806 -Β` ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ.doc"),
            Revision(2)
        );
        // The ordinal is sometimes written after the word instead.
        assert_eq!(
            parse_revision("ΠΑΡΑΣΚΕΥΗ 14 ΑΥΓΟΥΣΤΟΥ 2026-ΟΡΘΗ Α.pdf"),
            Revision(1)
        );

        assert!(parse_revision("ΤΡΙΤΗ 5 ΜΑΙΟΥ 2026 - Γ ΟΡΘΗ.pdf") > Revision(2));
    }

    #[test]
    fn revisions_sort_so_the_latest_wins() {
        let mut revisions = [Revision(2), Revision::ORIGINAL, Revision(1)];
        revisions.sort();
        assert_eq!(revisions, [Revision(0), Revision(1), Revision(2)]);
    }
}

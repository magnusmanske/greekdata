//! On-call hospitals in Attica, published by the Ministry of Health.
//!
//! The ministry publishes one rota per day as a PDF (with a Word copy alongside) on a
//! single article page holding thousands of files. Each PDF is a table: the rows are
//! clinical specialities, the columns are shifts, and a hospital's cell position is the
//! only thing that says when it is on duty.
//!
//! Both the file listing and the documents are messy in ways worth stating plainly:
//! dates exist only in Greek filenames and are frequently misspelled, corrected reissues
//! are marked `ΟΡΘΗ ΕΠΑΝΑΚΟΙΝΟΠΟΙΗΣΗ`, and hospital names vary in punctuation and
//! spacing between documents. Dates and revisions are parsed forgivingly by
//! [`crate::greek`], and names are matched through the alias table.

use super::{Attribution, Ctx, DateWindow, DocumentRef, FetchedDoc, Source};
use crate::{
    Error, Result,
    cache::CachePolicy,
    greek::{self, dates, revision, times},
    model::{
        DataGroup, EntityDraft, EntityKind, Extraction, Identity, PropertyDraft, PropertyPayload,
        Record, Severity, Warning,
    },
    pdf::{self, Line, table},
};
use async_trait::async_trait;
use jiff::civil::Date;
use scraper::{Html, Selector};

const LISTING: &str = "https://www.moh.gov.gr/articles/citizen/efhmeries-nosokomeiwn/68-efhmeries-nosokomeiwn-attikhs";

/// The left-hand header cell of the rota table, which marks the header row.
const TABLE_HEADER: &str = "ΚΛΙΝΙΚΕΣ";

/// Prefix of a health centre entry, folded: `Κ.Υ.` (Κέντρο Υγείας).
const HEALTH_CENTRE: &str = "Κ Υ ";

pub struct MohAtticaHospitals;

#[async_trait]
impl Source for MohAtticaHospitals {
    fn id(&self) -> &'static str {
        "moh-attica-hospitals"
    }

    fn group(&self) -> DataGroup {
        DataGroup::Hospitals
    }

    fn attribution(&self) -> Attribution {
        Attribution {
            publisher: "Υπουργείο Υγείας",
            homepage: LISTING,
            terms: "Official government publication, republished with attribution.",
        }
    }

    async fn discover(&self, ctx: &Ctx, window: DateWindow) -> Result<Vec<DocumentRef>> {
        // One page lists every rota ever published, so it changes daily and is large.
        let listing = ctx
            .fetcher
            .get_with(self.id(), LISTING, CachePolicy::Revalidate)
            .await?;
        let today = super::today();

        Ok(published_documents(&listing.text())?
            .into_iter()
            .filter(|reference| reference.date.is_some_and(|date| window.contains(date)))
            .map(|reference| {
                let volatile = reference.date.is_some_and(|date| date >= today);
                reference.volatile(volatile)
            })
            .collect())
    }

    fn parse(&self, doc: &FetchedDoc) -> Result<Extraction> {
        let date = doc
            .reference
            .date
            .ok_or_else(|| Error::parse("ministry rota", "no date for this document"))?;

        let pages = pdf::extract(&doc.fetched.body)?;
        let mut extraction = Extraction::default();

        for page in &pages {
            read_health_centres(page, date, &mut extraction);

            let Some(rota) = table::reconstruct(page, &is_table_header) else {
                // Not every page carries the table; only say so if it had content.
                if page.lines.len() > 2 {
                    extraction.warnings.push(Warning::new(
                        Severity::Info,
                        "no_table_on_page",
                        format!("page {} has no rota table", page.number),
                    ));
                }
                continue;
            };
            read_rota(&rota, date, &mut extraction);
        }

        if extraction.records.is_empty() {
            extraction.warnings.push(Warning::warn(
                "empty_document",
                format!("no hospitals found in the rota for {date}"),
            ));
        }

        Ok(extraction)
    }
}

/// Recognizes the rota's header row.
///
/// The row-label cell is sometimes typeset a fraction off the shift columns' baseline,
/// splitting the header in two, so either half is accepted: the label, or a line holding
/// at least two shift times.
fn is_table_header(line: &Line) -> bool {
    let text = line.text();
    if greek::fold(&text).starts_with(TABLE_HEADER) {
        return true;
    }

    line.words
        .iter()
        .filter(|word| {
            word.text
                .split_once(':')
                .is_some_and(|(hours, _)| hours.chars().all(|c| c.is_ascii_digit()))
        })
        .count()
        >= 2
}

/// Reads the file listing into one document reference per published PDF.
///
/// The Word copy of each rota is skipped, but its filename is used first: it carries an
/// unambiguous `efimeria_YYYYMMDD` date, which rescues a PDF whose Greek filename is too
/// mangled to read.
fn published_documents(html: &str) -> Result<Vec<DocumentRef>> {
    let link = Selector::parse("a[href*='fdl=']")
        .map_err(|error| Error::parse("css selector", error.to_string()))?;
    let document = Html::parse_document(html);

    let files: Vec<PublishedFile> = document
        .select(&link)
        .filter_map(|anchor| {
            let href = anchor.value().attr("href")?;
            let filename = filename_from(anchor.value().attr("title")?)?;
            Some(PublishedFile {
                url: absolute_url(href),
                date: dates::parse_date(&filename),
                revision: revision::parse_revision(&filename),
                is_pdf: filename.to_lowercase().ends_with(".pdf"),
                filename,
            })
        })
        .collect();

    Ok(files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.is_pdf)
        .filter_map(|(index, file)| {
            let date = file.date.or_else(|| neighbouring_date(&files, index))?;
            Some(
                DocumentRef::new(&file.url, &file.filename)
                    .on(date)
                    .revision(file.revision),
            )
        })
        .collect())
}

struct PublishedFile {
    url: String,
    filename: String,
    date: Option<Date>,
    revision: crate::model::Revision,
    is_pdf: bool,
}

/// The date from the Word copy published immediately beside this PDF.
///
/// The two formats of a day's rota are always listed as a pair, so an unreadable Greek
/// filename can borrow the machine-readable date from its sibling.
fn neighbouring_date(files: &[PublishedFile], index: usize) -> Option<Date> {
    let before = index.checked_sub(1).and_then(|at| files.get(at));
    let after = files.get(index + 1);

    before
        .into_iter()
        .chain(after)
        .filter(|neighbour| !neighbour.is_pdf)
        .find_map(|neighbour| neighbour.date)
}

/// `Download: ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026.pdf (564.1 KB)` becomes the filename alone.
fn filename_from(title: &str) -> Option<String> {
    let name = title
        .trim()
        .strip_prefix("Download:")
        .unwrap_or(title)
        .trim();
    let name = match name.rfind('(') {
        Some(at) => name[..at].trim(),
        None => name,
    };

    (!name.is_empty()).then(|| name.to_string())
}

fn absolute_url(href: &str) -> String {
    match href.strip_prefix('?') {
        Some(query) => format!("{LISTING}?{query}"),
        None => href.to_string(),
    }
}

/// Turns the reconstructed table into one record per hospital and shift.
fn read_rota(rota: &table::Table, date: Date, extraction: &mut Extraction) {
    for (index, column) in rota.columns.iter().enumerate().skip(1) {
        // A column is split into entries as a whole, not cell by cell, because a long
        // name sometimes wraps across the boundary between two speciality rows. Each
        // entry then belongs to the row its first line came from.
        let mut lines = Vec::new();
        let mut line_rows = Vec::new();
        for (row_index, row) in rota.rows.iter().enumerate() {
            for line in row.cell(index) {
                lines.push(line.clone());
                line_rows.push(row_index);
            }
        }

        let entries = split_entries(&lines);
        if entries.is_empty() {
            continue;
        }

        // Columns whose header is not a time range are remarks, not a shift.
        let Some(shift) = times::parse_range(&column.label) else {
            extraction.warnings.push(Warning::new(
                Severity::Info,
                "non_shift_column",
                format!(
                    "{date}: ignoring `{}` column containing {:?}",
                    column.label,
                    entries
                        .iter()
                        .map(|(_, entry)| &entry.name)
                        .collect::<Vec<_>>()
                ),
            ));
            continue;
        };
        let (starts_at, ends_at) = match shift.on(date) {
            Some((start, end)) => (Some(start), Some(end)),
            None => (None, None),
        };

        for (line, entry) in entries {
            let clinic = line_rows
                .get(line)
                .and_then(|row| rota.rows.get(*row))
                .map_or("", |row| row.label.trim());

            // A name with a quote left open is a wrap the layout split in a way this
            // parser could not rejoin. It is still recorded, but flagged for review
            // rather than passed off as a clean hospital name.
            if unclosed_quote(&entry.name) || entry.name.starts_with('»') {
                extraction.warnings.push(Warning::warn(
                    "suspicious_name",
                    format!("{date} {clinic}: `{}` looks like a split name", entry.name),
                ));
            }

            let mut hospital = EntityDraft::new(
                EntityKind::Hospital,
                greek::matching_key(&entry.name),
                &entry.name,
            )
            .identified_by(Identity::Name);
            hospital.municipality = Some("Αττική".to_string());

            extraction.records.push(Record {
                entity: hospital,
                properties: vec![PropertyDraft {
                    on_date: date,
                    starts_at,
                    ends_at,
                    payload: PropertyPayload::HospitalOnCall {
                        clinic: clinic.to_string(),
                        shift: column.label.clone(),
                        notes: entry.notes,
                    },
                }],
            });
        }
    }
}

/// One hospital as written in a table cell.
#[derive(Debug, PartialEq)]
struct Entry {
    name: String,
    notes: Option<String>,
}

/// Splits a column's lines into hospitals, each paired with the index of the line it
/// started on so the caller can tell which speciality row it belongs to.
///
/// A column holds many hospitals and a long name wraps over two or three lines, with no
/// blank line or indentation to tell the two situations apart. The vertical gap does not
/// help either — it is the same within a wrapped name as between neighbouring entries —
/// so the decision is made from the text, by [`continues_previous`].
fn split_entries(lines: &[String]) -> Vec<(usize, Entry)> {
    let mut entries: Vec<(usize, Entry)> = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // A bracketed line qualifies the entry above it — "(ΚΑΙ ΔΙΑΒΗΤΟΛΟΓΙΚΟ)" — and is
        // kept out of the name so the hospital still matches its usual spelling.
        if let Some((_, entry)) = entries.last_mut()
            && line.starts_with('(')
        {
            append(&mut entry.notes, line);
            continue;
        }

        match entries.last_mut() {
            Some((_, entry)) if continues_previous(&entry.name, line) => {
                entry.name.push(' ');
                entry.name.push_str(line);
            }
            _ => entries.push((
                index,
                Entry {
                    name: line.to_string(),
                    notes: None,
                },
            )),
        }
    }

    for (_, entry) in &mut entries {
        if let Some(qualifier) = split_trailing_hours(&mut entry.name) {
            append(&mut entry.notes, &qualifier);
        }
    }
    // A stray connector left over from a wrap is not a hospital.
    entries.retain(|(_, entry)| !is_only_connectors(&entry.name));

    entries
}

/// Whether a name is nothing but leftover joining words, such as a stranded `έως`.
fn is_only_connectors(name: &str) -> bool {
    let folded = greek::fold(name);
    folded.is_empty() || folded.split(' ').all(is_connector)
}

fn is_connector(folded_word: &str) -> bool {
    matches!(folded_word, "ΕΩΣ" | "ΚΑΙ" | "ΜΕΧΡΙ" | "ΑΠΟ" | "")
}

fn append(target: &mut Option<String>, text: &str) {
    match target {
        Some(existing) => {
            existing.push(' ');
            existing.push_str(text);
        }
        none => *none = Some(text.to_string()),
    }
}

/// Whether `line` continues the hospital name above it rather than starting a new one.
///
/// A line opening with a hospital-type abbreviation is always a new hospital, and that
/// is checked first: without it, one name whose closing quote never arrives would
/// swallow the whole rest of the cell. After that, an opening quote continues, as does
/// any line closing a quote the previous one left open, or following a previous line
/// that is plainly unfinished. Anything else — a bare word such as the regional heading
/// `ΠΕΙΡΑΙΑΣ` — starts a new entry.
fn continues_previous(previous: &str, line: &str) -> bool {
    if opens_new_entry(line) {
        return false;
    }
    // A quoted name continues the line above only while that line is still waiting for
    // one. `Γ.Ν.Ν.ΙΩΝΙΑΣ` is; `Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»` already has its own, so a
    // following `«ΔΡΟΜΟΚΑΪΤΕΙΟ»` is a different hospital.
    if line.starts_with('«') {
        return !previous.contains('»');
    }

    unclosed_quote(previous) || is_bare_abbreviation(previous) || trails_off(previous)
}

/// A line beginning with an abbreviated hospital type: a capital letter, then a stop.
fn opens_new_entry(line: &str) -> bool {
    let mut characters = line.chars();
    let Some(first) = characters.next() else {
        return false;
    };

    first.is_alphabetic() && first.is_uppercase() && characters.next() == Some('.')
}

fn unclosed_quote(text: &str) -> bool {
    text.matches('«').count() > text.matches('»').count()
}

/// `Γ.Ν.Α` on its own is a hospital type with the name still to come.
fn is_bare_abbreviation(text: &str) -> bool {
    let trimmed = text.trim_end_matches('.');
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c == '.' || (c.is_alphabetic() && c.is_uppercase()))
        && trimmed.chars().filter(|c| c.is_alphabetic()).count() <= 5
}

/// Words that cannot end a name: the entry continues on the next line.
fn trails_off(text: &str) -> bool {
    let folded = greek::fold(text);
    folded.split(' ').next_back().is_some_and(is_connector)
}

/// Moves a trailing "open until" qualifier out of the name and returns it.
///
/// Rotas often add a cut-off to a cell — `Γ.Ν.Ε. «ΘΡΙΑΣΙΟ» έως 14:30`, or just a bare
/// `15:00`. Left in place it makes the same hospital look like a different one on every
/// day it appears.
fn split_trailing_hours(name: &mut String) -> Option<String> {
    let trimmed = name.trim_end();
    let cut = trailing_clock(trimmed)?;

    // Keep any preceding "έως"/"μέχρι" with the qualifier rather than the name.
    let head = trimmed[..cut].trim_end();
    let cut = match head.rsplit_once(' ') {
        Some((before, last)) if matches!(greek::fold(last).as_str(), "ΕΩΣ" | "ΜΕΧΡΙ") => {
            before.len()
        }
        _ => cut,
    };

    let qualifier = trimmed[cut..].trim().to_string();
    name.truncate(cut);
    let trimmed_len = name.trim_end().len();
    name.truncate(trimmed_len);

    (!qualifier.is_empty()).then_some(qualifier)
}

/// The byte offset at which a trailing `HH:MM` or `HH.MM` starts, if there is one.
fn trailing_clock(text: &str) -> Option<usize> {
    let last = text.rsplit(' ').next()?;
    let (hours, minutes) = last.split_once([':', '.'])?;

    let looks_like_time = (1..=2).contains(&hours.len())
        && minutes.len() == 2
        && hours.chars().all(|c| c.is_ascii_digit())
        && minutes.chars().all(|c| c.is_ascii_digit());

    looks_like_time.then(|| text.len() - last.len())
}

/// Reads the health centres listed above the table, each with its own opening hours.
fn read_health_centres(page: &pdf::Page, date: Date, extraction: &mut Extraction) {
    for line in &page.lines {
        let text = line.text();
        if !greek::fold(&text).starts_with(HEALTH_CENTRE) {
            continue;
        }

        let Some((name, hours)) = text.split_once(':') else {
            continue;
        };
        let (name, hours) = (name.trim(), hours.trim());
        if name.is_empty() {
            continue;
        }

        let (starts_at, ends_at) = match times::parse_range(hours).and_then(|range| range.on(date))
        {
            Some((start, end)) => (Some(start), Some(end)),
            None => {
                extraction.warnings.push(Warning::warn(
                    "unreadable_hours",
                    format!("{date} {name}: could not read hours from {hours:?}"),
                ));
                (None, None)
            }
        };

        extraction.records.push(Record {
            entity: EntityDraft::new(EntityKind::HealthCentre, greek::matching_key(name), name)
                .identified_by(Identity::Name),
            properties: vec![PropertyDraft {
                on_date: date,
                starts_at,
                ends_at,
                payload: PropertyPayload::HealthCentreOnCall,
            }],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Fetched;
    use jiff::Timestamp;

    const LISTING_FIXTURE: &str = include_str!("../../tests/fixtures/moh_listing.html");

    fn date() -> Date {
        Date::new(2026, 8, 17).expect("valid date")
    }

    fn parse_fixture() -> Extraction {
        let doc = FetchedDoc {
            reference: DocumentRef::new(LISTING, "ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026.pdf").on(date()),
            fetched: Fetched {
                url: LISTING.into(),
                body: include_bytes!("../../tests/fixtures/moh_hospitals_20260817.pdf").to_vec(),
                sha256: "test".into(),
                fetched_at: Timestamp::now(),
                from_cache: true,
            },
        };
        MohAtticaHospitals.parse(&doc).expect("the rota parses")
    }

    fn shifts_for<'a>(extraction: &'a Extraction, hospital: &str) -> Vec<(&'a str, &'a str)> {
        extraction
            .records
            .iter()
            .filter(|record| record.entity.name == hospital)
            .flat_map(|record| &record.properties)
            .filter_map(|property| match &property.payload {
                PropertyPayload::HospitalOnCall { clinic, shift, .. } => {
                    Some((clinic.as_str(), shift.as_str()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_hospital_is_recorded_against_the_shift_column_it_sits_in() {
        let extraction = parse_fixture();
        let shifts = shifts_for(&extraction, "Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»");

        // This hospital holds the main overnight shift, in several specialities.
        assert!(
            shifts.contains(&("Παθολογική", "14:30 – 08:00 επομένης")),
            "got {shifts:?}"
        );
        assert!(shifts.contains(&("Καρδιολογική", "14:30 – 08:00 επομένης")));
        // and never the morning one.
        assert!(
            !shifts
                .iter()
                .any(|(_, shift)| shift.starts_with("08:00 – 14:30"))
        );
    }

    #[test]
    fn a_morning_shift_hospital_is_not_confused_with_an_overnight_one() {
        let extraction = parse_fixture();
        let shifts = shifts_for(&extraction, "Γ.Ο.Ν.Κ. «ΑΓ. ΑΝΑΡΓΥΡΟΙ»");

        assert!(
            shifts.contains(&("Παθολογική", "08:00 – 14:30")),
            "got {shifts:?}"
        );
        assert!(!shifts.iter().any(|(_, shift)| shift.contains("επομένης")));
    }

    #[test]
    fn shift_times_are_anchored_to_the_rota_date() {
        let extraction = parse_fixture();
        let overnight = extraction
            .records
            .iter()
            .filter(|record| record.entity.name == "Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»")
            .flat_map(|record| &record.properties)
            .next()
            .expect("a property");

        assert_eq!(overnight.on_date, date());
        assert_eq!(
            overnight.starts_at.map(|at| at.to_string()).as_deref(),
            Some("2026-08-17T14:30:00")
        );
        // The shift ends the following morning.
        assert_eq!(
            overnight.ends_at.map(|at| at.to_string()).as_deref(),
            Some("2026-08-18T08:00:00")
        );
    }

    #[test]
    fn health_centres_are_recorded_with_their_own_hours() {
        let extraction = parse_fixture();
        let centre = extraction
            .records
            .iter()
            .find(|record| record.entity.name.contains("ΑΜΑΡΟΥΣΙΟΥ"))
            .expect("the Maroussi health centre");

        assert_eq!(centre.entity.kind, EntityKind::HealthCentre);
        assert_eq!(
            centre.properties[0]
                .ends_at
                .map(|at| at.to_string())
                .as_deref(),
            Some("2026-08-17T22:00:00")
        );
    }

    #[test]
    fn the_whole_rota_yields_a_plausible_number_of_duties() {
        let extraction = parse_fixture();
        let hospitals: std::collections::BTreeSet<&str> = extraction
            .records
            .iter()
            .filter(|record| record.entity.kind == EntityKind::Hospital)
            .map(|record| record.entity.name.as_str())
            .collect();

        // Attica's rota covers a handful of hospitals across many specialities.
        assert!(hospitals.len() >= 5, "only found {hospitals:?}");
        assert!(extraction.records.len() > 30);
    }

    fn entries(lines: &[&str]) -> Vec<Entry> {
        split_entries(
            &lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .map(|(_, entry)| entry)
        .collect()
    }

    fn names(lines: &[&str]) -> Vec<String> {
        entries(lines).into_iter().map(|entry| entry.name).collect()
    }

    #[test]
    fn a_wrapped_name_is_one_hospital_and_a_bracketed_remark_is_not_part_of_it() {
        assert_eq!(
            entries(&[
                "Γ.Ν.Ν.ΙΩΝΙΑΣ",
                "«ΚΩΝ/ΠΟΥΛΕΙΟ»",
                "Γ.Ν.Π. «ΤΖΑΝΕΙΟ»",
                "(ΚΑΙ ΔΙΑΒΗΤΟΛΟΓΙΚΟ)",
            ]),
            [
                Entry {
                    name: "Γ.Ν.Ν.ΙΩΝΙΑΣ «ΚΩΝ/ΠΟΥΛΕΙΟ»".to_string(),
                    notes: None
                },
                Entry {
                    name: "Γ.Ν.Π. «ΤΖΑΝΕΙΟ»".to_string(),
                    notes: Some("(ΚΑΙ ΔΙΑΒΗΤΟΛΟΓΙΚΟ)".to_string())
                },
            ]
        );
    }

    #[test]
    fn two_complete_names_in_a_row_stay_separate() {
        // The vertical gap here is the same as within a wrapped name, so only the text
        // can tell these apart.
        assert_eq!(
            names(&["Γ.Ν.Α. «ΠΑΜΜΑΚΑΡΙΣΤΟΣ»", "Γ.Ν.Α. «ΙΠΠΟΚΡΑΤΕΙΟ»"]),
            ["Γ.Ν.Α. «ΠΑΜΜΑΚΑΡΙΣΤΟΣ»", "Γ.Ν.Α. «ΙΠΠΟΚΡΑΤΕΙΟ»"]
        );
    }

    #[test]
    fn a_name_wrapped_without_quotes_is_rejoined() {
        // "Γ.Ν.Α" alone is a hospital type awaiting its name; "έως" cannot end a name.
        assert_eq!(
            entries(&["Γ.Ν.Α", "ΠΑΜΜΑΚΑΡΙΣΤΟΣ έως", "23:00"]),
            [Entry {
                name: "Γ.Ν.Α ΠΑΜΜΑΚΑΡΙΣΤΟΣ".to_string(),
                notes: Some("έως 23:00".to_string()),
            }]
        );
    }

    #[test]
    fn a_hospital_whose_whole_name_is_quoted_stands_on_its_own() {
        // The line above already has its quoted name, so this is a second hospital.
        assert_eq!(
            names(&["Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»", "«ΔΡΟΜΟΚΑΪΤΕΙΟ»"]),
            ["Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»", "«ΔΡΟΜΟΚΑΪΤΕΙΟ»"]
        );
    }

    #[test]
    fn an_entry_reports_the_line_it_started_on() {
        // The caller uses this to attribute a name that wrapped across a row boundary
        // to the speciality it actually belongs to.
        let split = split_entries(&[
            "Γ.Ν.Α. «ΙΠΠΟΚΡΑΤΕΙΟ»".to_string(),
            "Γ.Ν.Α. «Γ.".to_string(),
            "ΓΕΝΝΗΜΑΤΑΣ»".to_string(),
        ]);
        let starts: Vec<usize> = split.iter().map(|(line, _)| *line).collect();
        assert_eq!(starts, [0, 1]);
    }

    #[test]
    fn a_regional_heading_does_not_get_glued_onto_the_hospital_above_it() {
        assert_eq!(
            names(&[
                "Γ.Ν.Α. «ΙΠΠΟΚΡΑΤΕΙΟ»",
                "ΠΕΙΡΑΙΑΣ",
                "Γ.Ν.Ν. «ΑΓ. ΠΑΝΤΕΛΕΗΜΩΝ»"
            ]),
            [
                "Γ.Ν.Α. «ΙΠΠΟΚΡΑΤΕΙΟ»",
                "ΠΕΙΡΑΙΑΣ",
                "Γ.Ν.Ν. «ΑΓ. ΠΑΝΤΕΛΕΗΜΩΝ»"
            ]
        );
    }

    #[test]
    fn a_name_split_mid_quote_is_rejoined() {
        assert_eq!(
            names(&["Γ.Ν.Α. «Γ.", "ΓΕΝΝΗΜΑΤΑΣ»"]),
            ["Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»"]
        );
        assert_eq!(
            names(&["Γ.Ν.Α. «ΚΟΡΓ. ΜΠΕΝ.", "ΕΕΣ»"]),
            ["Γ.Ν.Α. «ΚΟΡΓ. ΜΠΕΝ. ΕΕΣ»"]
        );
    }

    #[test]
    fn a_closing_time_becomes_a_note_so_the_hospital_still_matches() {
        // The same hospital appears with and without a cut-off time on different days.
        assert_eq!(
            entries(&["Γ.Ν.Ε. «ΘΡΙΑΣΙΟ» έως 14:30"]),
            [Entry {
                name: "Γ.Ν.Ε. «ΘΡΙΑΣΙΟ»".to_string(),
                notes: Some("έως 14:30".to_string()),
            }]
        );
        assert_eq!(
            names(&["Γ.Ο.Ν.Κ. «ΑΓ. ΑΝΑΡΓΥΡΟΙ» 15:00"]),
            ["Γ.Ο.Ν.Κ. «ΑΓ. ΑΝΑΡΓΥΡΟΙ»"]
        );
        // A name that merely ends in a number is left alone.
        assert_eq!(names(&["Γ.Ν.Α. «ΚΑΤ» 2"]), ["Γ.Ν.Α. «ΚΑΤ» 2"]);
    }

    #[test]
    fn the_file_listing_yields_pdfs_with_dates_and_revisions() {
        let documents = published_documents(LISTING_FIXTURE).expect("listing parses");

        // Only PDFs, never the Word copies.
        assert!(
            documents
                .iter()
                .all(|doc| doc.label.to_lowercase().ends_with(".pdf"))
        );
        assert_eq!(documents.len(), 4);

        let by_date: Vec<(String, u32)> = documents
            .iter()
            .map(|doc| (doc.date.expect("a date").to_string(), doc.revision.0))
            .collect();
        assert_eq!(
            by_date,
            [
                ("2026-08-17".to_string(), 0),
                ("2026-08-16".to_string(), 0),
                // A misspelled month still resolves, and the reissue is numbered.
                ("2025-09-14".to_string(), 0),
                ("2026-08-06".to_string(), 1),
            ]
        );
    }

    #[test]
    fn a_pdf_with_an_unreadable_filename_borrows_its_siblings_date() {
        let documents = published_documents(LISTING_FIXTURE).expect("listing parses");
        let rescued = documents
            .iter()
            .find(|doc| doc.label.starts_with("ΕΦΗΜΕΡΙΑ"))
            .expect("the unreadable entry");

        assert_eq!(
            rescued.date.map(|date| date.to_string()).as_deref(),
            Some("2026-08-16")
        );
    }

    #[test]
    fn a_download_title_reduces_to_a_filename() {
        assert_eq!(
            filename_from("Download: ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026.pdf (564.1 KB)").as_deref(),
            Some("ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026.pdf")
        );
        assert_eq!(
            filename_from("Download: efimeria_20260817.doc (212.5 KB)").as_deref(),
            Some("efimeria_20260817.doc")
        );
        assert_eq!(filename_from("   ").as_deref(), None);
    }
}

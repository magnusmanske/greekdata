//! On-call pharmacies published by the Pharmaceutical Association of Attica.
//!
//! <https://fsa-efimeries.gr/> is a server-rendered htmx application. The home page
//! lists the dates currently published; posting a date back to `/Home/FilteredHomeResults`
//! returns an HTML fragment of duty cards, each carrying the association's own pharmacy
//! id, address, municipality, pharmacist, opening hours, phone and coordinates.

use super::{Attribution, Ctx, DateWindow, DocumentRef, FetchedDoc, Source};
use crate::{
    Error, Result,
    cache::CachePolicy,
    greek::times,
    model::{
        DataGroup, EntityDraft, EntityKind, Extraction, Identity, Location, PropertyDraft,
        PropertyPayload, Record, Warning,
    },
};
use async_trait::async_trait;
use jiff::civil::Date;
use scraper::{ElementRef, Html, Selector};

const HOME: &str = "https://fsa-efimeries.gr/";
const RESULTS: &str = "https://fsa-efimeries.gr/Home/FilteredHomeResults";

pub struct FsaPharmacies;

#[async_trait]
impl Source for FsaPharmacies {
    fn id(&self) -> &'static str {
        "fsa-attica-pharmacies"
    }

    fn group(&self) -> DataGroup {
        DataGroup::Pharmacies
    }

    fn attribution(&self) -> Attribution {
        Attribution {
            publisher: "Φαρμακευτικός Σύλλογος Αττικής",
            homepage: HOME,
            terms: "Public-service duty roster, republished with attribution.",
        }
    }

    async fn discover(&self, ctx: &Ctx, window: DateWindow) -> Result<Vec<DocumentRef>> {
        // The home page is the only statement of which dates exist, and it changes
        // daily, so it is always revalidated.
        let home = ctx
            .fetcher
            .get_with(self.id(), HOME, CachePolicy::Revalidate)
            .await?;
        let today = super::today();

        Ok(published_dates(&home.text())?
            .into_iter()
            .filter(|date| window.contains(*date))
            .map(|date| {
                DocumentRef::new(RESULTS, format!("Εφημερεύοντα φαρμακεία {date}"))
                    .on(date)
                    .with_form(vec![
                        ("Date".into(), date.to_string()),
                        ("IsOpen".into(), "false".into()),
                        ("address".into(), String::new()),
                    ])
                    // A rota that has not happened yet can still be changed.
                    .volatile(date >= today)
            })
            .collect())
    }

    fn parse(&self, doc: &FetchedDoc) -> Result<Extraction> {
        let date = doc
            .reference
            .date
            .ok_or_else(|| Error::parse("fsa document", "no date attached to the request"))?;
        let selectors = Selectors::new()?;
        let html = Html::parse_fragment(&doc.fetched.text());

        let mut extraction = Extraction::default();
        for card in html.select(&selectors.card) {
            match read_card(&selectors, card, date) {
                Ok((record, warnings)) => {
                    extraction.records.push(record);
                    extraction.warnings.extend(warnings);
                }
                Err(warning) => extraction.warnings.push(warning),
            }
        }

        if extraction.records.is_empty() {
            extraction.warnings.push(Warning::warn(
                "empty_document",
                format!("no pharmacies listed for {date}"),
            ));
        }

        Ok(extraction)
    }
}

/// The CSS selectors this parser needs, built once per document so a malformed selector
/// surfaces as an error rather than a panic.
struct Selectors {
    card: Selector,
    title: Selector,
    subtitle: Selector,
    detail: Selector,
    phone: Selector,
    map_link: Selector,
    date_option: Selector,
}

impl Selectors {
    fn new() -> Result<Self> {
        Ok(Self {
            card: selector("div.card[id]")?,
            title: selector(".card-title h6")?,
            subtitle: selector(".card-subtitle")?,
            detail: selector(".card-text span")?,
            phone: selector(".card-text h6")?,
            map_link: selector(".card-footer a[href*='query=']")?,
            date_option: selector("select#Date option")?,
        })
    }
}

fn selector(pattern: &str) -> Result<Selector> {
    Selector::parse(pattern)
        .map_err(|error| Error::parse("css selector", format!("`{pattern}`: {error}")))
}

/// Reads the dates the association currently publishes from the home page's date picker.
fn published_dates(html: &str) -> Result<Vec<Date>> {
    let selectors = Selectors::new()?;
    let document = Html::parse_document(html);

    Ok(document
        .select(&selectors.date_option)
        .filter_map(|option| option.value().attr("value"))
        .filter_map(|value| value.trim().parse::<Date>().ok())
        .collect())
}

/// Turns one duty card into a record. Returns `Err` only when the card is unusable.
fn read_card(
    selectors: &Selectors,
    card: ElementRef<'_>,
    date: Date,
) -> std::result::Result<(Record, Vec<Warning>), Warning> {
    let local_id = card
        .value()
        .attr("id")
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Warning::warn("card_without_id", "duty card has no pharmacy id"))?;

    let address = text_of(card, &selectors.title);
    let municipality = text_of(card, &selectors.subtitle);
    let phone = text_of(card, &selectors.phone);

    // The two detail lines are the pharmacist and the opening hours, in that order.
    // Keeping the order matters: it preserves hours we cannot parse, such as "ΚΑΤΟΠΙΝ
    // ΣΥΝΕΝΝΟΗΣΗΣ" (by arrangement). Only a line that plainly reads as a time range in
    // the wrong position overrides it.
    let lines: Vec<String> = card
        .select(&selectors.detail)
        .map(|line| collapse(&line.text().collect::<String>()))
        .filter(|line| !line.is_empty())
        .collect();
    let reads_as_hours = |line: &String| !times::parse_ranges(line).is_empty();
    let (pharmacist, hours_text) = match lines.as_slice() {
        [] => (None, None),
        [only] if reads_as_hours(only) => (None, Some(only.clone())),
        [only] => (Some(only.clone()), None),
        [first, second, ..] if reads_as_hours(first) && !reads_as_hours(second) => {
            (Some(second.clone()), Some(first.clone()))
        }
        [first, second, ..] => (Some(first.clone()), Some(second.clone())),
    };

    let mut warnings = Vec::new();
    let location = card
        .select(&selectors.map_link)
        .find_map(|link| link.value().attr("href"))
        .and_then(coordinates_from_map_link);
    if location.is_none() {
        warnings.push(Warning::warn(
            "missing_location",
            format!("pharmacy {local_id} has no usable coordinates"),
        ));
    }

    // A split shift is several duty windows on one day, so it becomes several
    // properties. A pharmacy whose hours we cannot read is still recorded as on duty,
    // just without times.
    let windows: Vec<(Option<_>, Option<_>)> = hours_text
        .as_deref()
        .map(times::parse_ranges)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|range| range.on(date))
        .map(|(start, end)| (Some(start), Some(end)))
        .collect();
    if windows.is_empty() {
        warnings.push(Warning::warn(
            "unreadable_hours",
            format!(
                "pharmacy {local_id} on {date}: could not read hours from {:?}",
                hours_text.as_deref().unwrap_or("")
            ),
        ));
    }

    // The pharmacist's name is how these pharmacies are known; the address is the
    // fallback when the association omitted it.
    let name = pharmacist
        .clone()
        .or_else(|| address.clone())
        .unwrap_or_else(|| format!("Φαρμακείο {local_id}"));

    let mut entity =
        EntityDraft::new(EntityKind::Pharmacy, local_id, name).identified_by(Identity::SourceKey);
    entity.address = address;
    entity.municipality = municipality;
    entity.phone = phone;
    entity.location = location;

    let payload = PropertyPayload::PharmacyOnCall {
        pharmacist,
        hours_text,
    };
    let properties = if windows.is_empty() {
        vec![PropertyDraft {
            on_date: date,
            starts_at: None,
            ends_at: None,
            payload,
        }]
    } else {
        windows
            .into_iter()
            .map(|(starts_at, ends_at)| PropertyDraft {
                on_date: date,
                starts_at,
                ends_at,
                payload: payload.clone(),
            })
            .collect()
    };

    Ok((Record { entity, properties }, warnings))
}

/// Pulls coordinates out of the card's "navigate here" link, whose query parameter holds
/// `lat,lon` (usually percent-encoded).
fn coordinates_from_map_link(href: &str) -> Option<Location> {
    let (_, query) = href.split_once("query=")?;
    let query = query
        .split('&')
        .next()?
        .replace("%2C", ",")
        .replace("%2c", ",");
    let (lat, lon) = query.split_once(',')?;
    Location::new(lat.trim().parse().ok()?, lon.trim().parse().ok()?).ok()
}

fn text_of(card: ElementRef<'_>, selector: &Selector) -> Option<String> {
    card.select(selector)
        .map(|element| collapse(&element.text().collect::<String>()))
        .find(|text| !text.is_empty())
}

/// Collapses the whitespace that server-rendered HTML leaves behind.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Fetched;
    use jiff::Timestamp;

    const CARD_FRAGMENT: &str = include_str!("../../tests/fixtures/fsa_duty_cards.html");
    const HOME_FRAGMENT: &str = include_str!("../../tests/fixtures/fsa_home.html");

    fn date() -> Date {
        Date::new(2026, 8, 17).expect("valid date")
    }

    fn parse_fixture() -> Extraction {
        let doc = FetchedDoc {
            reference: DocumentRef::new(RESULTS, "fixture").on(date()),
            fetched: Fetched {
                url: RESULTS.into(),
                body: CARD_FRAGMENT.as_bytes().to_vec(),
                sha256: "test".into(),
                fetched_at: Timestamp::now(),
                from_cache: true,
            },
        };
        FsaPharmacies.parse(&doc).expect("fixture parses")
    }

    #[test]
    fn reads_every_duty_card_in_the_fragment() {
        let extraction = parse_fixture();
        assert_eq!(extraction.records.len(), 3);
    }

    #[test]
    fn reads_a_cards_full_detail() {
        let extraction = parse_fixture();
        let record = &extraction.records[0];

        assert_eq!(record.entity.local_id, "288990");
        assert_eq!(record.entity.name, "ΑΝΤΩΝΟΠΟΥΛΟΥ ΒΑΣΙΛΙΚΗ");
        assert_eq!(record.entity.address.as_deref(), Some("ΔΑΙΔΑΛΟΥ 40"));
        assert_eq!(record.entity.municipality.as_deref(), Some("ΑΓ.ΔΗΜΗΤΡΙΟΣ"));
        assert_eq!(record.entity.phone.as_deref(), Some("2109888520"));

        let location = record.entity.location.expect("coordinates");
        assert!((location.lat() - 37.921_861_5).abs() < 1e-6);
        assert!((location.lon() - 23.721_171_0).abs() < 1e-6);
    }

    #[test]
    fn colloquial_hours_become_real_timestamps() {
        let extraction = parse_fixture();
        let property = &extraction.records[0].properties[0];

        assert_eq!(property.on_date, date());
        assert_eq!(
            property.starts_at.map(|at| at.to_string()).as_deref(),
            Some("2026-08-17T08:00:00")
        );
        assert_eq!(
            property.ends_at.map(|at| at.to_string()).as_deref(),
            Some("2026-08-17T23:00:00")
        );
        assert!(matches!(
            &property.payload,
            PropertyPayload::PharmacyOnCall { hours_text: Some(text), .. }
                if text == "8 ΠΡΩΙ - 11 ΒΡΑΔΥ"
        ));
    }

    #[test]
    fn a_split_shift_becomes_two_duty_windows_the_second_running_overnight() {
        let extraction = parse_fixture();
        let record = &extraction.records[1];
        assert_eq!(record.entity.local_id, "288995");
        assert_eq!(record.properties.len(), 2);

        let stamps: Vec<(String, String)> = record
            .properties
            .iter()
            .map(|property| {
                (
                    property.starts_at.expect("start").to_string(),
                    property.ends_at.expect("end").to_string(),
                )
            })
            .collect();
        assert_eq!(
            stamps,
            [
                (
                    "2026-08-17T08:00:00".to_string(),
                    "2026-08-17T14:00:00".to_string()
                ),
                (
                    "2026-08-17T17:00:00".to_string(),
                    "2026-08-18T08:00:00".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_card_with_unreadable_hours_is_kept_flagged_and_quoted_verbatim() {
        let extraction = parse_fixture();
        let record = &extraction.records[2];

        // The pharmacy is still recorded as on duty for the day, with no times.
        assert_eq!(record.entity.local_id, "288999");
        assert_eq!(record.properties.len(), 1);
        assert_eq!(record.properties[0].starts_at, None);

        // The published wording is kept even though we could not turn it into times.
        assert!(matches!(
            &record.properties[0].payload,
            PropertyPayload::PharmacyOnCall { hours_text: Some(text), pharmacist: Some(who) }
                if text == "ΚΑΤΟΠΙΝ ΣΥΝΕΝΝΟΗΣΗΣ" && who == "ΑΝΤΩΝΟΠΟΥΛΟΥ ΒΑΣΙΛΙΚΗ"
        ));
        assert!(
            extraction
                .warnings
                .iter()
                .any(|warning| warning.code == "unreadable_hours"
                    && warning.detail.contains("288999"))
        );
    }

    #[test]
    fn a_card_without_coordinates_is_flagged_rather_than_dropped() {
        let extraction = parse_fixture();
        assert!(extraction.records[2].entity.location.is_none());
        assert!(
            extraction
                .warnings
                .iter()
                .any(|warning| warning.code == "missing_location")
        );
    }

    #[test]
    fn pharmacies_are_identified_by_the_associations_key_not_by_name() {
        // Two pharmacists can share a name, so name matching must be off for this source.
        let extraction = parse_fixture();
        assert!(
            extraction
                .records
                .iter()
                .all(|record| record.entity.identity == Identity::SourceKey)
        );
    }

    #[test]
    fn the_home_page_yields_the_published_date_range() {
        let dates = published_dates(HOME_FRAGMENT).expect("dates parse");
        assert_eq!(dates.first(), Some(&date()));
        assert_eq!(dates.len(), 4);
    }

    #[test]
    fn map_links_yield_coordinates_however_they_are_encoded() {
        let encoded = coordinates_from_map_link(
            "https://www.google.gr/maps/search/?api=1&query=37.9218615%2C23.7211710",
        );
        let plain = coordinates_from_map_link(
            "https://www.google.gr/maps/search/?api=1&query=37.9218615,23.7211710",
        );
        assert_eq!(encoded, plain);
        assert!(encoded.is_some());

        assert_eq!(coordinates_from_map_link("https://example.org/"), None);
        assert_eq!(coordinates_from_map_link("?query=not,coordinates"), None);
    }
}

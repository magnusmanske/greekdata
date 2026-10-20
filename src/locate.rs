//! Finding coordinates for entities that arrive without any.
//!
//! Pharmacy rotas carry coordinates; hospital rotas are names on a page. To put a
//! hospital on a map its location has to come from somewhere else, and here that is
//! Wikidata — a secondary source, used because no official Greek dataset offers this.
//!
//! Matching a ministry abbreviation like `Γ.Ν.Α. «ΑΓ. ΑΝΑΡΓΥΡΟΙ»` to
//! `Γενικό Ογκολογικό Νοσοκομείο Κηφισίας Άγιοι Ανάργυροι` is inexact, and getting it
//! wrong would send somebody to the wrong hospital. So the rules are deliberately
//! strict: only the distinctive quoted part of the name is matched, only against
//! candidates inside Attica, and only when exactly one candidate fits. Everything
//! matched records the Wikidata item it came from, so any claim here can be checked.

use crate::{
    Error, Result,
    cache::CachePolicy,
    db::Db,
    greek,
    model::{EntityKind, Severity, Warning},
    sources::Ctx,
};
use wikimisc::{
    entity_id::ItemId,
    lat_lon::{BoundingBox, LatLon},
    sparql_results::SparqlApiResult,
    sparql_value::SparqlValue,
};

/// Recorded against a located entity so the API and the map can say where the
/// coordinates came from.
pub const LOCATION_SOURCE: &str = "wikidata";

/// WDQS v1 (Blazegraph). The query is plain SPARQL, so it also runs on v2 (QLever,
/// `query-next.wikidata.org`), which refuses user agents it has not allow-listed until
/// its public launch.
const ENDPOINT: &str = "https://query.wikidata.org/sparql";

/// Hospitals in Greece that have coordinates, with their Greek names and aliases.
const QUERY: &str = r#"SELECT ?item ?label ?alias ?coord WHERE {
  ?item wdt:P31/wdt:P279* wd:Q16917 .
  ?item wdt:P17 wd:Q41 .
  ?item wdt:P625 ?coord .
  OPTIONAL { ?item rdfs:label ?label . FILTER(LANG(?label) = "el") }
  OPTIONAL { ?item skos:altLabel ?alias . FILTER(LANG(?alias) = "el") }
}"#;

/// Attica, generously. Restricting candidates to the region the rota covers is what
/// stops an Athens hospital matching its Thessaloniki namesake.
const ATTICA: BoundingBox = BoundingBox {
    min_lat: 37.5,
    max_lat: 38.4,
    min_lon: 23.0,
    max_lon: 24.2,
};

/// What a locating run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LocateReport {
    pub located: usize,
    pub ambiguous: usize,
    pub unmatched: usize,
}

/// A place Wikidata knows the position of.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub qid: ItemId,
    pub names: Vec<String>,
    pub location: LatLon,
}

/// Gives coordinates to stored hospitals that have none.
pub async fn hospitals(ctx: &Ctx) -> Result<LocateReport> {
    let response = ctx
        .fetcher
        .get_with(LOCATION_SOURCE, &query_url(), CachePolicy::PreferCache)
        .await?;
    let candidates = parse_candidates(&response.text())?;
    let inside: Vec<Candidate> = candidates
        .into_iter()
        .filter(|candidate| ATTICA.contains(&candidate.location))
        .collect();
    tracing::info!(candidates = inside.len(), "wikidata hospitals in Attica");

    let unlocated: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM entity WHERE kind = ?1 AND (lat IS NULL OR lon IS NULL)",
    )
    .bind(EntityKind::Hospital.as_str())
    .fetch_all(ctx.db.pool())
    .await?;

    let mut report = LocateReport::default();
    let mut warnings = Vec::new();

    for (id, name) in unlocated {
        match resolve(&name, &inside) {
            Match::One(candidate) => {
                set_location(&ctx.db, id, candidate).await?;
                report.located += 1;
                warnings.push(Warning::new(
                    Severity::Info,
                    "located",
                    format!(
                        "`{name}` placed at {}, {} from wikidata:{}",
                        candidate.location.lat, candidate.location.lon, candidate.qid
                    ),
                ));
            }
            Match::Several(qids) => {
                report.ambiguous += 1;
                warnings.push(Warning::warn(
                    "ambiguous_location",
                    format!("`{name}` matches {} Wikidata items: {qids:?}", qids.len()),
                ));
            }
            Match::None => {
                report.unmatched += 1;
                warnings.push(Warning::new(
                    Severity::Info,
                    "no_location",
                    format!("`{name}` has no coordinates and no Wikidata match"),
                ));
            }
        }
    }

    crate::db::ingest::record_issues(&ctx.db, LOCATION_SOURCE, None, &warnings).await?;
    Ok(report)
}

async fn set_location(db: &Db, entity_id: i64, candidate: &Candidate) -> Result<()> {
    let mut tx = db.pool().begin().await?;

    sqlx::query("UPDATE entity SET lat = ?1, lon = ?2, location_source = ?3 WHERE id = ?4")
        .bind(candidate.location.lat)
        .bind(candidate.location.lon)
        .bind(LOCATION_SOURCE)
        .bind(entity_id)
        .execute(&mut *tx)
        .await?;

    // Recording the item makes every placement checkable after the fact.
    sqlx::query(
        "INSERT INTO entity_external_id (entity_id, scheme, value) VALUES (?1, ?2, ?3)
         ON CONFLICT DO NOTHING",
    )
    .bind(entity_id)
    .bind(LOCATION_SOURCE)
    .bind(candidate.qid.to_string())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// The outcome of matching one name.
#[derive(Debug, PartialEq)]
enum Match<'a> {
    One(&'a Candidate),
    Several(Vec<ItemId>),
    None,
}

/// Matches a ministry hospital name against the candidates.
fn resolve<'a>(name: &str, candidates: &'a [Candidate]) -> Match<'a> {
    let Some(wanted) = distinctive_words(name) else {
        return Match::None;
    };

    let hits: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| {
            candidate
                .names
                .iter()
                .any(|known| all_words_present(&wanted, known))
        })
        .collect();

    match hits.as_slice() {
        [only] => Match::One(only),
        [] => Match::None,
        several => Match::Several(several.iter().map(|c| c.qid).collect()),
    }
}

/// A word of the name being looked for, and whether the source abbreviated it.
#[derive(Debug, PartialEq)]
struct Wanted {
    word: String,
    abbreviated: bool,
}

/// The words inside the quotes, which are what actually name the hospital.
///
/// `Γ.Ν.Α. «ΑΓ. ΑΝΑΡΓΥΡΟΙ»` is a general hospital of Athens called Άγιοι Ανάργυροι; only
/// the second part distinguishes it from every other general hospital of Athens. A
/// trailing stop marks an abbreviation, so `ΑΓ.` is allowed to match `Άγιοι`.
fn distinctive_words(name: &str) -> Option<Vec<Wanted>> {
    let (_, after) = name.split_once('«')?;
    let quoted = after.split_once('»').map_or(after, |(inside, _)| inside);

    let mut wanted: Vec<Wanted> = Vec::new();
    for token in quoted.split_whitespace() {
        // A hyphenated compound is several words, each of which must be present.
        let folded = greek::fold(token);
        let mut words = folded.split(' ').filter(|word| !word.is_empty()).peekable();
        while let Some(word) = words.next() {
            wanted.push(Wanted {
                word: word.to_string(),
                // Only the last part of the token can be the abbreviated one.
                abbreviated: token.ends_with('.') && words.peek().is_none(),
            });
        }
    }

    // Too little to go on is worse than nothing: refuse rather than guess.
    let letters: usize = wanted.iter().map(|w| w.word.chars().count()).sum();
    (letters >= 4).then_some(wanted)
}

/// Whether every wanted word appears in `known`, abbreviations matching as prefixes.
fn all_words_present(wanted: &[Wanted], known: &str) -> bool {
    let words: Vec<String> = greek::fold(known)
        .split(' ')
        .filter(|word| !word.is_empty())
        .map(String::from)
        .collect();

    wanted.iter().all(|want| {
        words.iter().any(|word| {
            if want.abbreviated {
                word.starts_with(&want.word)
            } else {
                *word == want.word
            }
        })
    })
}

fn query_url() -> String {
    format!(
        "{ENDPOINT}?format=json&query={}",
        crate::cache::percent_encode(QUERY)
    )
}

/// Folds the SPARQL rows, one per name, into one candidate per item.
pub fn parse_candidates(json: &str) -> Result<Vec<Candidate>> {
    let response: SparqlApiResult = serde_json::from_str(json)?;
    let mut candidates: Vec<Candidate> = Vec::new();

    for row in response.bindings() {
        let Some(qid) = row
            .get("item")
            .and_then(SparqlValue::entity_id)
            .and_then(|id| id.as_item())
        else {
            continue;
        };
        let Some(SparqlValue::Location(location)) = row.get("coord") else {
            continue;
        };

        let existing = candidates.iter_mut().find(|entry| entry.qid == qid);
        let candidate = match existing {
            Some(entry) => entry,
            None => {
                candidates.push(Candidate {
                    qid,
                    names: Vec::new(),
                    location: location.clone(),
                });
                candidates.last_mut().ok_or_else(|| {
                    Error::parse("wikidata", "candidate vanished after being pushed")
                })?
            }
        };

        for name in [row.get("label"), row.get("alias")].into_iter().flatten() {
            if let SparqlValue::Literal(name) = name
                && !candidate.names.contains(name)
            {
                candidate.names.push(name.clone());
            }
        }
    }

    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(qid: &str, names: &[&str]) -> Candidate {
        Candidate {
            qid: qid.parse().expect("a valid item ID"),
            names: names.iter().map(|name| name.to_string()).collect(),
            location: LatLon::new(37.98, 23.75),
        }
    }

    fn attica() -> Vec<Candidate> {
        vec![
            candidate("Q1", &["Γενικό Νοσοκομείο Αθηνών «Αλεξάνδρα»"]),
            candidate(
                "Q2",
                &["Γενικό Ογκολογικό Νοσοκομείο Κηφισίας Άγιοι Ανάργυροι"],
            ),
            candidate(
                "Q3",
                &["Γενικό Κρατικό Νοσοκομείο Αθηνών «Γιώργος Γεννηματάς»"],
            ),
            candidate("Q4", &["Σισμανόγλειο Νοσοκομείο"]),
            candidate("Q5", &["Σισμανόγλειο-Αμαλία Φλέμινγκ Γενικό Νοσοκομείο"]),
        ]
    }

    fn matched(name: &str) -> Option<String> {
        match resolve(name, &attica()) {
            Match::One(candidate) => Some(candidate.qid.to_string()),
            _ => None,
        }
    }

    #[test]
    fn a_plain_quoted_name_matches() {
        assert_eq!(matched("Γ.Ν.Α. «ΑΛΕΞΑΝΔΡΑ»").as_deref(), Some("Q1"));
    }

    #[test]
    fn an_abbreviated_word_matches_the_word_it_abbreviates() {
        // ΑΓ. is Άγιοι; Γ. is Γιώργος.
        assert_eq!(matched("Γ.Ο.Ν.Κ. «ΑΓ. ΑΝΑΡΓΥΡΟΙ»").as_deref(), Some("Q2"));
        assert_eq!(matched("Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»").as_deref(), Some("Q3"));
    }

    #[test]
    fn an_unabbreviated_word_must_match_in_full() {
        // "ΑΝΑΡΓ" is not an abbreviation, so it must not match "Ανάργυροι".
        assert_eq!(matched("Γ.Ο.Ν.Κ. «ΑΓ. ΑΝΑΡΓ»"), None);
    }

    #[test]
    fn an_ambiguous_name_is_refused_rather_than_guessed() {
        let candidates = attica();
        let outcome = resolve("Γ.Ν.Α. «ΣΙΣΜΑΝΟΓΛΕΙΟ»", &candidates);
        assert!(
            matches!(&outcome, Match::Several(qids) if qids.len() == 2),
            "got {outcome:?}"
        );
    }

    #[test]
    fn a_hyphenated_compound_needs_both_of_its_parts() {
        let candidates = vec![
            candidate(
                "Q6",
                &["Νοσοκομείο Ερυθρός Σταυρός", "Κοργιαλένειο-Μπενάκειο"],
            ),
            candidate("Q7", &["Μπενάκειο κάτι άλλο"]),
        ];
        assert!(matches!(
            resolve("Γ.Ν.Α. «ΚΟΡΓΙΑΛΕΝΕΙΟ-ΜΠΕΝΑΚΕΙΟ» Ε.Ε.Σ.", &candidates),
            Match::One(found) if found.qid == "Q6"
        ));
    }

    #[test]
    fn a_name_with_nothing_distinctive_is_not_matched() {
        let candidates = attica();
        assert_eq!(resolve("ΠΕΙΡΑΙΑΣ", &candidates), Match::None);
        assert_eq!(resolve("Γ.Ν.Α. «Γ.", &candidates), Match::None);
        assert_eq!(resolve("", &candidates), Match::None);
    }

    #[test]
    fn a_name_that_matches_nothing_known_is_not_forced() {
        let candidates = attica();
        assert_eq!(resolve("Π.Γ.Ν.Α. «ΑΙΓΗΝΙΤΕΙΟ»", &candidates), Match::None);
    }

    #[test]
    fn attica_bounds_exclude_the_rest_of_greece() {
        // Athens is in; Thessaloniki and Heraklion are not.
        assert!(ATTICA.contains(&LatLon::new(37.98, 23.73)));
        assert!(!ATTICA.contains(&LatLon::new(40.64, 22.94)));
        assert!(!ATTICA.contains(&LatLon::new(35.34, 25.14)));
    }

    /// One WDQS result row, as the endpoint writes it.
    fn row(item: &str, name: Option<(&str, &str)>, coord: &str) -> String {
        let name = name.map_or(String::new(), |(var, text)| {
            format!(r#""{var}":{{"xml:lang":"el","type":"literal","value":"{text}"}},"#)
        });
        format!(
            r#"{{"item":{{"type":"uri","value":"{item}"}},{name}
               "coord":{{"datatype":"http://www.opengis.net/ont/geosparql#wktLiteral",
                         "type":"literal","value":"{coord}"}}}}"#
        )
    }

    fn response(rows: &[String]) -> String {
        format!(
            r#"{{"head":{{"vars":["item","label","alias","coord"]}},
                "results":{{"bindings":[{}]}}}}"#,
            rows.join(",")
        )
    }

    #[test]
    fn sparql_rows_fold_into_one_candidate_per_item() {
        let item = "http://www.wikidata.org/entity/Q7";
        let coord = "Point(23.73158 38.01425)";
        let json = response(&[
            row(item, Some(("label", "Νοσοκομείο Παμμακάριστος")), coord),
            row(item, Some(("alias", "Παμμακάριστος")), coord),
        ]);

        let candidates = parse_candidates(&json).expect("parses");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].qid, "Q7");
        assert_eq!(
            candidates[0].names,
            ["Νοσοκομείο Παμμακάριστος", "Παμμακάριστος"]
        );
        // WKT is longitude first.
        assert!((candidates[0].location.lat - 38.01425).abs() < 1e-6);
        assert!((candidates[0].location.lon - 23.73158).abs() < 1e-6);
    }

    #[test]
    fn rows_without_an_item_or_a_point_are_skipped() {
        let json = response(&[
            // QLever's spelling is read too.
            row(
                "http://www.wikidata.org/entity/Q1",
                None,
                "POINT(23.7 37.9)",
            ),
            row(
                "http://www.wikidata.org/entity/P2",
                None,
                "Point(23.7 37.9)",
            ),
            row("http://example.org/Q3", None, "Point(23.7 37.9)"),
            row("http://www.wikidata.org/entity/Q4", None, "Point(23.7)"),
            row(
                "http://www.wikidata.org/entity/Q5",
                None,
                "<http://www.wikidata.org/entity/Q405> Point(23.7 37.9)",
            ),
        ]);

        let candidates = parse_candidates(&json).expect("parses");
        let qids: Vec<String> = candidates.iter().map(|c| c.qid.to_string()).collect();
        assert_eq!(qids, ["Q1"]);
    }
}

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
use serde::Deserialize;

/// Recorded against a located entity so the API and the map can say where the
/// coordinates came from.
pub const LOCATION_SOURCE: &str = "wikidata";

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
const ATTICA: Bounds = Bounds {
    min_lat: 37.5,
    max_lat: 38.4,
    min_lon: 23.0,
    max_lon: 24.2,
};

struct Bounds {
    min_lat: f64,
    max_lat: f64,
    min_lon: f64,
    max_lon: f64,
}

impl Bounds {
    fn contains(&self, lat: f64, lon: f64) -> bool {
        (self.min_lat..=self.max_lat).contains(&lat) && (self.min_lon..=self.max_lon).contains(&lon)
    }
}

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
    pub qid: String,
    pub names: Vec<String>,
    pub lat: f64,
    pub lon: f64,
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
        .filter(|candidate| ATTICA.contains(candidate.lat, candidate.lon))
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
                        candidate.lat, candidate.lon, candidate.qid
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
        .bind(candidate.lat)
        .bind(candidate.lon)
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
    .bind(&candidate.qid)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// The outcome of matching one name.
#[derive(Debug, PartialEq)]
enum Match<'a> {
    One(&'a Candidate),
    Several(Vec<&'a str>),
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
        several => Match::Several(several.iter().map(|c| c.qid.as_str()).collect()),
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

/// The subset of the SPARQL JSON results we need.
#[derive(Deserialize)]
struct SparqlResponse {
    results: SparqlResults,
}

#[derive(Deserialize)]
struct SparqlResults {
    bindings: Vec<SparqlBinding>,
}

#[derive(Deserialize)]
struct SparqlBinding {
    item: SparqlValue,
    #[serde(default)]
    label: Option<SparqlValue>,
    #[serde(default)]
    alias: Option<SparqlValue>,
    coord: SparqlValue,
}

#[derive(Deserialize)]
struct SparqlValue {
    value: String,
}

/// Folds the SPARQL rows, one per name, into one candidate per item.
pub fn parse_candidates(json: &str) -> Result<Vec<Candidate>> {
    let response: SparqlResponse = serde_json::from_str(json)?;
    let mut candidates: Vec<Candidate> = Vec::new();

    for binding in response.results.bindings {
        let Some(qid) = binding.item.value.rsplit('/').next().map(String::from) else {
            continue;
        };
        let Some((lat, lon)) = parse_point(&binding.coord.value) else {
            continue;
        };

        let existing = candidates.iter_mut().find(|entry| entry.qid == qid);
        let candidate = match existing {
            Some(entry) => entry,
            None => {
                candidates.push(Candidate {
                    qid,
                    names: Vec::new(),
                    lat,
                    lon,
                });
                candidates.last_mut().ok_or_else(|| {
                    Error::parse("wikidata", "candidate vanished after being pushed")
                })?
            }
        };

        for name in [binding.label, binding.alias].into_iter().flatten() {
            if !candidate.names.contains(&name.value) {
                candidate.names.push(name.value);
            }
        }
    }

    Ok(candidates)
}

/// `Point(23.7554 37.98018)` is longitude then latitude, in that order.
fn parse_point(literal: &str) -> Option<(f64, f64)> {
    let inside = literal.trim().strip_prefix("Point(")?.strip_suffix(')')?;
    let (lon, lat) = inside.split_once(' ')?;
    Some((lat.trim().parse().ok()?, lon.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(qid: &str, names: &[&str]) -> Candidate {
        Candidate {
            qid: qid.into(),
            names: names.iter().map(|name| name.to_string()).collect(),
            lat: 37.98,
            lon: 23.75,
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
            Match::One(candidate) => Some(candidate.qid.clone()),
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
        assert!(ATTICA.contains(37.98, 23.73));
        assert!(!ATTICA.contains(40.64, 22.94));
        assert!(!ATTICA.contains(35.34, 25.14));
    }

    #[test]
    fn coordinates_are_read_longitude_first() {
        assert_eq!(
            parse_point("Point(23.7554 37.98018)"),
            Some((37.98018, 23.7554))
        );
        assert_eq!(parse_point("Point(23.7554)"), None);
        assert_eq!(parse_point("somewhere"), None);
    }

    #[test]
    fn sparql_rows_fold_into_one_candidate_per_item() {
        let json = r#"{"results":{"bindings":[
            {"item":{"value":"http://www.wikidata.org/entity/Q7"},
             "label":{"value":"Νοσοκομείο Παμμακάριστος"},
             "coord":{"value":"Point(23.73158 38.01425)"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q7"},
             "alias":{"value":"Παμμακάριστος"},
             "coord":{"value":"Point(23.73158 38.01425)"}}
        ]}}"#;

        let candidates = parse_candidates(json).expect("parses");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].qid, "Q7");
        assert_eq!(
            candidates[0].names,
            ["Νοσοκομείο Παμμακάριστος", "Παμμακάριστος"]
        );
        assert!((candidates[0].lat - 38.01425).abs() < 1e-6);
    }
}

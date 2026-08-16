//! The extensibility seam.
//!
//! A data group is added by implementing [`Source`] and registering it below. Fetching,
//! caching, provenance, entity resolution, supersession and error reporting are handled
//! once, here, for every source.

pub mod fsa_pharmacies;
pub mod moh_hospitals;

use crate::{
    Error, Result,
    cache::{CachePolicy, Fetched, Fetcher},
    config::Config,
    db::{Db, ingest},
    model::{DataGroup, Extraction, Revision, Severity, Warning},
};
use async_trait::async_trait;
use jiff::civil::Date;

/// Who published a document, so every API response can credit its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribution {
    /// The publishing body, in Greek.
    pub publisher: &'static str,
    /// The page a human should visit to see the original.
    pub homepage: &'static str,
    /// How this data may be used, as far as the publisher states it.
    pub terms: &'static str,
}

/// An inclusive range of dates to ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateWindow {
    pub from: Date,
    pub to: Date,
}

impl DateWindow {
    pub fn new(from: Date, to: Date) -> Self {
        let (from, to) = if from <= to { (from, to) } else { (to, from) };
        Self { from, to }
    }

    pub fn contains(&self, date: Date) -> bool {
        (self.from..=self.to).contains(&date)
    }

    /// Every date in the window, earliest first.
    pub fn dates(&self) -> impl Iterator<Item = Date> + '_ {
        std::iter::successors(Some(self.from), |date| date.tomorrow().ok())
            .take_while(|date| *date <= self.to)
    }
}

/// A document a source knows how to fetch and parse.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentRef {
    pub url: String,
    /// The date the document is about, when that is knowable before fetching it.
    pub date: Option<Date>,
    pub revision: Revision,
    /// Human-readable identification, normally the published filename or label.
    pub label: String,
    /// Form fields, when the document is only reachable through a POST.
    pub form: Vec<(String, String)>,
    /// Whether the content may still change upstream. A rota for a future date can be
    /// revised; a rota for last Tuesday cannot.
    pub volatile: bool,
}

impl DocumentRef {
    pub fn new(url: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            date: None,
            revision: Revision::ORIGINAL,
            label: label.into(),
            form: Vec::new(),
            volatile: false,
        }
    }

    pub fn on(mut self, date: Date) -> Self {
        self.date = Some(date);
        self
    }

    pub fn revision(mut self, revision: Revision) -> Self {
        self.revision = revision;
        self
    }

    pub fn with_form(mut self, form: Vec<(String, String)>) -> Self {
        self.form = form;
        self
    }

    pub fn volatile(mut self, volatile: bool) -> Self {
        self.volatile = volatile;
        self
    }

    /// A stable URI identifying this document, used for provenance.
    ///
    /// Form parameters are folded in, because a source that serves every date from one
    /// POST endpoint would otherwise give all its documents the same identity. The
    /// result is for identification and display; it is not necessarily fetchable.
    pub fn identity(&self) -> String {
        if self.form.is_empty() {
            return self.url.clone();
        }
        let form: Vec<(&str, &str)> = self
            .form
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        format!("{}?{}", self.url, crate::cache::encode_form(&form))
    }

    /// How eagerly this document should be re-fetched.
    fn cache_hint(&self) -> CachePolicy {
        if self.volatile {
            CachePolicy::Force
        } else {
            CachePolicy::PreferCache
        }
    }
}

/// A fetched document together with the reference that produced it.
#[derive(Debug, Clone)]
pub struct FetchedDoc {
    pub reference: DocumentRef,
    pub fetched: Fetched,
}

/// Shared services handed to every source.
pub struct Ctx {
    pub fetcher: Fetcher,
    pub db: Db,
}

impl Ctx {
    pub async fn open(config: &Config, policy: CachePolicy) -> Result<Self> {
        Ok(Self {
            fetcher: Fetcher::new(config, policy)?,
            db: Db::open(&config.database_url).await?,
        })
    }

    async fn fetch(&self, source_id: &str, reference: &DocumentRef) -> Result<FetchedDoc> {
        let fetched = if reference.form.is_empty() {
            self.fetcher
                .get_with(source_id, &reference.url, reference.cache_hint())
                .await?
        } else {
            let form: Vec<(&str, &str)> = reference
                .form
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            self.fetcher
                .post_form_with(source_id, &reference.url, &form, reference.cache_hint())
                .await?
        };

        Ok(FetchedDoc {
            reference: reference.clone(),
            fetched,
        })
    }
}

/// One place data comes from. Implementations do only two things: say which documents
/// exist, and turn one document's bytes into normalized records.
#[async_trait]
pub trait Source: Send + Sync {
    /// Stable slug, used as the cache namespace and the external-id scheme. Changing it
    /// orphans previously stored data, so it must not change.
    fn id(&self) -> &'static str;

    fn group(&self) -> DataGroup;

    fn attribution(&self) -> Attribution;

    /// Lists the documents covering `window`, without downloading their contents where
    /// that can be avoided.
    async fn discover(&self, ctx: &Ctx, window: DateWindow) -> Result<Vec<DocumentRef>>;

    /// Turns one document into records. Pure: no network, no database, no clock, so it
    /// can be tested against a committed fixture.
    fn parse(&self, doc: &FetchedDoc) -> Result<Extraction>;
}

/// What an ingest run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestReport {
    pub documents: usize,
    pub entities: usize,
    pub properties: usize,
    pub warnings: usize,
    pub failed: usize,
}

/// Runs a source end to end for a window of dates.
///
/// Documents are processed oldest revision first, so that supersession lands the right
/// way round. A document that fails to parse is reported and skipped; it does not abort
/// the rest of the run.
pub async fn ingest(ctx: &Ctx, source: &dyn Source, window: DateWindow) -> Result<IngestReport> {
    let mut references = source.discover(ctx, window).await?;
    references.retain(|reference| reference.date.is_none_or(|date| window.contains(date)));
    references.sort_by_key(|reference| (reference.date, reference.revision));

    let mut report = IngestReport::default();
    for reference in &references {
        match ingest_document(ctx, source, reference).await {
            Ok(stored) => {
                report.documents += 1;
                report.entities += stored.entities;
                report.properties += stored.properties;
                report.warnings += stored.warnings;
            }
            Err(error) => {
                report.failed += 1;
                tracing::warn!(url = %reference.url, %error, "skipping document");
                ingest::record_issues(
                    &ctx.db,
                    source.id(),
                    None,
                    &[Warning::new(
                        Severity::Error,
                        "document_failed",
                        format!("{}: {error}", reference.label),
                    )],
                )
                .await?;
            }
        }
    }

    Ok(report)
}

#[derive(Default)]
struct DocumentOutcome {
    entities: usize,
    properties: usize,
    warnings: usize,
}

async fn ingest_document(
    ctx: &Ctx,
    source: &dyn Source,
    reference: &DocumentRef,
) -> Result<DocumentOutcome> {
    let doc = ctx.fetch(source.id(), reference).await?;
    let snapshot = ingest::record_snapshot(&ctx.db, source.id(), reference, &doc.fetched).await?;

    let extraction = source.parse(&doc)?;
    let stored = ingest::store(&ctx.db, source.id(), snapshot, &extraction).await?;

    let mut warnings = extraction.warnings.clone();
    warnings.extend(stored.unrecognized_names.iter().map(|name| {
        Warning::new(
            Severity::Info,
            "unrecognized_name",
            format!("first time seeing `{name}`; check it is not a variant spelling"),
        )
    }));
    ingest::record_issues(&ctx.db, source.id(), Some(snapshot.id), &warnings).await?;

    if let Some(date) = reference.date {
        ingest::resolve_supersession(&ctx.db, source.id(), date).await?;
    }

    tracing::info!(
        url = %reference.url,
        entities = stored.entities,
        properties = stored.properties,
        cached = doc.fetched.from_cache,
        "ingested"
    );

    Ok(DocumentOutcome {
        entities: stored.entities,
        properties: stored.properties,
        warnings: warnings.len(),
    })
}

/// Today's date in Athens, which is the calendar these rotas are published against.
pub fn today() -> Date {
    let athens = jiff::tz::TimeZone::get("Europe/Athens").unwrap_or(jiff::tz::TimeZone::UTC);
    jiff::Timestamp::now().to_zoned(athens).date()
}

/// Every source the binary knows about.
pub fn all() -> Vec<Box<dyn Source>> {
    vec![
        Box::new(fsa_pharmacies::FsaPharmacies),
        Box::new(moh_hospitals::MohAtticaHospitals),
    ]
}

/// Looks up a source by its slug.
pub fn by_id(id: &str) -> Result<Box<dyn Source>> {
    all()
        .into_iter()
        .find(|source| source.id() == id)
        .ok_or_else(|| Error::UnknownSource(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i16, month: i8, day: i8) -> Date {
        Date::new(year, month, day).expect("valid date")
    }

    #[test]
    fn a_window_is_inclusive_and_orders_itself() {
        let window = DateWindow::new(date(2026, 8, 20), date(2026, 8, 17));
        assert_eq!(window.from, date(2026, 8, 17));
        assert!(window.contains(date(2026, 8, 17)));
        assert!(window.contains(date(2026, 8, 20)));
        assert!(!window.contains(date(2026, 8, 21)));
        assert_eq!(window.dates().count(), 4);
    }

    #[test]
    fn every_registered_source_has_a_distinct_id() {
        let sources = all();
        let mut ids: Vec<&str> = sources.iter().map(|source| source.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate source id");
    }

    #[test]
    fn an_unknown_source_is_an_error() {
        assert!(by_id("nonexistent").is_err());
    }

    #[test]
    fn volatile_documents_bypass_the_cache() {
        let reference = DocumentRef::new("https://example.org/", "today").volatile(true);
        assert_eq!(reference.cache_hint(), CachePolicy::Force);
        assert_eq!(
            DocumentRef::new("https://example.org/", "archive").cache_hint(),
            CachePolicy::PreferCache
        );
    }
}

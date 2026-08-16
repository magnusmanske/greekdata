//! An unchanged document must not be parsed twice.
//!
//! Documents are identified by the SHA-256 of their bytes. Re-running an ingest — which
//! is the normal daily case, and every backfill — should therefore do no work for
//! anything that has not changed upstream. These tests drive the real pipeline with a
//! stand-in source that counts how often it is asked to parse.

use async_trait::async_trait;
use greekdata::{
    Result,
    cache::CachePolicy,
    config::Config,
    model::{
        DataGroup, EntityDraft, EntityKind, Extraction, Identity, PropertyDraft, PropertyPayload,
        Record,
    },
    sources::{Attribution, Ctx, DateWindow, DocumentRef, FetchedDoc, Source, WhenUnchanged},
};
use jiff::civil::Date;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const SOURCE_ID: &str = "counting-source";

fn date() -> Date {
    Date::new(2026, 8, 17).expect("valid date")
}

/// A source serving one document per day out of the cache, counting its parses.
struct Counting {
    parses: Arc<AtomicUsize>,
    /// Fail this many parses before succeeding, to model a parser that cannot yet cope.
    failures: usize,
}

#[async_trait]
impl Source for Counting {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn group(&self) -> DataGroup {
        DataGroup::Pharmacies
    }

    fn attribution(&self) -> Attribution {
        Attribution {
            publisher: "Test",
            homepage: "https://example.org/",
            terms: "Test fixture.",
        }
    }

    async fn discover(&self, _ctx: &Ctx, window: DateWindow) -> Result<Vec<DocumentRef>> {
        Ok(window
            .dates()
            .map(|day| {
                DocumentRef::new(format!("https://example.org/{day}"), day.to_string()).on(day)
            })
            .collect())
    }

    fn parse(&self, doc: &FetchedDoc) -> Result<Extraction> {
        let attempt = self.parses.fetch_add(1, Ordering::SeqCst);
        if attempt < self.failures {
            return Err(greekdata::Error::parse(
                "test document",
                "cannot read this yet",
            ));
        }

        let day = doc.reference.date.unwrap_or_else(date);
        Ok(Extraction {
            records: vec![Record {
                entity: EntityDraft::new(EntityKind::Pharmacy, "p1", doc.fetched.text().trim())
                    .identified_by(Identity::SourceKey),
                properties: vec![PropertyDraft {
                    on_date: day,
                    starts_at: None,
                    ends_at: None,
                    payload: PropertyPayload::PharmacyOnCall {
                        pharmacist: None,
                        hours_text: None,
                    },
                }],
            }],
            warnings: Vec::new(),
        })
    }
}

/// Puts a document in the cache so the pipeline can run without a network.
async fn publish(config: &Config, url: &str, body: &str) {
    greekdata::cache::Fetcher::new(config, CachePolicy::PreferCache)
        .expect("fetcher")
        .seed(SOURCE_ID, url, body.as_bytes())
        .await
        .expect("seed the cache");
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    parses: Arc<AtomicUsize>,
    failures: usize,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            cache_dir: dir.path().join("cache"),
            database_url: format!("sqlite://{}", dir.path().join("greekdata.db").display()),
            request_delay: std::time::Duration::ZERO,
            ..Config::default()
        };

        Self {
            _dir: dir,
            config,
            parses: Arc::new(AtomicUsize::new(0)),
            failures: 0,
        }
    }

    async fn run(&self, unchanged: WhenUnchanged) -> greekdata::sources::IngestReport {
        let ctx = Ctx::open(&self.config, CachePolicy::CacheOnly)
            .await
            .expect("context");
        let source = Counting {
            parses: Arc::clone(&self.parses),
            failures: self.failures,
        };
        greekdata::sources::ingest(&ctx, &source, DateWindow::new(date(), date()), unchanged)
            .await
            .expect("ingest")
    }

    fn parses(&self) -> usize {
        self.parses.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn an_unchanged_document_is_parsed_once_however_often_it_is_ingested() {
    let fixture = Fixture::new().await;
    publish(
        &fixture.config,
        "https://example.org/2026-08-17",
        "ΦΑΡΜΑΚΕΙΟ Α",
    )
    .await;

    let first = fixture.run(WhenUnchanged::Skip).await;
    assert_eq!((first.documents, first.skipped), (1, 0));

    for _ in 0..3 {
        let again = fixture.run(WhenUnchanged::Skip).await;
        assert_eq!((again.documents, again.skipped), (0, 1));
    }

    assert_eq!(
        fixture.parses(),
        1,
        "the document was parsed more than once"
    );
}

#[tokio::test]
async fn a_changed_document_is_parsed_again() {
    let fixture = Fixture::new().await;
    let url = "https://example.org/2026-08-17";

    publish(&fixture.config, url, "ΦΑΡΜΑΚΕΙΟ Α").await;
    fixture.run(WhenUnchanged::Skip).await;

    // The publisher reissues the day's roster with a different pharmacy.
    publish(&fixture.config, url, "ΦΑΡΜΑΚΕΙΟ Β").await;
    let second = fixture.run(WhenUnchanged::Skip).await;

    assert_eq!((second.documents, second.skipped), (1, 0));
    assert_eq!(fixture.parses(), 2);
}

#[tokio::test]
async fn reparse_overrides_the_skip() {
    let fixture = Fixture::new().await;
    publish(
        &fixture.config,
        "https://example.org/2026-08-17",
        "ΦΑΡΜΑΚΕΙΟ Α",
    )
    .await;

    fixture.run(WhenUnchanged::Skip).await;
    let forced = fixture.run(WhenUnchanged::Reparse).await;

    assert_eq!((forced.documents, forced.skipped), (1, 0));
    assert_eq!(fixture.parses(), 2);
}

#[tokio::test]
async fn skipping_leaves_the_stored_records_exactly_as_they_were() {
    let fixture = Fixture::new().await;
    publish(
        &fixture.config,
        "https://example.org/2026-08-17",
        "ΦΑΡΜΑΚΕΙΟ Α",
    )
    .await;

    fixture.run(WhenUnchanged::Skip).await;
    let db = greekdata::db::Db::open(&fixture.config.database_url)
        .await
        .expect("open");
    let before: Vec<(i64, String)> = sqlx::query_as("SELECT id, on_date FROM property ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("read");

    fixture.run(WhenUnchanged::Skip).await;
    let after: Vec<(i64, String)> = sqlx::query_as("SELECT id, on_date FROM property ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("read");

    // Not merely the same count: the same rows, untouched.
    assert_eq!(before, after);
    assert!(!before.is_empty());
}

#[tokio::test]
async fn a_document_that_failed_to_parse_is_tried_again_next_time() {
    // Only a document that produced records may be treated as done. Marking a failure
    // as handled would quietly lose the day for good.
    let mut fixture = Fixture::new().await;
    fixture.failures = 1;
    publish(
        &fixture.config,
        "https://example.org/2026-08-17",
        "ΦΑΡΜΑΚΕΙΟ Α",
    )
    .await;

    let first = fixture.run(WhenUnchanged::Skip).await;
    assert_eq!((first.documents, first.skipped, first.failed), (0, 0, 1));

    let second = fixture.run(WhenUnchanged::Skip).await;
    assert_eq!((second.documents, second.skipped, second.failed), (1, 0, 0));
    assert_eq!(fixture.parses(), 2);

    // And once it has worked, it is left alone.
    let third = fixture.run(WhenUnchanged::Skip).await;
    assert_eq!((third.documents, third.skipped), (0, 1));
}

//! End-to-end checks on the storage rules that the rest of the project depends on:
//! history is kept, corrections win, re-ingesting changes nothing, and the same entity
//! written two ways stays one entity.

use greekdata::{
    cache::Fetched,
    db::{Db, ingest, query},
    model::{
        EntityDraft, EntityKind, Extraction, Identity, PropertyDraft, PropertyPayload, Record,
        Revision,
    },
    sources::DocumentRef,
};
use jiff::{Timestamp, civil::Date};

const SOURCE: &str = "test-source";

fn date() -> Date {
    Date::new(2026, 8, 17).expect("valid date")
}

fn fetched(body: &str) -> Fetched {
    Fetched {
        url: "https://example.org/rota".into(),
        body: body.as_bytes().to_vec(),
        sha256: format!("sha-of-{body}"),
        fetched_at: Timestamp::now(),
        from_cache: false,
    }
}

fn rota(url: &str, revision: u32) -> DocumentRef {
    DocumentRef::new(url, format!("rota revision {revision}"))
        .on(date())
        .revision(Revision(revision))
}

/// One hospital on duty, named however the document spelled it.
fn extraction(name: &str, clinic: &str) -> Extraction {
    Extraction {
        records: vec![Record {
            entity: EntityDraft::new(EntityKind::Hospital, name, name)
                .identified_by(Identity::Name),
            properties: vec![PropertyDraft {
                on_date: date(),
                starts_at: None,
                ends_at: None,
                payload: PropertyPayload::HospitalOnCall {
                    clinic: clinic.into(),
                    shift: "14:30 – 08:00 επομένης".into(),
                    notes: None,
                },
            }],
        }],
        warnings: Vec::new(),
    }
}

async fn ingest_document(db: &Db, reference: &DocumentRef, body: &str, extraction: &Extraction) {
    let snapshot = ingest::record_snapshot(db, SOURCE, reference, &fetched(body))
        .await
        .expect("record snapshot");
    ingest::store(db, SOURCE, snapshot, extraction)
        .await
        .expect("store");
    ingest::resolve_supersession(db, SOURCE, date())
        .await
        .expect("resolve supersession");
}

async fn active_clinics(db: &Db) -> Vec<String> {
    let mut request = query::OnCallQuery::new(date());
    request.kind = Some(EntityKind::Hospital);

    query::on_call(db, &request)
        .await
        .expect("query")
        .into_iter()
        .filter_map(|row| {
            serde_json::from_str::<PropertyPayload>(&row.payload)
                .ok()
                .and_then(|payload| match payload {
                    PropertyPayload::HospitalOnCall { clinic, .. } => Some(clinic),
                    _ => None,
                })
        })
        .collect()
}

#[tokio::test]
async fn a_corrected_reissue_wins_but_the_original_is_still_there() {
    let db = Db::open_in_memory().await.expect("open");

    ingest_document(
        &db,
        &rota("https://example.org/original", 0),
        "original",
        &extraction("Γ.Ν.Α. «ΣΩΤΗΡΙΑ»", "Παθολογική"),
    )
    .await;
    ingest_document(
        &db,
        &rota("https://example.org/corrected", 1),
        "corrected",
        &extraction("Γ.Ν.Α. «ΣΩΤΗΡΙΑ»", "Καρδιολογική"),
    )
    .await;

    // Only the correction is served.
    assert_eq!(active_clinics(&db).await, ["Καρδιολογική"]);

    // But the original is still stored, just marked superseded.
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM property")
        .fetch_one(db.pool())
        .await
        .expect("count");
    let superseded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM property WHERE superseded = 1")
        .fetch_one(db.pool())
        .await
        .expect("count superseded");
    assert_eq!((total, superseded), (2, 1));
}

#[tokio::test]
async fn ingesting_an_older_revision_afterwards_does_not_undo_the_correction() {
    let db = Db::open_in_memory().await.expect("open");

    ingest_document(
        &db,
        &rota("https://example.org/corrected", 1),
        "corrected",
        &extraction("Γ.Ν.Α. «ΣΩΤΗΡΙΑ»", "Καρδιολογική"),
    )
    .await;
    // A backfill run reaches the original later than the reissue.
    ingest_document(
        &db,
        &rota("https://example.org/original", 0),
        "original",
        &extraction("Γ.Ν.Α. «ΣΩΤΗΡΙΑ»", "Παθολογική"),
    )
    .await;

    assert_eq!(active_clinics(&db).await, ["Καρδιολογική"]);
}

#[tokio::test]
async fn re_ingesting_the_same_document_changes_nothing() {
    let db = Db::open_in_memory().await.expect("open");
    let reference = rota("https://example.org/original", 0);
    let extraction = extraction("Γ.Ν.Α. «ΣΩΤΗΡΙΑ»", "Παθολογική");

    for _ in 0..3 {
        ingest_document(&db, &reference, "original", &extraction).await;
    }

    let properties: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM property")
        .fetch_one(db.pool())
        .await
        .expect("count");
    let snapshots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM snapshot")
        .fetch_one(db.pool())
        .await
        .expect("count");

    assert_eq!((properties, snapshots), (1, 1));
    assert_eq!(active_clinics(&db).await, ["Παθολογική"]);
}

#[tokio::test]
async fn the_same_hospital_spelled_differently_stays_one_entity() {
    let db = Db::open_in_memory().await.expect("open");

    // Two documents, two spellings of one hospital: punctuation and spacing differ.
    ingest_document(
        &db,
        &rota("https://example.org/monday", 0),
        "monday",
        &extraction("Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»", "Παθολογική"),
    )
    .await;
    ingest_document(
        &db,
        &rota("https://example.org/tuesday", 0),
        "tuesday",
        &extraction("Γ.Ν.Α «Γ.ΓΕΝΝΗΜΑΤΑΣ»", "Παθολογική"),
    )
    .await;

    let hospitals = query::entities(&db, Some(EntityKind::Hospital), None, 100)
        .await
        .expect("list hospitals");
    assert_eq!(hospitals.len(), 1, "got {hospitals:?}");
}

#[tokio::test]
async fn two_pharmacies_sharing_a_pharmacists_name_stay_separate() {
    let db = Db::open_in_memory().await.expect("open");
    let reference = rota("https://example.org/pharmacies", 0);

    // The association assigns its own keys, so identical names are still two shops.
    let same_name = Extraction {
        records: ["1001", "1002"]
            .into_iter()
            .map(|key| Record {
                entity: EntityDraft::new(EntityKind::Pharmacy, key, "ΠΑΠΑΔΟΠΟΥΛΟΥ ΜΑΡΙΑ")
                    .identified_by(Identity::SourceKey),
                properties: vec![PropertyDraft {
                    on_date: date(),
                    starts_at: None,
                    ends_at: None,
                    payload: PropertyPayload::PharmacyOnCall {
                        pharmacist: Some("ΠΑΠΑΔΟΠΟΥΛΟΥ ΜΑΡΙΑ".into()),
                        hours_text: None,
                    },
                }],
            })
            .collect(),
        warnings: Vec::new(),
    };
    ingest_document(&db, &reference, "pharmacies", &same_name).await;

    let pharmacies = query::entities(&db, Some(EntityKind::Pharmacy), None, 100)
        .await
        .expect("list pharmacies");
    assert_eq!(pharmacies.len(), 2);
}

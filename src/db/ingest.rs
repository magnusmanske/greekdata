//! The write side of the database: recording provenance, resolving entities, and
//! storing what a document said.
//!
//! Nothing here ever deletes history. When a source republishes a corrected rota, the
//! earlier rows stay exactly where they were and are simply marked superseded.

use crate::{
    Result,
    cache::Fetched,
    db::Db,
    greek::matching_key,
    model::{EntityDraft, Extraction, Identity, PropertyDraft, Warning},
    sources::DocumentRef,
};
use jiff::{Timestamp, civil::Date};
use sqlx::{Sqlite, Transaction};

/// A stored source document, and whether we had already seen this exact content.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotRef {
    pub id: i64,
    pub is_new: bool,
}

/// What one document contributed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stored {
    pub entities: usize,
    pub properties: usize,
    /// Names of entities created for the first time, where the name is what identifies
    /// them. These are worth a human glance: a genuinely new hospital is rarer than a
    /// spelling we have not seen before, and the latter silently fragments an entity.
    pub unrecognized_names: Vec<String>,
}

/// Records a fetched document, reusing the existing row if the same bytes were already
/// stored under the same URL.
pub async fn record_snapshot(
    db: &Db,
    source_id: &str,
    reference: &DocumentRef,
    fetched: &Fetched,
) -> Result<SnapshotRef> {
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM snapshot WHERE source_id = ?1 AND url = ?2 AND sha256 = ?3",
    )
    .bind(source_id)
    .bind(reference.identity())
    .bind(&fetched.sha256)
    .fetch_optional(db.pool())
    .await?;

    if let Some(id) = existing {
        return Ok(SnapshotRef { id, is_new: false });
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO snapshot (source_id, url, sha256, fetched_at, published_date, revision, label)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         RETURNING id",
    )
    .bind(source_id)
    .bind(reference.identity())
    .bind(&fetched.sha256)
    .bind(fetched.fetched_at.to_string())
    .bind(reference.date.map(|date| date.to_string()))
    .bind(i64::from(reference.revision.0))
    .bind(&reference.label)
    .fetch_one(db.pool())
    .await?;

    Ok(SnapshotRef { id, is_new: true })
}

/// Stores everything a document said, replacing whatever that same document contributed
/// before so a re-ingest is idempotent.
pub async fn store(
    db: &Db,
    source_id: &str,
    snapshot: SnapshotRef,
    extraction: &Extraction,
) -> Result<Stored> {
    let mut tx = db.pool().begin().await?;

    sqlx::query("DELETE FROM property WHERE snapshot_id = ?1")
        .bind(snapshot.id)
        .execute(&mut *tx)
        .await?;

    let mut stored = Stored::default();
    for record in &extraction.records {
        let (entity_id, created) = resolve_entity(&mut tx, source_id, &record.entity).await?;
        stored.entities += 1;
        if created && record.entity.identity == Identity::Name {
            stored.unrecognized_names.push(record.entity.name.clone());
        }
        for property in &record.properties {
            insert_property(&mut tx, entity_id, snapshot.id, property).await?;
            stored.properties += 1;
        }
    }

    tx.commit().await?;
    Ok(stored)
}

/// Finds the entity this draft describes, or creates it. Reports whether it was created.
///
/// Matching goes from most to least reliable: the source's own key, then the folded
/// name, then a previously recorded alias.
async fn resolve_entity(
    tx: &mut Transaction<'_, Sqlite>,
    source_id: &str,
    draft: &EntityDraft,
) -> Result<(i64, bool)> {
    let key = matching_key(&draft.name);

    let by_external: Option<i64> = sqlx::query_scalar(
        "SELECT entity_id FROM entity_external_id WHERE scheme = ?1 AND value = ?2",
    )
    .bind(source_id)
    .bind(&draft.local_id)
    .fetch_optional(&mut **tx)
    .await?;

    // Name matching is only safe when the name is what identifies the entity.
    let name_matches_allowed = draft.identity == Identity::Name;

    let by_name: Option<i64> = match by_external {
        Some(id) => Some(id),
        None if !name_matches_allowed => None,
        None => {
            sqlx::query_scalar("SELECT id FROM entity WHERE kind = ?1 AND name_folded = ?2")
                .bind(draft.kind.as_str())
                .bind(&key)
                .fetch_optional(&mut **tx)
                .await?
        }
    };

    let entity_id = match by_name {
        Some(id) => Some(id),
        None if !name_matches_allowed => None,
        None => {
            sqlx::query_scalar(
                "SELECT a.entity_id FROM entity_alias a
                 JOIN entity e ON e.id = a.entity_id
                 WHERE a.alias_folded = ?1 AND e.kind = ?2",
            )
            .bind(&key)
            .bind(draft.kind.as_str())
            .fetch_optional(&mut **tx)
            .await?
        }
    };

    let created = entity_id.is_none();
    let now = Timestamp::now().to_string();
    let entity_id = match entity_id {
        Some(id) => {
            // Fill in anything we did not know before, but never overwrite a known
            // value with a null just because this document omitted it.
            sqlx::query(
                "UPDATE entity SET
                     name = ?1, name_folded = ?2,
                     address = COALESCE(?3, address),
                     municipality = COALESCE(?4, municipality),
                     lat = COALESCE(?5, lat), lon = COALESCE(?6, lon),
                     url = COALESCE(?7, url), phone = COALESCE(?8, phone),
                     updated_at = ?9
                 WHERE id = ?10",
            )
            .bind(&draft.name)
            .bind(&key)
            .bind(&draft.address)
            .bind(&draft.municipality)
            .bind(draft.location.map(|point| point.lat()))
            .bind(draft.location.map(|point| point.lon()))
            .bind(&draft.url)
            .bind(&draft.phone)
            .bind(&now)
            .bind(id)
            .execute(&mut **tx)
            .await?;
            id
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO entity
                     (kind, name, name_folded, address, municipality, lat, lon, url, phone,
                      created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                 RETURNING id",
            )
            .bind(draft.kind.as_str())
            .bind(&draft.name)
            .bind(&key)
            .bind(&draft.address)
            .bind(&draft.municipality)
            .bind(draft.location.map(|point| point.lat()))
            .bind(draft.location.map(|point| point.lon()))
            .bind(&draft.url)
            .bind(&draft.phone)
            .bind(&now)
            .fetch_one(&mut **tx)
            .await?
        }
    };

    // Remember this spelling, and any others the source offered, for next time.
    for alias in std::iter::once(&draft.name).chain(draft.aliases.iter()) {
        sqlx::query(
            "INSERT INTO entity_alias (entity_id, alias, alias_folded) VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING",
        )
        .bind(entity_id)
        .bind(alias)
        .bind(matching_key(alias))
        .execute(&mut **tx)
        .await?;
    }

    let source_key = std::iter::once((source_id, draft.local_id.as_str()));
    let declared = draft
        .external_ids
        .iter()
        .map(|id| (id.scheme.as_str(), id.value.as_str()));
    for (scheme, value) in source_key.chain(declared) {
        sqlx::query(
            "INSERT INTO entity_external_id (entity_id, scheme, value) VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING",
        )
        .bind(entity_id)
        .bind(scheme)
        .bind(value)
        .execute(&mut **tx)
        .await?;
    }

    Ok((entity_id, created))
}

async fn insert_property(
    tx: &mut Transaction<'_, Sqlite>,
    entity_id: i64,
    snapshot_id: i64,
    property: &PropertyDraft,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO property (entity_id, snapshot_id, kind, on_date, starts_at, ends_at, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(entity_id)
    .bind(snapshot_id)
    .bind(property.payload.kind())
    .bind(property.on_date.to_string())
    .bind(property.starts_at.map(|at| at.to_string()))
    .bind(property.ends_at.map(|at| at.to_string()))
    .bind(serde_json::to_string(&property.payload)?)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Marks every row for a source and date as superseded except those from its latest
/// revision. Idempotent, and correct even if an older revision is ingested afterwards.
pub async fn resolve_supersession(db: &Db, source_id: &str, date: Date) -> Result<()> {
    let date = date.to_string();
    let mut tx = db.pool().begin().await?;

    sqlx::query(
        "UPDATE property SET superseded = 1
         WHERE snapshot_id IN
             (SELECT id FROM snapshot WHERE source_id = ?1 AND published_date = ?2)",
    )
    .bind(source_id)
    .bind(&date)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE property SET superseded = 0
         WHERE snapshot_id =
             (SELECT id FROM snapshot WHERE source_id = ?1 AND published_date = ?2
              ORDER BY revision DESC, id DESC LIMIT 1)",
    )
    .bind(source_id)
    .bind(&date)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Files parse problems against the document they came from, for `greekdata report`.
pub async fn record_issues(
    db: &Db,
    source_id: &str,
    snapshot_id: Option<i64>,
    warnings: &[Warning],
) -> Result<()> {
    if warnings.is_empty() {
        return Ok(());
    }

    let mut tx = db.pool().begin().await?;
    if let Some(id) = snapshot_id {
        sqlx::query("DELETE FROM ingest_issue WHERE snapshot_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    let now = Timestamp::now().to_string();
    for warning in warnings {
        sqlx::query(
            "INSERT INTO ingest_issue (snapshot_id, source_id, severity, code, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(snapshot_id)
        .bind(source_id)
        .bind(warning.severity.as_str())
        .bind(warning.code)
        .bind(&warning.detail)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

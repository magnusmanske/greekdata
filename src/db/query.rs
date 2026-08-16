//! The read side of the database, shared by the API server and the CLI.
//!
//! Queries return only current rows by default: a corrected rota supersedes the one it
//! replaced, but both stay in the database and the superseded one can still be asked for.

use crate::{Result, db::Db, model::EntityKind};
use jiff::civil::Date;
use serde::Serialize;
use sqlx::FromRow;

/// Hard ceilings, so a hostile or careless query cannot ask for the whole database.
pub const MAX_LIMIT: i64 = 500;
pub const MAX_RADIUS_KM: f64 = 50.0;

const EARTH_RADIUS_KM: f64 = 6371.0;

/// An entity together with one thing that is true of it on a given day.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct OnCallRow {
    pub entity_id: i64,
    pub kind: String,
    pub name: String,
    pub address: Option<String>,
    pub municipality: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub phone: Option<String>,
    /// Where the coordinates came from, when not from the source itself.
    pub location_source: Option<String>,
    pub on_date: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    /// Group-specific detail, stored as JSON text and served as nested JSON.
    #[serde(serialize_with = "serialize_json_text")]
    pub payload: String,
    pub source_id: String,
    pub source_url: String,
    /// Kilometres from the requested point, when one was given.
    #[sqlx(default)]
    pub distance_km: Option<f64>,
}

/// A stored entity, without any of its dated properties.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct EntityRow {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub address: Option<String>,
    pub municipality: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub phone: Option<String>,
    pub url: Option<String>,
    pub location_source: Option<String>,
}

/// A parse problem recorded during ingest.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct IssueRow {
    pub source_id: String,
    pub severity: String,
    pub code: String,
    pub detail: String,
    pub created_at: String,
}

/// A point and a radius to search within.
#[derive(Debug, Clone, Copy)]
pub struct Near {
    pub lat: f64,
    pub lon: f64,
    pub radius_km: f64,
}

/// What to look up for a given day.
#[derive(Debug, Clone)]
pub struct OnCallQuery {
    pub date: Date,
    pub kind: Option<EntityKind>,
    pub near: Option<Near>,
    pub limit: i64,
}

impl OnCallQuery {
    pub fn new(date: Date) -> Self {
        Self {
            date,
            kind: None,
            near: None,
            limit: 100,
        }
    }
}

/// Everything on duty on a day, optionally narrowed by kind and proximity.
///
/// Proximity is filtered by bounding box in SQL — which the `entity_geo` index can
/// serve — and then refined to a true great-circle distance in Rust.
pub async fn on_call(db: &Db, query: &OnCallQuery) -> Result<Vec<OnCallRow>> {
    let limit = query.limit.clamp(1, MAX_LIMIT);
    let box_limit = query.near.map_or(limit, |_| MAX_LIMIT);
    let bounds = query.near.map(bounding_box);

    let mut rows: Vec<OnCallRow> = sqlx::query_as(
        "SELECT e.id AS entity_id, e.kind, e.name, e.address, e.municipality, e.lat, e.lon,
                e.phone, e.location_source, p.on_date, p.starts_at, p.ends_at, p.payload,
                s.source_id, s.url AS source_url
         FROM property p
         JOIN entity e   ON e.id = p.entity_id
         JOIN snapshot s ON s.id = p.snapshot_id
         WHERE p.on_date = ?1
           AND p.superseded = 0
           AND (?2 IS NULL OR e.kind = ?2)
           AND (?3 IS NULL OR (e.lat BETWEEN ?3 AND ?4 AND e.lon BETWEEN ?5 AND ?6))
         ORDER BY e.name, p.starts_at
         LIMIT ?7",
    )
    .bind(query.date.to_string())
    .bind(query.kind.map(|kind| kind.as_str()))
    .bind(bounds.map(|b| b.min_lat))
    .bind(bounds.map(|b| b.max_lat))
    .bind(bounds.map(|b| b.min_lon))
    .bind(bounds.map(|b| b.max_lon))
    .bind(box_limit)
    .fetch_all(db.pool())
    .await?;

    if let Some(near) = query.near {
        for row in &mut rows {
            row.distance_km = match (row.lat, row.lon) {
                (Some(lat), Some(lon)) => Some(distance_km(near.lat, near.lon, lat, lon)),
                _ => None,
            };
        }
        rows.retain(|row| row.distance_km.is_some_and(|km| km <= near.radius_km));
        rows.sort_by(|a, b| {
            a.distance_km
                .unwrap_or(f64::MAX)
                .total_cmp(&b.distance_km.unwrap_or(f64::MAX))
        });
        rows.truncate(limit as usize);
    }

    Ok(rows)
}

/// Lists entities, optionally filtered by kind and by a substring of the name.
pub async fn entities(
    db: &Db,
    kind: Option<EntityKind>,
    search: Option<&str>,
    limit: i64,
) -> Result<Vec<EntityRow>> {
    let pattern = search.map(|text| format!("%{}%", crate::greek::matching_key(text)));

    Ok(sqlx::query_as(
        "SELECT id, kind, name, address, municipality, lat, lon, phone, url, location_source
         FROM entity
         WHERE (?1 IS NULL OR kind = ?1)
           AND (?2 IS NULL OR name_folded LIKE ?2)
         ORDER BY name
         LIMIT ?3",
    )
    .bind(kind.map(|kind| kind.as_str()))
    .bind(pattern)
    .bind(limit.clamp(1, MAX_LIMIT))
    .fetch_all(db.pool())
    .await?)
}

pub async fn entity(db: &Db, id: i64) -> Result<Option<EntityRow>> {
    Ok(sqlx::query_as(
        "SELECT id, kind, name, address, municipality, lat, lon, phone, url, location_source
         FROM entity WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?)
}

/// Parse problems, worst first, for `greekdata report`.
pub async fn issues(db: &Db, source_id: Option<&str>, limit: i64) -> Result<Vec<IssueRow>> {
    Ok(sqlx::query_as(
        "SELECT source_id, severity, code, detail, created_at
         FROM ingest_issue
         WHERE (?1 IS NULL OR source_id = ?1)
         ORDER BY CASE severity WHEN 'error' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END,
                  created_at DESC
         LIMIT ?2",
    )
    .bind(source_id)
    .bind(limit.clamp(1, MAX_LIMIT))
    .fetch_all(db.pool())
    .await?)
}

/// Serves a JSON column as nested JSON rather than an escaped string. A payload we
/// cannot re-parse is passed through verbatim instead of failing the whole response.
fn serialize_json_text<S: serde::Serializer>(
    text: &str,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => value.serialize(serializer),
        Err(_) => text.serialize(serializer),
    }
}

#[derive(Debug, Clone, Copy)]
struct BoundingBox {
    min_lat: f64,
    max_lat: f64,
    min_lon: f64,
    max_lon: f64,
}

/// A generous box around a point, used only to let the index discard distant rows.
fn bounding_box(near: Near) -> BoundingBox {
    let radius = near.radius_km.clamp(0.0, MAX_RADIUS_KM);
    let lat_span = radius / 111.0;
    // Longitude degrees shrink towards the poles; guard the division near them.
    let lon_span = radius / (111.0 * near.lat.to_radians().cos().abs().max(0.01));

    BoundingBox {
        min_lat: near.lat - lat_span,
        max_lat: near.lat + lat_span,
        min_lon: near.lon - lon_span,
        max_lon: near.lon + lon_span,
    }
}

/// Great-circle distance between two points, in kilometres.
fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let delta_phi = (lat2 - lat1).to_radians();
    let delta_lambda = (lon2 - lon1).to_radians();

    let a = (delta_phi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (delta_lambda / 2.0).sin().powi(2);

    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_matches_a_known_separation() {
        // Syntagma Square to Piraeus port is about 8 km.
        let km = distance_km(37.9755, 23.7348, 37.9420, 23.6465);
        assert!((7.0..9.5).contains(&km), "got {km} km");
        assert_eq!(distance_km(37.9755, 23.7348, 37.9755, 23.7348), 0.0);
    }

    #[test]
    fn the_bounding_box_contains_the_radius() {
        let near = Near {
            lat: 37.98,
            lon: 23.73,
            radius_km: 5.0,
        };
        let bounds = bounding_box(near);

        // A point due north at exactly the radius must fall inside the box.
        assert!(bounds.max_lat > near.lat + 4.9 / 111.0);
        assert!(bounds.min_lon < near.lon);
        assert!(bounds.max_lon > near.lon);
    }

    #[test]
    fn the_bounding_box_survives_the_poles() {
        let bounds = bounding_box(Near {
            lat: 90.0,
            lon: 0.0,
            radius_km: 10.0,
        });
        assert!(bounds.max_lon.is_finite() && bounds.min_lon.is_finite());
    }
}

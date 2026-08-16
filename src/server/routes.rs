//! Request handlers. Each one validates its inputs into a typed query, runs it, and
//! answers with the data plus enough provenance to trace it back to the publisher.

use super::AppState;
use crate::{
    db::query::{self, EntityRow, MAX_LIMIT, MAX_RADIUS_KM, Near, OnCallQuery, OnCallRow},
    model::EntityKind,
    sources,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Anything a caller can get wrong, reported without leaking internals.
pub enum ApiError {
    BadRequest(String),
    NotFound,
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            ),
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

impl From<crate::Error> for ApiError {
    fn from(error: crate::Error) -> Self {
        // The detail goes to the log, not to the caller.
        tracing::error!(%error, "request failed");
        Self::Internal
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

type ApiResult<T> = std::result::Result<T, ApiError>;

pub async fn healthz() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
pub struct OnCallParams {
    /// Day to look up, `YYYY-MM-DD`. Defaults to today in Athens.
    date: Option<String>,
    /// Entity kind, e.g. `pharmacy` or `hospital`.
    kind: Option<String>,
    /// `lat,lon` to search around.
    near: Option<String>,
    /// Search radius in kilometres.
    radius: Option<f64>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct OnCallResponse {
    date: String,
    count: usize,
    results: Vec<OnCallRow>,
    attribution: Vec<AttributionBody>,
}

pub async fn on_call(
    State(state): State<AppState>,
    Query(params): Query<OnCallParams>,
) -> ApiResult<Json<OnCallResponse>> {
    let date = match params.date.as_deref() {
        Some(text) => parse_date(text)?,
        None => sources::today(),
    };

    let mut request = OnCallQuery::new(date);
    request.kind = params.kind.as_deref().map(parse_kind).transpose()?;
    request.near = params
        .near
        .as_deref()
        .map(parse_point)
        .transpose()?
        .map(|(lat, lon)| Near {
            lat,
            lon,
            radius_km: params.radius.unwrap_or(3.0).clamp(0.1, MAX_RADIUS_KM),
        });
    request.limit = params.limit.unwrap_or(100).clamp(1, MAX_LIMIT);

    let results = query::on_call(&state.db, &request).await?;

    Ok(Json(OnCallResponse {
        date: date.to_string(),
        count: results.len(),
        results,
        attribution: attributions(),
    }))
}

#[derive(Deserialize)]
pub struct EntitiesParams {
    kind: Option<String>,
    /// Substring of the name, matched against the accent-folded form.
    q: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct EntitiesResponse {
    count: usize,
    results: Vec<EntityRow>,
}

pub async fn entities(
    State(state): State<AppState>,
    Query(params): Query<EntitiesParams>,
) -> ApiResult<Json<EntitiesResponse>> {
    let kind = params.kind.as_deref().map(parse_kind).transpose()?;
    let limit = params.limit.unwrap_or(100).clamp(1, MAX_LIMIT);
    let results = query::entities(&state.db, kind, params.q.as_deref(), limit).await?;

    Ok(Json(EntitiesResponse {
        count: results.len(),
        results,
    }))
}

pub async fn entity(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<EntityRow>> {
    query::entity(&state.db, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(Serialize)]
pub struct AttributionBody {
    id: &'static str,
    group: String,
    publisher: &'static str,
    homepage: &'static str,
    terms: &'static str,
}

pub async fn sources() -> Json<Vec<AttributionBody>> {
    Json(attributions())
}

fn attributions() -> Vec<AttributionBody> {
    sources::all()
        .iter()
        .map(|source| {
            let attribution = source.attribution();
            AttributionBody {
                id: source.id(),
                group: source.group().to_string(),
                publisher: attribution.publisher,
                homepage: attribution.homepage,
                terms: attribution.terms,
            }
        })
        .collect()
}

fn parse_date(text: &str) -> ApiResult<Date> {
    Date::from_str(text.trim())
        .map_err(|_| ApiError::BadRequest(format!("`{text}` is not a YYYY-MM-DD date")))
}

fn parse_kind(text: &str) -> ApiResult<EntityKind> {
    EntityKind::from_str(text.trim()).map_err(|_| {
        let known: Vec<&str> = EntityKind::ALL.iter().map(|kind| kind.as_str()).collect();
        ApiError::BadRequest(format!(
            "`{text}` is not a known kind; expected one of {}",
            known.join(", ")
        ))
    })
}

fn parse_point(text: &str) -> ApiResult<(f64, f64)> {
    let malformed = || ApiError::BadRequest(format!("`{text}` is not a `lat,lon` pair"));
    let (lat, lon) = text.split_once(',').ok_or_else(malformed)?;
    let lat: f64 = lat.trim().parse().map_err(|_| malformed())?;
    let lon: f64 = lon.trim().parse().map_err(|_| malformed())?;

    // Reject impossible points rather than quietly searching nowhere.
    crate::model::Location::new(lat, lon).map_err(|_| malformed())?;
    Ok((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_are_validated_not_trusted() {
        assert!(parse_point("37.98,23.72").is_ok());
        assert!(parse_point(" 37.98 , 23.72 ").is_ok());
        assert!(parse_point("37.98").is_err());
        assert!(parse_point("north,east").is_err());
        assert!(parse_point("999,999").is_err());
    }

    #[test]
    fn unknown_kinds_are_rejected_with_a_useful_message() {
        assert!(parse_kind("pharmacy").is_ok());
        assert!(parse_kind(" hospital ").is_ok());
        assert!(parse_kind("submarine").is_err());
    }

    #[test]
    fn dates_must_be_iso() {
        assert!(parse_date("2026-08-17").is_ok());
        assert!(parse_date("17/08/2026").is_err());
    }
}

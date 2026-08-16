//! The read-only HTTP API.
//!
//! Every handler is a lookup: nothing here writes to the database. Inputs are bounded
//! (limits and radii are clamped, dates must parse), all SQL is parameterized, and CORS
//! is off unless origins are configured explicitly.

mod routes;

use crate::{Result, config::Config, db::Db};
use axum::{Router, http::HeaderValue, routing::get};
use std::time::Duration;
use tower_http::{
    cors::CorsLayer, limit::RequestBodyLimitLayer, timeout::TimeoutLayer, trace::TraceLayer,
};

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
}

/// Builds the router. Separate from [`serve`] so tests can drive it without a socket.
pub fn router(config: &Config, db: Db) -> Router {
    let api = Router::new()
        .route("/on-call", get(routes::on_call))
        .route("/entities", get(routes::entities))
        .route("/entities/{id}", get(routes::entity))
        .route("/sources", get(routes::sources));

    Router::new()
        .route("/healthz", get(routes::healthz))
        .nest("/api/v1", api)
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(15),
        ))
        // Nothing here accepts a body; keep the ceiling small.
        .layer(RequestBodyLimitLayer::new(4 * 1024))
        .layer(cors(config))
        .with_state(AppState { db })
}

pub async fn serve(config: Config, db: Db) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|source| crate::Error::io(config.bind.to_string(), source))?;

    tracing::info!(address = %config.bind, "serving");
    axum::serve(listener, router(&config, db))
        .await
        .map_err(|source| crate::Error::io("http server", source))
}

/// Cross-origin access is opt-in: an unconfigured deployment allows no browser origins.
fn cors(config: &Config) -> CorsLayer {
    let origins: Vec<HeaderValue> = config
        .cors_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();

    if origins.is_empty() {
        CorsLayer::new()
    } else {
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([axum::http::Method::GET])
    }
}

//! The read-only HTTP API, and the small site that documents it.
//!
//! Every handler is a lookup: nothing here writes to the database. Inputs are bounded
//! (limits and radii are clamped, dates must parse), all SQL is parameterized, and CORS
//! is off unless origins are configured explicitly.

mod index;
mod routes;

use crate::{Result, config::Config, db::Db};
use axum::{Router, http::HeaderValue, routing::MethodRouter, routing::get};
use std::{sync::Arc, time::Duration};
use tower_http::{
    cors::CorsLayer, limit::RequestBodyLimitLayer, set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer, trace::TraceLayer,
};

/// A query parameter an endpoint accepts.
pub struct Parameter {
    pub name: &'static str,
    pub description: &'static str,
}

/// What an endpoint is, for both routing and the front page.
pub struct Endpoint {
    pub path: &'static str,
    pub summary: &'static str,
    pub parameters: &'static [Parameter],
    /// Consumed when the router is built; the rest becomes [`EndpointDoc`].
    route: MethodRouter<AppState>,
}

impl Endpoint {
    fn doc(&self) -> EndpointDoc {
        EndpointDoc {
            path: self.path.to_string(),
            summary: self.summary,
            parameters: self.parameters,
        }
    }
}

/// The documentation half of an [`Endpoint`], kept in state for the front page.
#[derive(Clone)]
pub struct EndpointDoc {
    pub path: String,
    pub summary: &'static str,
    pub parameters: &'static [Parameter],
}

/// Every endpoint's documentation, without building a router. Production code gets the
/// same list from [`router`], which produces it alongside the routes.
#[cfg(test)]
fn endpoint_docs() -> Vec<EndpointDoc> {
    endpoints().iter().map(Endpoint::doc).collect()
}

/// Every endpoint the server offers.
///
/// This is the single source of truth: the router is built from it and the front page is
/// rendered from it, so a new endpoint cannot be added without also being documented.
fn endpoints() -> Vec<Endpoint> {
    const NEAR: Parameter = Parameter {
        name: "near",
        description: "`lat,lon` to search around, e.g. `37.9755,23.7348`",
    };
    const RADIUS: Parameter = Parameter {
        name: "radius",
        description: "search radius in km when `near` is given (default 3, max 50)",
    };
    const LIMIT: Parameter = Parameter {
        name: "limit",
        description: "maximum results (default 100, max 500)",
    };

    vec![
        Endpoint {
            path: "/api/v1/on-call",
            summary: "Pharmacies, hospitals and health centres on duty on a given day, \
                      newest published version of the rota, optionally nearest first.",
            parameters: &[
                Parameter {
                    name: "date",
                    description: "day to look up as `YYYY-MM-DD` (default: today in Athens)",
                },
                Parameter {
                    name: "kind",
                    description: "`pharmacy`, `hospital` or `health_centre`",
                },
                NEAR,
                RADIUS,
                LIMIT,
            ],
            route: get(routes::on_call),
        },
        Endpoint {
            path: "/api/v1/entities",
            summary: "Search stored places by name. Matching ignores accents, case, \
                      punctuation and spacing.",
            parameters: &[
                Parameter {
                    name: "kind",
                    description: "restrict to one kind of place",
                },
                Parameter {
                    name: "q",
                    description: "part of a name, in Greek, with or without accents",
                },
                LIMIT,
            ],
            route: get(routes::entities),
        },
        Endpoint {
            path: "/api/v1/entities/{id}",
            summary: "One place by its numeric id.",
            parameters: &[],
            route: get(routes::entity),
        },
        Endpoint {
            path: "/api/v1/sources",
            summary: "The data sources in use, with their publishers and terms.",
            parameters: &[],
            route: get(routes::sources),
        },
        Endpoint {
            path: "/healthz",
            summary: "Liveness check. Returns `ok`.",
            parameters: &[],
            route: get(routes::healthz),
        },
    ]
}

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub endpoints: Arc<Vec<EndpointDoc>>,
}

/// Builds the router. Separate from [`serve`] so tests can drive it without a socket.
pub fn router(config: &Config, db: Db) -> Router {
    let mut routes = Router::new();
    let mut documented = Vec::new();

    for endpoint in endpoints() {
        documented.push(endpoint.doc());
        routes = routes.route(endpoint.path, endpoint.route);
    }

    routes
        .route("/", get(index::index))
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(15),
        ))
        // Nothing here accepts a body; keep the ceiling small.
        .layer(RequestBodyLimitLayer::new(4 * 1024))
        .layer(cors(config))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        // The page is self-contained, so nothing may be loaded from anywhere else.
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'",
            ),
        ))
        .with_state(AppState {
            db,
            endpoints: Arc::new(documented),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_endpoint_is_documented() {
        for endpoint in endpoints() {
            assert!(
                endpoint.path.starts_with('/'),
                "{} is not a path",
                endpoint.path
            );
            assert!(
                !endpoint.summary.trim().is_empty(),
                "{} has no summary",
                endpoint.path
            );
            for parameter in endpoint.parameters {
                assert!(
                    !parameter.description.trim().is_empty(),
                    "{} parameter `{}` has no description",
                    endpoint.path,
                    parameter.name
                );
            }
        }
    }

    #[test]
    fn endpoint_paths_are_unique() {
        let mut paths: Vec<&str> = endpoints().iter().map(|e| e.path).collect();
        let count = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), count, "duplicate endpoint path");
    }
}

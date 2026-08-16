//! The read-only HTTP API, and the small site that documents it.
//!
//! Every handler is a lookup: nothing here writes to the database. Inputs are bounded
//! (limits and radii are clamped, dates must parse), all SQL is parameterized, and CORS
//! is off unless origins are configured explicitly.

mod assets;
mod index;
mod map;
mod routes;

use crate::{Result, config::Config, db::Db};
use axum::{Router, http::HeaderValue, routing::MethodRouter, routing::get};
use std::{sync::Arc, time::Duration};
use tower_http::{
    cors::CorsLayer, limit::RequestBodyLimitLayer, set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer, trace::TraceLayer,
};

/// The page is self-contained: scripts, styles and marker images all come from here, so
/// nothing may be loaded from anywhere else. The one exception is map tiles.
///
/// Both the bare tile host and its sub-domains are listed. A CSP wildcard requires at
/// least one label, so `*.tile.openstreetmap.org` permits `a.tile.openstreetmap.org` but
/// not `tile.openstreetmap.org` — which is the host the map actually uses, and listing
/// only the wildcard silently blocked every tile.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
     script-src 'self'; \
     style-src 'self'; \
     img-src 'self' data: https://tile.openstreetmap.org https://*.tile.openstreetmap.org; \
     connect-src 'self'; \
     base-uri 'none'; \
     form-action 'none'; \
     frame-ancestors 'none'";

/// Whether an entry is something a person visits or something a program calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Page,
    Api,
}

/// A query parameter an endpoint accepts.
pub struct Parameter {
    pub name: &'static str,
    pub description: &'static str,
}

/// What an endpoint is, for both routing and the front page.
pub struct Endpoint {
    pub surface: Surface,
    pub path: &'static str,
    pub summary: &'static str,
    pub parameters: &'static [Parameter],
    /// Consumed when the router is built; the rest becomes [`EndpointDoc`].
    route: MethodRouter<AppState>,
}

impl Endpoint {
    fn doc(&self) -> EndpointDoc {
        EndpointDoc {
            surface: self.surface,
            path: self.path.to_string(),
            summary: self.summary,
            parameters: self.parameters,
        }
    }
}

/// The documentation half of an [`Endpoint`], kept in state for the front page.
#[derive(Clone)]
pub struct EndpointDoc {
    pub surface: Surface,
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
            surface: Surface::Page,
            path: "/pharmacies",
            summary: "Map of the pharmacies on duty in Attica, with opening hours, phone \
                      numbers and directions.",
            parameters: &[],
            route: get(map::pharmacies),
        },
        Endpoint {
            surface: Surface::Page,
            path: "/hospitals",
            summary: "Map of the hospitals on call in Attica, by clinical speciality.",
            parameters: &[],
            route: get(map::hospitals),
        },
        Endpoint {
            surface: Surface::Api,
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
            surface: Surface::Api,
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
            surface: Surface::Api,
            path: "/api/v1/entities/{id}",
            summary: "One place by its numeric id.",
            parameters: &[],
            route: get(routes::entity),
        },
        Endpoint {
            surface: Surface::Api,
            path: "/api/v1/sources",
            summary: "The data sources in use, with their publishers and terms.",
            parameters: &[],
            route: get(routes::sources),
        },
        Endpoint {
            surface: Surface::Api,
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
    /// How often this deployment refreshes its data, so the site can say so rather
    /// than leaving a reader to guess how stale what they see might be.
    pub refresh: Option<Duration>,
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
        .route("/assets/{*path}", get(assets::asset))
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
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        ))
        .with_state(AppState {
            db,
            endpoints: Arc::new(documented),
            refresh: config.update_interval,
        })
}

pub async fn serve(config: Config, db: Db) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|source| crate::Error::io(config.bind.to_string(), source))?;

    // The updater runs beside the server rather than inside a request, so a slow source
    // cannot hold up a reader and a failing one cannot bring the site down.
    let updater = config.update_interval.map(|interval| {
        tracing::info!(
            every_minutes = interval.as_secs() / 60,
            "refreshing data in the background"
        );
        tokio::spawn(crate::update::run_forever(
            config.clone(),
            db.clone(),
            interval,
        ))
    });
    if updater.is_none() {
        tracing::info!("background updating is off; run `greekdata ingest` to refresh");
    }

    tracing::info!(address = %config.bind, "serving");
    let outcome = axum::serve(listener, router(&config, db))
        .await
        .map_err(|source| crate::Error::io("http server", source));

    if let Some(updater) = updater {
        updater.abort();
    }
    outcome
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

/// Escapes text for HTML.
///
/// Everything rendered by this server comes from the source code rather than from a
/// request, but escaping is applied anyway: the moment something becomes user- or
/// data-derived, forgetting it would be an injection.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_in_text_is_escaped() {
        assert_eq!(
            escape("<script>alert('x' & \"y\")</script>"),
            "&lt;script&gt;alert(&#39;x&#39; &amp; &quot;y&quot;)&lt;/script&gt;"
        );
    }

    /// The origin the map script loads tiles from, read out of the script itself.
    fn tile_origin() -> String {
        let script = include_str!("../../assets/app.js");
        let url = script
            .split("L.tileLayer(\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("app.js should create a tile layer");

        let after_scheme = url.find("://").expect("an absolute tile URL") + 3;
        let host_end = url[after_scheme..]
            .find('/')
            .map_or(url.len(), |at| after_scheme + at);
        url[..host_end].to_string()
    }

    #[test]
    fn the_policy_allows_the_tile_server_the_map_actually_uses() {
        // The policy and the script live in different files and different languages, so
        // nothing but a test stops them drifting apart — and when they do, the map draws
        // as an empty grey grid with the reason only in the browser console.
        let origin = tile_origin();
        assert!(
            CONTENT_SECURITY_POLICY.contains(&origin),
            "app.js loads tiles from {origin}, which the policy does not allow:\n  {CONTENT_SECURITY_POLICY}"
        );
    }

    #[test]
    fn the_policy_still_refuses_everything_else() {
        assert!(CONTENT_SECURITY_POLICY.starts_with("default-src 'none'"));
        assert!(CONTENT_SECURITY_POLICY.contains("script-src 'self'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("unsafe-inline"));
        assert!(!CONTENT_SECURITY_POLICY.contains("unsafe-eval"));
    }

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

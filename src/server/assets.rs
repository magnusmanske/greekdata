//! Static files, compiled into the binary.
//!
//! Embedding rather than reading from disk keeps deployment to a single file and means
//! the server cannot be made to serve something outside this list. Leaflet is vendored
//! rather than loaded from a CDN, so the site depends on no third party at run time and
//! the content security policy can refuse every external script.

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

/// The complete set of files this server will serve, and nothing else.
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "site.css",
        "text/css; charset=utf-8",
        include_bytes!("../../assets/site.css"),
    ),
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/app.js"),
    ),
    (
        "leaflet.css",
        "text/css; charset=utf-8",
        include_bytes!("../../assets/leaflet.css"),
    ),
    (
        "leaflet.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/leaflet.js"),
    ),
    (
        "images/marker-icon.png",
        "image/png",
        include_bytes!("../../assets/marker-icon.png"),
    ),
    (
        "images/marker-icon-2x.png",
        "image/png",
        include_bytes!("../../assets/marker-icon-2x.png"),
    ),
    (
        "images/marker-shadow.png",
        "image/png",
        include_bytes!("../../assets/marker-shadow.png"),
    ),
];

pub async fn asset(Path(path): Path<String>) -> Response {
    let Some((_, content_type, bytes)) = ASSETS.iter().find(|(name, _, _)| *name == path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    (
        [
            (header::CONTENT_TYPE, *content_type),
            // These change only when the binary does.
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        *bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_has_content_and_a_unique_name() {
        let mut names: Vec<&str> = ASSETS.iter().map(|(name, _, _)| *name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate asset name");

        for (name, content_type, bytes) in ASSETS {
            assert!(!bytes.is_empty(), "{name} is empty");
            assert!(!content_type.is_empty(), "{name} has no content type");
        }
    }

    #[tokio::test]
    async fn an_unknown_path_is_not_found_rather_than_read_from_disk() {
        let response = asset(Path("../../etc/passwd".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_known_asset_is_served_with_its_type() {
        let response = asset(Path("app.js".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/javascript; charset=utf-8")
        );
    }
}

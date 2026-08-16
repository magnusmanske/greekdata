//! The map pages.
//!
//! One template serves every data group: the page is a shell of controls plus an empty
//! map, and `app.js` fills it from the same public API anyone else can call. What each
//! page shows is carried in `data-` attributes, because the content security policy
//! allows no inline script.

use super::escape;
use crate::{model::EntityKind, sources};
use axum::response::Html;
use std::fmt::Write;

/// A map page: which kind of place it plots, and how to describe it.
pub struct MapPage {
    pub kind: EntityKind,
    pub title: &'static str,
    pub intro: &'static str,
}

pub const PHARMACIES: MapPage = MapPage {
    kind: EntityKind::Pharmacy,
    title: "Pharmacies on duty",
    intro: "Pharmacies rostered to open outside normal hours in Attica.",
};

pub const HOSPITALS: MapPage = MapPage {
    kind: EntityKind::Hospital,
    title: "Hospitals on call",
    intro: "Hospitals taking admissions in Attica, by clinical speciality.",
};

pub async fn pharmacies() -> Html<String> {
    Html(render(&PHARMACIES))
}

pub async fn hospitals() -> Html<String> {
    Html(render(&HOSPITALS))
}

fn render(page: &MapPage) -> String {
    let mut html = String::with_capacity(4 * 1024);

    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str(
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1, \
         viewport-fit=cover\">\n",
    );
    let _ = writeln!(html, "<title>{} — greekdata</title>", escape(page.title));
    let _ = writeln!(
        html,
        "<meta name=\"description\" content=\"{}\">",
        escape(page.intro)
    );
    html.push_str("<link rel=\"stylesheet\" href=\"/assets/leaflet.css\">\n");
    html.push_str("<link rel=\"stylesheet\" href=\"/assets/site.css\">\n");
    html.push_str("<script src=\"/assets/leaflet.js\" defer></script>\n");
    html.push_str("<script src=\"/assets/app.js\" defer></script>\n");
    html.push_str("</head>\n");

    // The script reads what to show from here rather than from an inline script.
    let _ = writeln!(
        html,
        "<body class=\"map\" data-kind=\"{}\" data-today=\"{}\">",
        escape(page.kind.as_str()),
        escape(&sources::today().to_string())
    );

    html.push_str("<header class=\"bar\">\n");
    html.push_str("<a class=\"home\" href=\"/\" aria-label=\"About this service\">&#9432;</a>\n");
    let _ = writeln!(html, "<h1>{}</h1>", escape(page.title));
    // Labelled by attribute rather than a hidden <label>, which screen readers skip.
    html.push_str(
        "<input type=\"date\" id=\"date\" aria-label=\"Date to show\">\n\
         <button type=\"button\" id=\"locate\">Near me</button>\n",
    );
    html.push_str("</header>\n");

    html.push_str("<div id=\"map\"></div>\n");
    html.push_str("<div class=\"results\" id=\"results-panel\">\n");
    html.push_str("<p class=\"status\" id=\"status\" role=\"status\">Loading…</p>\n");
    html.push_str("<div id=\"results\"></div>\n");
    html.push_str("</div>\n");

    html.push_str(
        "<noscript><p class=\"status\">This map needs JavaScript. The same data \
                   is available without it from <a href=\"/api/v1/on-call\">the API</a>.\
                   </p></noscript>\n",
    );
    html.push_str("</body>\n</html>\n");

    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_page_tells_the_script_what_to_plot() {
        let html = render(&PHARMACIES);
        assert!(html.contains("data-kind=\"pharmacy\""));
        assert!(html.contains("data-today=\""));
        assert!(html.contains("/assets/app.js"));
        assert!(html.contains("id=\"map\""));
    }

    #[test]
    fn each_page_plots_its_own_kind() {
        assert!(render(&HOSPITALS).contains("data-kind=\"hospital\""));
        assert!(!render(&HOSPITALS).contains("data-kind=\"pharmacy\""));
    }

    #[test]
    fn the_page_carries_no_inline_script() {
        // Anything inline would be blocked by the content security policy anyway;
        // this catches it at build time instead of in the browser console.
        for page in [&PHARMACIES, &HOSPITALS] {
            let html = render(page);
            for opening in html.match_indices("<script") {
                let tag = &html[opening.0..];
                let end = tag.find('>').unwrap_or(0);
                assert!(
                    tag[..end].contains("src="),
                    "inline script in {}",
                    page.title
                );
            }
            assert!(
                !html.contains(" onclick="),
                "inline handler in {}",
                page.title
            );
        }
    }

    #[test]
    fn there_is_a_way_back_to_the_explanation() {
        assert!(render(&PHARMACIES).contains("href=\"/\""));
    }
}

//! The front page: what this service is, what it offers, and what it does not promise.
//!
//! The endpoint list is rendered from the same table that builds the router, so it
//! cannot fall out of date. The sources list comes from the registered sources, so a new
//! data group credits its publisher here automatically.

use super::{AppState, EndpointDoc, Parameter, Surface, escape};
use axum::{extract::State, response::Html};
use std::fmt::Write;

pub async fn index(State(state): State<AppState>) -> Html<String> {
    Html(render(&state.endpoints))
}

fn render(endpoints: &[EndpointDoc]) -> String {
    let mut page = String::with_capacity(8 * 1024);

    page.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    page.push_str("<meta charset=\"utf-8\">\n");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    page.push_str("<title>greekdata</title>\n");
    page.push_str("<link rel=\"stylesheet\" href=\"/assets/site.css\">\n");
    page.push_str("</head>\n<body class=\"prose\">\n");

    page.push_str("<h1>greekdata</h1>\n");
    page.push_str(
        "<p class=\"tagline\">Public-interest data from Greece, collected from official \
         sources, normalized, and served as JSON.</p>\n",
    );

    page.push_str(
        "<p>This service reads what Greek public bodies publish — currently which \
         pharmacies and hospitals are on duty in Attica — and turns it into structured \
         data you can query by date and location. The published documents are HTML pages \
         and PDF tables meant for people to read; this turns them into something a \
         program can use.</p>\n",
    );
    page.push_str(
        "<p>Every record keeps a link to the document it came from and the day that \
         document covers. Nothing is overwritten: when a rota is reissued with \
         corrections, the earlier version stays in the database and the corrected one is \
         served.</p>\n",
    );
    page.push_str(
        "<p>The maps below plot what is on duty today, and will route you there. \
         Pharmacy positions are published with the roster itself. Hospital rotas give \
         only names, so those positions are matched in from Wikidata where they can be \
         found unambiguously — a popup says so when that is the case, and a hospital \
         with no confident position is listed under the map rather than pinned to a \
         guess.</p>\n",
    );

    page.push_str("<h2>Maps</h2>\n");
    page.push_str("<div class=\"maplinks\">\n");
    for endpoint in endpoints.iter().filter(|e| e.surface == Surface::Page) {
        let _ = writeln!(
            page,
            "<a href=\"{}\">{}</a>",
            escape(&endpoint.path),
            escape(&page_label(&endpoint.path))
        );
    }
    page.push_str("</div>\n");
    for endpoint in endpoints.iter().filter(|e| e.surface == Surface::Page) {
        render_endpoint(&mut page, endpoint);
    }

    page.push_str("<h2>API</h2>\n");
    page.push_str(
        "<p>All responses are JSON, so any of these can also be saved as a data file.</p>\n",
    );
    for endpoint in endpoints.iter().filter(|e| e.surface == Surface::Api) {
        render_endpoint(&mut page, endpoint);
    }

    page.push_str("<h2>Where the data comes from</h2>\n");
    render_sources(&mut page);

    page.push_str("<h2>Disclaimer</h2>\n");
    page.push_str("<div class=\"disclaimer\">\n");
    page.push_str(
        "<p>This service automatically processes data published by third parties. The \
         source documents are written by hand and are frequently inconsistent or \
         mistaken, and the automated reading of them can introduce further errors. The \
         output may therefore be incomplete, out of date, or simply wrong.</p>\n",
    );
    page.push_str(
        "<p>Positions shown on the maps, and the routes offered from them, may be wrong: \
         hospital positions in particular are matched from a third-party source and are \
         not part of what the ministry published.</p>\n",
    );
    page.push_str(
        "<p>No legal responsibility is accepted for the output or for anything done on \
         the basis of it. It is provided as-is, with no warranty of any kind. <strong>Do \
         not rely on it in an emergency or for any medical decision.</strong> Check the \
         original source, linked with every record, or call the official services.</p>\n",
    );
    page.push_str(
        "<p>Copyright in the source material remains with the bodies that published it, \
         listed above.</p>\n",
    );
    page.push_str("</div>\n");

    page.push_str(
        "<footer>Data is refreshed from the sources above; each record carries the date \
         of the document it came from.</footer>\n",
    );
    page.push_str("</body>\n</html>\n");

    page
}

/// A button label for a page path: `/pharmacies` reads better as "Pharmacies".
fn page_label(path: &str) -> String {
    let name = path.trim_start_matches('/');
    let mut characters = name.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => name.to_string(),
    }
}

fn render_endpoint(page: &mut String, endpoint: &EndpointDoc) {
    page.push_str("<div class=\"endpoint\">\n");
    let _ = write!(
        page,
        "<code class=\"path\">GET {}</code>\n<p>{}</p>\n",
        escape(&endpoint.path),
        prose(endpoint.summary)
    );

    if !endpoint.parameters.is_empty() {
        page.push_str("<ul class=\"params\">\n");
        for Parameter { name, description } in endpoint.parameters {
            let _ = writeln!(
                page,
                "<li><code>{}</code> — {}</li>",
                escape(name),
                prose(description)
            );
        }
        page.push_str("</ul>\n");
    }
    page.push_str("</div>\n");
}

fn render_sources(page: &mut String) {
    for source in crate::sources::all() {
        let attribution = source.attribution();
        page.push_str("<div class=\"source\">\n");
        let _ = write!(
            page,
            "<strong>{}</strong> — {}<br>\n<a href=\"{}\">{}</a><br>\n\
             <code>{}</code> · {}\n",
            escape(attribution.publisher),
            escape(&source.group().to_string()),
            escape(attribution.homepage),
            escape(attribution.homepage),
            escape(source.id()),
            escape(attribution.terms),
        );
        page.push_str("</div>\n");
    }
}

/// Escapes text for HTML and turns `backticked` spans into code.
///
/// Descriptions are written in the endpoint table as if they were markdown, because that
/// is how they read in the source. An unpaired backtick is left as written.
fn prose(text: &str) -> String {
    let escaped = escape(text);
    let mut out = String::with_capacity(escaped.len());
    let mut rest = escaped.as_str();

    while let Some((before, after)) = rest.split_once('`') {
        let Some((code, remainder)) = after.split_once('`') else {
            break;
        };
        out.push_str(before);
        out.push_str("<code>");
        out.push_str(code);
        out.push_str("</code>");
        rest = remainder;
    }
    out.push_str(rest);

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        let endpoints: Vec<EndpointDoc> = super::super::endpoint_docs();
        render(&endpoints)
    }

    #[test]
    fn the_page_lists_every_endpoint_the_router_serves() {
        let rendered = page();
        for endpoint in super::super::endpoint_docs() {
            assert!(
                rendered.contains(&escape(&endpoint.path)),
                "{} is missing from the page",
                endpoint.path
            );
        }
    }

    #[test]
    fn the_page_credits_every_source() {
        let rendered = page();
        for source in crate::sources::all() {
            assert!(
                rendered.contains(&escape(source.attribution().publisher)),
                "{} is not credited",
                source.id()
            );
        }
    }

    #[test]
    fn the_page_carries_the_disclaimer() {
        let rendered = page();
        assert!(rendered.contains("automatically processes data published by third parties"));
        assert!(rendered.contains("No legal responsibility is accepted"));
        assert!(rendered.contains("Do not rely on it in an emergency"));
    }

    #[test]
    fn backticked_spans_become_code_and_stay_escaped() {
        assert_eq!(
            prose("day as `YYYY-MM-DD`, or `<today>`"),
            "day as <code>YYYY-MM-DD</code>, or <code>&lt;today&gt;</code>"
        );
        // An unpaired backtick must not swallow the rest of the text.
        assert_eq!(prose("a ` dangling tick"), "a ` dangling tick");
        assert_eq!(prose("no ticks at all"), "no ticks at all");
    }
}

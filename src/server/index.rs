//! The front page: what this service is, what it offers, and what it does not promise.
//!
//! The endpoint list is rendered from the same table that builds the router, so it
//! cannot fall out of date. The sources list comes from the registered sources, so a new
//! data group credits its publisher here automatically.

use super::{AppState, EndpointDoc, Parameter};
use axum::{extract::State, response::Html};
use std::fmt::Write;

const STYLE: &str = "\
:root { color-scheme: light dark; }
body { max-width: 46rem; margin: 0 auto; padding: 2rem 1rem 4rem;
       font: 1rem/1.6 system-ui, sans-serif; }
h1 { margin-bottom: 0.2rem; }
.tagline { margin-top: 0; opacity: 0.75; }
h2 { margin-top: 2.5rem; border-bottom: 1px solid rgba(128,128,128,0.35);
     padding-bottom: 0.3rem; }
code { font-family: ui-monospace, monospace; font-size: 0.9em; }
.endpoint { margin-bottom: 1.5rem; }
.endpoint code.path { font-size: 1rem; font-weight: 600; }
.params { margin: 0.4rem 0 0; padding-left: 1.2rem; font-size: 0.9rem; opacity: 0.85; }
.source { margin-bottom: 0.9rem; }
.disclaimer { border: 1px solid rgba(128,128,128,0.4); border-radius: 0.4rem;
              padding: 0.8rem 1rem; background: rgba(128,128,128,0.08); }
footer { margin-top: 3rem; font-size: 0.85rem; opacity: 0.7; }
";

pub async fn index(State(state): State<AppState>) -> Html<String> {
    Html(render(&state.endpoints))
}

fn render(endpoints: &[EndpointDoc]) -> String {
    let mut page = String::with_capacity(8 * 1024);

    page.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    page.push_str("<meta charset=\"utf-8\">\n");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    page.push_str("<title>greekdata</title>\n");
    let _ = writeln!(page, "<style>{STYLE}</style>");
    page.push_str("</head>\n<body>\n");

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

    page.push_str("<h2>Available endpoints</h2>\n");
    page.push_str(
        "<p>All responses are JSON, so any of these can also be saved as a data file.</p>\n",
    );
    for endpoint in endpoints {
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

/// Escapes text for HTML.
///
/// Everything on this page comes from the source code rather than from a request, but
/// escaping is applied anyway: the moment something here becomes user- or data-derived,
/// forgetting it would be an injection.
fn escape(text: &str) -> String {
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

    #[test]
    fn markup_in_text_is_escaped() {
        assert_eq!(
            escape("<script>alert('x' & \"y\")</script>"),
            "&lt;script&gt;alert(&#39;x&#39; &amp; &quot;y&quot;)&lt;/script&gt;"
        );
    }
}

//! Reading PDFs as positioned words rather than as a stream of text.
//!
//! The rotas we parse are tables: a hospital name means nothing without the column it
//! sits in. Flat text extraction destroys that, so this module collects each glyph with
//! its position on the page, assembles glyphs into words and lines, and leaves the
//! interpretation of the layout to [`table`].

pub mod table;

use crate::{Error, Result};
use pdf_extract::{ColorSpace, Document, MediaBox, OutputDev, OutputError, Path, Transform};

/// Fraction of the font size that a horizontal gap must exceed to separate two words.
const WORD_GAP_RATIO: f64 = 0.28;

/// How far apart two baselines may be and still count as the same line.
const LINE_TOLERANCE_RATIO: f64 = 0.4;
const MIN_LINE_TOLERANCE: f64 = 1.5;

/// A single positioned glyph, as the PDF content stream draws it.
#[derive(Debug, Clone, PartialEq)]
struct Glyph {
    text: String,
    x: f64,
    y: f64,
    advance: f64,
    font_size: f64,
}

/// A run of glyphs with no meaningful gap between them, in page coordinates with the
/// origin at the top left and y increasing downwards.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub font_size: f64,
}

impl Word {
    /// The horizontal midpoint, which is what decides the column a word belongs to.
    pub fn centre_x(&self) -> f64 {
        self.x + self.width / 2.0
    }

    pub fn end_x(&self) -> f64 {
        self.x + self.width
    }
}

/// Words sharing a baseline, ordered left to right.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub y: f64,
    pub words: Vec<Word>,
}

impl Line {
    /// The whole line as text, with single spaces between words.
    pub fn text(&self) -> String {
        self.words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One page, as lines of positioned words ordered top to bottom.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Page {
    pub number: u32,
    pub width: f64,
    pub height: f64,
    pub lines: Vec<Line>,
}

impl Page {
    pub fn words(&self) -> impl Iterator<Item = &Word> {
        self.lines.iter().flat_map(|line| line.words.iter())
    }
}

/// Extracts every page of a PDF as positioned lines and words.
pub fn extract(bytes: &[u8]) -> Result<Vec<Page>> {
    let mut document =
        Document::load_mem(bytes).map_err(|error| Error::parse("pdf", error.to_string()))?;
    inline_indirect_glyph_widths(&mut document);

    let mut collector = GlyphCollector::default();
    pdf_extract::output_doc(&document, &mut collector)
        .map_err(|error| Error::parse("pdf content", error.to_string()))?;

    Ok(collector
        .pages
        .into_iter()
        .map(|(number, width, height, glyphs)| Page {
            number,
            width,
            height,
            lines: assemble(glyphs),
        })
        .collect())
}

/// Keys whose values must be literal for the glyph widths to be read correctly.
const WIDTH_KEYS: [&[u8]; 2] = [b"W", b"DW"];

/// A `c_first c_last width` run longer than this is left alone rather than expanded.
const MAX_WIDTH_RUN: i64 = 4096;

/// Repairs CID font glyph widths in memory so `pdf-extract` reads them correctly.
///
/// Two things defeat it on the ministry's "Print to PDF" output, and both make every
/// glyph fall back to the default width of 1000 — which throws x positions off by more
/// than a table column, putting hospitals in the wrong shift:
///
/// 1. `/W` is written as an indirect reference, and is read without resolving it.
/// 2. The array uses the `c_first c_last width` run form, which its parser mis-reads.
///
/// Resolving the reference and rewriting runs into the equivalent `cid [w ...]` form
/// fixes both, without patching or forking the crate. Both forms are valid PDF, so this
/// only makes the document more explicit.
fn inline_indirect_glyph_widths(document: &mut Document) {
    let mut resolved: Vec<(pdf_extract::ObjectId, pdf_extract::Object)> = Vec::new();
    for object in document.objects.values() {
        collect_width_references(document, object, &mut resolved);
    }
    let resolved: std::collections::HashMap<_, _> = resolved.into_iter().collect();

    let mut objects = std::mem::take(&mut document.objects);
    for object in objects.values_mut() {
        inline_width_references(object, &resolved);
    }
    document.objects = objects;
}

fn collect_width_references(
    document: &Document,
    object: &pdf_extract::Object,
    out: &mut Vec<(pdf_extract::ObjectId, pdf_extract::Object)>,
) {
    match object {
        pdf_extract::Object::Dictionary(dictionary) => {
            for (key, value) in dictionary.iter() {
                if let (true, pdf_extract::Object::Reference(id)) =
                    (WIDTH_KEYS.contains(&key.as_slice()), value)
                    && let Ok(target) = document.get_object(*id)
                {
                    out.push((*id, resolve_array_elements(document, target)));
                }
                collect_width_references(document, value, out);
            }
        }
        pdf_extract::Object::Array(items) => {
            for item in items {
                collect_width_references(document, item, out);
            }
        }
        _ => {}
    }
}

/// A width array may itself hold references; flatten one level so every entry is a
/// number or a literal array of numbers.
fn resolve_array_elements(
    document: &Document,
    object: &pdf_extract::Object,
) -> pdf_extract::Object {
    let pdf_extract::Object::Array(items) = object else {
        return object.clone();
    };

    pdf_extract::Object::Array(
        items
            .iter()
            .map(|item| match item {
                pdf_extract::Object::Reference(id) => document
                    .get_object(*id)
                    .map_or_else(|_| item.clone(), |target| target.clone()),
                other => other.clone(),
            })
            .collect(),
    )
}

fn inline_width_references(
    object: &mut pdf_extract::Object,
    resolved: &std::collections::HashMap<pdf_extract::ObjectId, pdf_extract::Object>,
) {
    match object {
        pdf_extract::Object::Dictionary(dictionary) => {
            for (key, value) in dictionary.iter_mut() {
                if !WIDTH_KEYS.contains(&key.as_slice()) {
                    inline_width_references(value, resolved);
                    continue;
                }
                if let pdf_extract::Object::Reference(id) = &*value
                    && let Some(literal) = resolved.get(id)
                {
                    *value = literal.clone();
                }
                if let pdf_extract::Object::Array(items) = value {
                    *items = expand_width_runs(items);
                }
            }
        }
        pdf_extract::Object::Array(items) => {
            for item in items {
                inline_width_references(item, resolved);
            }
        }
        _ => {}
    }
}

/// Rewrites `c_first c_last width` runs in a CID width array into `cid [w w ...]`.
///
/// Entries already in the `cid [w ...]` form are passed through untouched.
fn expand_width_runs(items: &[pdf_extract::Object]) -> Vec<pdf_extract::Object> {
    let mut out = Vec::with_capacity(items.len());
    let mut index = 0;

    while index < items.len() {
        match (items.get(index), items.get(index + 1), items.get(index + 2)) {
            // Already the explicit form: a starting CID followed by a list of widths.
            (Some(cid), Some(widths @ pdf_extract::Object::Array(_)), _) => {
                out.push(cid.clone());
                out.push(widths.clone());
                index += 2;
            }
            (Some(first), Some(last), Some(width)) => {
                let (Some(first), Some(last)) = (as_integer(first), as_integer(last)) else {
                    break;
                };
                // `c_last` is inclusive, and a malformed or enormous run is left alone.
                let count = last - first + 1;
                if count <= 0 || count > MAX_WIDTH_RUN {
                    out.extend_from_slice(&items[index..index + 3]);
                } else {
                    out.push(pdf_extract::Object::Integer(first));
                    out.push(pdf_extract::Object::Array(vec![
                        width.clone();
                        count as usize
                    ]));
                }
                index += 3;
            }
            _ => break,
        }
    }

    out
}

/// Maps the `U+F0xx` private-use block back onto ASCII.
///
/// Symbol-encoded fonts conventionally place ASCII at `U+F020`–`U+F0FF`. Left alone, the
/// space in those fonts arrives as `U+F020`, which is not whitespace to Rust and so ends
/// up looking like a one-character hospital name.
fn unmap_symbol_range(text: &str) -> String {
    text.chars()
        .map(|c| match u32::from(c) {
            code @ 0xF020..=0xF0FF => char::from_u32(code - 0xF000).unwrap_or(c),
            _ => c,
        })
        .collect()
}

fn as_integer(object: &pdf_extract::Object) -> Option<i64> {
    match object {
        pdf_extract::Object::Integer(value) => Some(*value),
        pdf_extract::Object::Real(value) => Some(*value as i64),
        _ => None,
    }
}

/// Groups glyphs into lines by baseline, then into words by horizontal gap.
///
/// `pdf-extract` reports one glyph at a time and its word callbacks do not correspond to
/// visual words, so spacing has to be inferred from geometry — which is what a reader
/// does too.
fn assemble(mut glyphs: Vec<Glyph>) -> Vec<Line> {
    glyphs.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));

    let mut lines: Vec<Vec<Glyph>> = Vec::new();
    for glyph in glyphs {
        let tolerance = (glyph.font_size * LINE_TOLERANCE_RATIO).max(MIN_LINE_TOLERANCE);
        match lines.last_mut() {
            Some(line)
                if line
                    .last()
                    .is_some_and(|last| (glyph.y - last.y).abs() <= tolerance) =>
            {
                line.push(glyph);
            }
            _ => lines.push(vec![glyph]),
        }
    }

    lines
        .into_iter()
        .filter_map(|mut line| {
            line.sort_by(|a, b| a.x.total_cmp(&b.x));
            let y = line.first()?.y;
            let words = split_into_words(line);
            (!words.is_empty()).then_some(Line { y, words })
        })
        .collect()
}

/// Splits one baseline's glyphs into words.
///
/// These files are produced by "Print to PDF", which draws an explicit space glyph
/// between words and positions every other glyph absolutely. So a space glyph is the
/// authoritative separator; the gap test is a fallback for documents that omit them.
fn split_into_words(glyphs: Vec<Glyph>) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut break_before_next = false;

    for glyph in glyphs {
        if glyph.text.trim().is_empty() {
            break_before_next = true;
            continue;
        }

        let gap_limit = glyph.font_size * WORD_GAP_RATIO;
        let joins = !break_before_next
            && words
                .last()
                .is_some_and(|word| glyph.x - word.end_x() <= gap_limit);
        break_before_next = false;

        match words.last_mut() {
            Some(word) if joins => {
                word.text.push_str(&glyph.text);
                word.width = (glyph.x + glyph.advance) - word.x;
            }
            _ => words.push(Word {
                text: glyph.text,
                x: glyph.x,
                y: glyph.y,
                width: glyph.advance,
                font_size: glyph.font_size,
            }),
        }
    }

    words
}

/// Accumulates glyphs as `pdf-extract` walks the content streams.
#[derive(Default)]
struct GlyphCollector {
    /// One entry per page: number, width, height, glyphs.
    pages: Vec<(u32, f64, f64, Vec<Glyph>)>,
    page_height: f64,
}

impl OutputDev for GlyphCollector {
    fn begin_page(
        &mut self,
        page_num: u32,
        media_box: &MediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> std::result::Result<(), OutputError> {
        self.page_height = media_box.ury - media_box.lly;
        self.pages.push((
            page_num,
            media_box.urx - media_box.llx,
            self.page_height,
            Vec::new(),
        ));
        Ok(())
    }

    fn end_page(&mut self) -> std::result::Result<(), OutputError> {
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        character: &str,
    ) -> std::result::Result<(), OutputError> {
        let Some((_, _, _, glyphs)) = self.pages.last_mut() else {
            return Ok(());
        };

        // The text rendering matrix places the glyph. PDF's origin is bottom left, so
        // flip y to match how a reader sees the page.
        glyphs.push(Glyph {
            text: unmap_symbol_range(character),
            x: trm.m31,
            y: self.page_height - trm.m32,
            // `width` is a glyph advance in text space; the matrix scales it to the page.
            advance: width * font_size * trm.m11.abs(),
            font_size,
        });

        Ok(())
    }

    fn begin_word(&mut self) -> std::result::Result<(), OutputError> {
        Ok(())
    }

    fn end_word(&mut self) -> std::result::Result<(), OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> std::result::Result<(), OutputError> {
        Ok(())
    }

    fn stroke(
        &mut self,
        _ctm: &Transform,
        _colorspace: &ColorSpace,
        _color: &[f64],
        _path: &Path,
    ) -> std::result::Result<(), OutputError> {
        Ok(())
    }

    fn fill(
        &mut self,
        _ctm: &Transform,
        _colorspace: &ColorSpace,
        _color: &[f64],
        _path: &Path,
    ) -> std::result::Result<(), OutputError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(text: &str, x: f64, y: f64) -> Glyph {
        Glyph {
            text: text.into(),
            x,
            y,
            advance: 10.0,
            font_size: 10.0,
        }
    }

    #[test]
    fn glyphs_on_one_baseline_become_words_split_at_the_gaps() {
        // "ΑΒ ΓΔ": a tight pair, a space-sized gap, another tight pair.
        let lines = assemble(vec![
            glyph("Α", 0.0, 100.0),
            glyph("Β", 10.0, 100.0),
            glyph("Γ", 30.0, 100.0),
            glyph("Δ", 40.0, 100.0),
        ]);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "ΑΒ ΓΔ");
        assert_eq!(lines[0].words[0].x, 0.0);
        assert_eq!(lines[0].words[1].x, 30.0);
    }

    #[test]
    fn separate_baselines_become_separate_lines_ordered_downwards() {
        let lines = assemble(vec![
            glyph("Β", 0.0, 120.0),
            glyph("Α", 0.0, 100.0),
            // Within tolerance of the first baseline: same line, despite the wobble.
            glyph("Γ", 20.0, 100.9),
        ]);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text(), "Α Γ");
        assert_eq!(lines[1].text(), "Β");
    }

    #[test]
    fn word_geometry_survives_assembly() {
        let lines = assemble(vec![glyph("Α", 5.0, 10.0), glyph("Β", 15.0, 10.0)]);
        let word = &lines[0].words[0];

        assert_eq!(word.x, 5.0);
        assert_eq!(word.end_x(), 25.0);
        assert_eq!(word.centre_x(), 15.0);
    }

    #[test]
    fn greek_text_survives_extraction_from_a_real_ministry_pdf() {
        // The risk this whole approach turns on: that a pure-Rust reader decodes the
        // Greek CID fonts these files use.
        let bytes = include_bytes!("../../tests/fixtures/moh_hospitals_20260817.pdf");
        let pages = extract(bytes).expect("the ministry PDF parses");

        assert_eq!(pages.len(), 4);
        let heading = pages[0].lines.first().expect("a first line");
        assert_eq!(heading.text(), "ΔΕΥΤΕΡΑ 17 ΑΥΓΟΥΣΤΟΥ 2026");

        // Column headers are time ranges, and they must survive as single words.
        assert!(
            pages[0].words().any(|word| word.text == "08:00-14:30"),
            "no clock times found"
        );
    }

    #[test]
    fn glyph_positions_are_accurate_enough_to_tell_columns_apart() {
        // Positions verified against poppler's `pdftotext -bbox-layout` on the same
        // file; these are the ones a mis-read width array would move by tens of points.
        let bytes = include_bytes!("../../tests/fixtures/moh_hospitals_20260817.pdf");
        let pages = extract(bytes).expect("the ministry PDF parses");

        let expected = [
            ("ΝΟΣΟΚΟΜΕΙΑ", 380.76, 63.65),
            ("ΠΡΩΙΝΗΣ", 447.00, 43.31),
            ("ΛΕΙΤΟΥΡΓΙΑΣ", 492.95, 61.45),
            ("08:00-14:30", 557.04, 56.31),
            ("ΟΜΑΔΕΣ", 615.96, 38.51),
        ];
        for (text, x, width) in expected {
            let word = pages[0]
                .words()
                .find(|word| word.text == text)
                .unwrap_or_else(|| panic!("missing word {text}"));
            assert!(
                (word.x - x).abs() < 0.05,
                "{text}: x {} should be {x}",
                word.x
            );
            assert!(
                (word.width - width).abs() < 0.05,
                "{text}: width {} should be {width}",
                word.width
            );
        }
    }

    #[test]
    fn width_runs_expand_into_the_explicit_form() {
        use pdf_extract::Object;

        // `3 5 226` means CIDs 3 through 5 inclusive are 226 units wide.
        let expanded =
            expand_width_runs(&[Object::Integer(3), Object::Integer(5), Object::Integer(226)]);
        assert_eq!(
            expanded,
            vec![
                Object::Integer(3),
                Object::Array(vec![Object::Integer(226); 3]),
            ]
        );

        // The explicit form is already correct and must pass through untouched.
        let explicit = vec![
            Object::Integer(7),
            Object::Array(vec![Object::Integer(500), Object::Integer(600)]),
        ];
        assert_eq!(expand_width_runs(&explicit), explicit);
    }

    #[test]
    fn an_absurd_width_run_is_left_alone_rather_than_expanded() {
        use pdf_extract::Object;

        let huge = vec![
            Object::Integer(0),
            Object::Integer(65535),
            Object::Integer(1000),
        ];
        assert_eq!(expand_width_runs(&huge), huge);
    }
}

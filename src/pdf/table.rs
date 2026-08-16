//! Rebuilding a table from positioned words.
//!
//! These rotas carry their meaning in the layout: which time-slot column a hospital sits
//! in, and which clinical speciality row. Nothing is drawn as a grid — there are no rule
//! lines to follow — so the columns are derived from the header row's cells and the rows
//! from the labels down the left-hand side.

use super::{Line, Page, Word};

/// Words in the header further apart than this start a new column.
const MIN_COLUMN_GAP: f64 = 20.0;
const COLUMN_GAP_RATIO: f64 = 2.0;

/// How far above a row's label a word may sit and still belong to that row.
const ROW_TOLERANCE: f64 = 6.0;

/// How far from the header baseline a word may sit and still be part of the header.
///
/// The row-label cell is sometimes typeset a point or two off the other header cells,
/// which splits it onto its own baseline; without this it would be lost and the label
/// column would disappear.
const HEADER_TOLERANCE: f64 = 6.0;

/// One column of the table, spanning `start..end` horizontally.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub label: String,
    pub start: f64,
    pub end: f64,
}

/// One row: its left-hand label and, for each column, the lines of text in that cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub top: f64,
    /// Indexed by column. Column 0 is the label column and is normally empty here.
    pub cells: Vec<Vec<String>>,
}

impl Row {
    pub fn cell(&self, column: usize) -> &[String] {
        self.cells.get(column).map_or(&[], Vec::as_slice)
    }
}

/// A reconstructed table.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
}

/// Rebuilds the table on `page`, using `is_header` to recognize the header row.
///
/// Returns `None` when the page has no recognizable header — a cover page, say, or a
/// layout change worth noticing rather than guessing about.
pub fn reconstruct(page: &Page, is_header: &dyn Fn(&Line) -> bool) -> Option<Table> {
    let header = header_row(page, is_header)?;
    let columns = columns_from(&header);
    if columns.len() < 2 {
        return None;
    }

    // Everything below the header belongs to the body.
    let body: Vec<&Word> = page
        .words()
        .filter(|word| word.y > header.y + ROW_TOLERANCE)
        .collect();

    let rows = row_starts(&body, &columns);
    if rows.is_empty() {
        return None;
    }

    Some(Table {
        rows: fill_rows(&body, &columns, rows),
        columns,
    })
}

/// Assembles the complete header row, gathering any cells typeset slightly off the
/// baseline that `is_header` matched.
fn header_row(page: &Page, is_header: &dyn Fn(&Line) -> bool) -> Option<Line> {
    let matched = page.lines.iter().find(|line| is_header(line))?;

    let mut words: Vec<Word> = page
        .words()
        .filter(|word| (word.y - matched.y).abs() <= HEADER_TOLERANCE)
        .cloned()
        .collect();
    words.sort_by(|a, b| a.x.total_cmp(&b.x));

    Some(Line {
        y: matched.y,
        words,
    })
}

/// Groups the header line's words into cells, and turns each into a column whose bounds
/// run to the midpoint of the gap to its neighbour.
fn columns_from(header: &Line) -> Vec<Column> {
    let mut cells: Vec<(String, f64, f64)> = Vec::new();

    for word in &header.words {
        let gap_limit = (word.font_size * COLUMN_GAP_RATIO).max(MIN_COLUMN_GAP);
        match cells.last_mut() {
            Some((text, _, end)) if word.x - *end <= gap_limit => {
                text.push(' ');
                text.push_str(&word.text);
                *end = word.end_x();
            }
            _ => cells.push((word.text.clone(), word.x, word.end_x())),
        }
    }

    // A column claims the space up to halfway towards the next header cell, which is
    // what lets a wide entry overflow its header without falling into the next column.
    let count = cells.len();
    cells
        .iter()
        .enumerate()
        .map(|(index, (label, start, end))| Column {
            label: label.clone(),
            start: if index == 0 {
                f64::MIN
            } else {
                midpoint(cells[index - 1].2, *start)
            },
            end: if index + 1 == count {
                f64::MAX
            } else {
                midpoint(*end, cells[index + 1].1)
            },
        })
        .collect()
}

fn midpoint(left: f64, right: f64) -> f64 {
    (left + right) / 2.0
}

/// Finds each row's label and vertical start, from the words in the first column.
fn row_starts(body: &[&Word], columns: &[Column]) -> Vec<(String, f64)> {
    let Some(label_column) = columns.first() else {
        return Vec::new();
    };

    let mut labels: Vec<&Word> = body
        .iter()
        .copied()
        .filter(|word| word.x < label_column.end)
        .collect();
    labels.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));

    let mut rows: Vec<(String, f64)> = Vec::new();
    for word in labels {
        match rows.last_mut() {
            // A label wrapping onto a second line continues the same row.
            Some((text, top)) if word.y - *top <= word.font_size * 1.5 => {
                text.push(' ');
                text.push_str(&word.text);
            }
            _ => rows.push((word.text.clone(), word.y)),
        }
    }

    rows
}

/// Places every body word into the cell its position implies.
fn fill_rows(body: &[&Word], columns: &[Column], starts: Vec<(String, f64)>) -> Vec<Row> {
    let mut rows: Vec<Row> = starts
        .into_iter()
        .map(|(label, top)| Row {
            label,
            top,
            cells: vec![Vec::new(); columns.len()],
        })
        .collect();

    // Collect words per cell first, so each cell's lines can be rebuilt independently.
    let mut buckets: Vec<Vec<Vec<&Word>>> = vec![vec![Vec::new(); columns.len()]; rows.len()];
    for word in body {
        let Some(row) = row_of(&rows, word) else {
            continue;
        };
        let Some(column) = columns
            .iter()
            .position(|column| word.x >= column.start && word.x < column.end)
        else {
            continue;
        };
        // The label column's own text is already the row label.
        if column > 0 {
            buckets[row][column].push(word);
        }
    }

    for (row, cells) in buckets.into_iter().enumerate() {
        for (column, words) in cells.into_iter().enumerate() {
            rows[row].cells[column] = group_into_lines(words);
        }
    }

    rows
}

/// The last row starting at or above this word.
fn row_of(rows: &[Row], word: &Word) -> Option<usize> {
    rows.iter()
        .rposition(|row| word.y >= row.top - ROW_TOLERANCE)
}

/// Rebuilds a cell's visual lines from the words that landed in it.
fn group_into_lines(mut words: Vec<&Word>) -> Vec<String> {
    words.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));

    let mut lines: Vec<(f64, Vec<&Word>)> = Vec::new();
    for word in words {
        let tolerance = (word.font_size * 0.4).max(1.5);
        match lines.last_mut() {
            Some((y, line)) if (word.y - *y).abs() <= tolerance => line.push(word),
            _ => lines.push((word.y, vec![word])),
        }
    }

    lines
        .into_iter()
        .map(|(_, line)| {
            line.iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|text| !text.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, x: f64, y: f64) -> Word {
        Word {
            text: text.into(),
            x,
            y,
            width: text.chars().count() as f64 * 6.0,
            font_size: 9.0,
        }
    }

    fn line(y: f64, words: Vec<Word>) -> Line {
        Line { y, words }
    }

    /// A miniature of the ministry's layout: a label column and two time-slot columns.
    fn sample_page() -> Page {
        Page {
            number: 1,
            width: 600.0,
            height: 400.0,
            lines: vec![
                line(
                    50.0,
                    vec![
                        word("ΚΛΙΝΙΚΕΣ", 30.0, 50.0),
                        word("08:00", 200.0, 50.0),
                        word("–", 232.0, 50.0),
                        word("14:30", 242.0, 50.0),
                        word("14:30", 400.0, 50.0),
                        word("–", 432.0, 50.0),
                        word("08:00", 442.0, 50.0),
                    ],
                ),
                line(
                    80.0,
                    vec![
                        word("Παθολογική", 30.0, 80.0),
                        word("Γ.Ν.Α.", 200.0, 80.0),
                        word("«ΙΠΠΟΚΡΑΤΕΙΟ»", 232.0, 80.0),
                        word("Γ.Ν.Α.", 400.0, 80.0),
                        word("«ΣΩΤΗΡΙΑ»", 432.0, 80.0),
                    ],
                ),
                // A second entry in the first slot, on its own line.
                line(95.0, vec![word("Γ.Ν.Ν.ΙΩΝΙΑΣ", 200.0, 95.0)]),
                line(
                    130.0,
                    vec![
                        word("Καρδιολογική", 30.0, 130.0),
                        word("Γ.Ν.Α.", 400.0, 130.0),
                        word("«ΑΛΕΞΑΝΔΡΑ»", 432.0, 130.0),
                    ],
                ),
            ],
        }
    }

    fn header_predicate(line: &Line) -> bool {
        line.text().starts_with("ΚΛΙΝΙΚΕΣ")
    }

    #[test]
    fn columns_come_from_the_header_cells() {
        let table = reconstruct(&sample_page(), &header_predicate).expect("a table");
        let labels: Vec<&str> = table
            .columns
            .iter()
            .map(|column| column.label.as_str())
            .collect();

        assert_eq!(labels, ["ΚΛΙΝΙΚΕΣ", "08:00 – 14:30", "14:30 – 08:00"]);
    }

    #[test]
    fn each_hospital_lands_in_its_own_shift_column() {
        let table = reconstruct(&sample_page(), &header_predicate).expect("a table");

        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0].label, "Παθολογική");
        assert_eq!(
            table.rows[0].cell(1),
            ["Γ.Ν.Α. «ΙΠΠΟΚΡΑΤΕΙΟ»", "Γ.Ν.Ν.ΙΩΝΙΑΣ"]
        );
        assert_eq!(table.rows[0].cell(2), ["Γ.Ν.Α. «ΣΩΤΗΡΙΑ»"]);

        assert_eq!(table.rows[1].label, "Καρδιολογική");
        assert!(table.rows[1].cell(1).is_empty());
        assert_eq!(table.rows[1].cell(2), ["Γ.Ν.Α. «ΑΛΕΞΑΝΔΡΑ»"]);
    }

    #[test]
    fn a_wide_entry_stays_in_its_column_instead_of_spilling_into_the_next() {
        let mut page = sample_page();
        // A name long enough to run past its own header cell.
        page.lines.push(line(
            110.0,
            vec![
                word("Γ.Ν.Α.", 200.0, 110.0),
                word("«ΚΟΡΓ.", 235.0, 110.0),
                word("ΜΠΕΝ.", 275.0, 110.0),
                word("ΕΕΣ»", 315.0, 110.0),
            ],
        ));

        let table = reconstruct(&page, &header_predicate).expect("a table");
        assert_eq!(
            table.rows[0].cell(1).last().map(String::as_str),
            Some("Γ.Ν.Α. «ΚΟΡΓ. ΜΠΕΝ. ΕΕΣ»")
        );
        assert_eq!(table.rows[0].cell(2), ["Γ.Ν.Α. «ΣΩΤΗΡΙΑ»"]);
    }

    #[test]
    fn a_page_without_a_header_is_reported_rather_than_guessed_at() {
        let page = Page {
            number: 1,
            width: 600.0,
            height: 400.0,
            lines: vec![line(50.0, vec![word("ΣΗΜΕΙΩΣΕΙΣ", 30.0, 50.0)])],
        };
        assert_eq!(reconstruct(&page, &header_predicate), None);
    }

    #[test]
    fn the_real_ministry_table_reconstructs() {
        let bytes = include_bytes!("../../tests/fixtures/moh_hospitals_20260817.pdf");
        let pages = super::super::extract(bytes).expect("pdf parses");
        let is_header = |line: &Line| line.text().starts_with("ΚΛΙΝΙΚΕΣ");
        let table = reconstruct(&pages[0], &is_header).expect("a table on page 1");

        assert_eq!(
            table
                .columns
                .iter()
                .map(|column| column.label.as_str())
                .collect::<Vec<_>>(),
            [
                "ΚΛΙΝΙΚΕΣ",
                "08:00 – 14:30",
                "08:00 – 16:00",
                "08:00 – 23:00",
                "14:30 – 08:00 επομένης",
                "08:00 – 08:00 επομένης",
                "ΠΑΡΑΤΗΡΗΣΕΙΣ",
            ]
        );

        let pathology = table
            .rows
            .iter()
            .find(|row| row.label == "Παθολογική")
            .expect("the pathology row");

        // The overnight shift, which is the column a mis-read layout would confuse.
        assert_eq!(
            pathology.cell(4),
            [
                "Γ.Ν.Α. «Γ. ΓΕΝΝΗΜΑΤΑΣ»",
                "Γ.Ν.Α. «ΣΩΤΗΡΙΑ»",
                "Γ.Ν.Α. «ΑΛΕΞΑΝΔΡΑ»"
            ]
        );
        assert!(
            pathology
                .cell(1)
                .contains(&"Γ.Ο.Ν.Κ. «ΑΓ. ΑΝΑΡΓΥΡΟΙ»".to_string())
        );
        assert!(pathology.cell(5).contains(&"Γ.Ν.Π. «ΤΖΑΝΕΙΟ»".to_string()));
    }
}

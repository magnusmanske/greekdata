//! Development aid: prints the positioned words extracted from a PDF.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: dump_pdf <file.pdf>")?;
    let bytes = std::fs::read(path)?;
    for page in greekdata::pdf::extract(&bytes)? {
        println!(
            "--- page {} ({:.0}x{:.0}) {} lines",
            page.number,
            page.width,
            page.height,
            page.lines.len()
        );
        for line in &page.lines {
            for word in &line.words {
                println!(
                    "{:8.2} {:8.2} {:7.2} {}",
                    word.x, word.y, word.width, word.text
                );
            }
        }
    }
    Ok(())
}

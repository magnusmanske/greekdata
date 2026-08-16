# Project scope
This projects aims to scrape current data, initially from Greece, from existing online data sources, normalize it, and store it in a structured format. The output is primarily an axum web server that serves the normalized data via an API. There will eventually also be a simple website to browse the data. Try to respect copyright and data usage policies.

## Information to store
A primary concept are `entities` (eg a hospital, a pharmacy, a cinema, a movie). Each entity has a `name`, `type`. Some have a `location`, `url` etc. Some have external IDs (eg IMDB, Wikidata item). Then there are properties (eg `open`/`on call`) that can have dates (day resolution) associated. THere will be specialized information associated with that property, eg. a cinema plays a film at a specific time. Older data should be kept. Start with a `sqlite` database for storage.

## Data sources
Prefer official government data sources whenever possible. Use primary sources where available. Use secondary sources only if the primary source is not available.

## Data groups
- List of open pharmacies in Greece, especially in the night/on weekends and public holidays.
- List of hospitals on call in Greece, especially in the night/on weekends and public holidays.

## Hard rules
- **Do not mention AI agents in commit messages.**
- **`git commit` is pre-authorized; `git push` is not.** Use sensible commit groupings for larger changes.
- ** Keep security in mind**, especially if this is a web-facing product.
- Always keep **code readability and long-term maintenance** in mind. Humans work on this code as well, so keep code and comments succinct and clear.
- **Keep the code simple** and elegant.
- **Aim to keep code small** where possible.
- Adhere to **SOLID and DRY principles**.
- Use **best practices** and **language standards**.
- **Take advantage of the Rust language**. "Parse, don't validate" etc.
- **No unsafe code**. Only use safe Rust.
- **Write code that does not panic**. Avoid panics in production code. Return proper `Result` errors instead.
- **Write tests** where it makes sense. Aim to keep test coverage high and tests simple.
- **Warn me if you think what I ask of you is a bad idea.** Or just window dressing, with no improvement of functionality, UX, or code readability. Be honest.
- **Fix clippy warnings**, even pre-existing ones.

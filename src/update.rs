//! One pass of the updater, and the schedule that repeats it.
//!
//! The same pass backs both `greekdata ingest` and the background updater the server
//! runs, so a scheduled refresh cannot drift from what the command line does.

use crate::{
    Result,
    cache::CachePolicy,
    config::Config,
    db::Db,
    locate::{self, LocateReport},
    sources::{self, Ctx, DateWindow, IngestReport, Source, WhenUnchanged},
};
use std::time::Duration;

/// How far back a scheduled run looks.
///
/// Yesterday, not today: an overnight shift that began yesterday is still running this
/// morning, and a correction to yesterday's rota can arrive after midnight.
const BEHIND_DAYS: i64 = 1;

/// How far ahead a scheduled run looks. The pharmacy roster is published weeks in
/// advance, the hospital one only days, so a week covers both without reaching for
/// documents that do not exist yet.
const AHEAD_DAYS: i64 = 7;

/// What one pass did.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub sources: Vec<(&'static str, IngestReport)>,
    /// Present when new records made it worth looking for coordinates.
    pub located: Option<LocateReport>,
}

impl Summary {
    /// Documents that were actually parsed, as opposed to skipped as unchanged.
    pub fn documents(&self) -> usize {
        self.sources
            .iter()
            .map(|(_, report)| report.documents)
            .sum()
    }

    pub fn skipped(&self) -> usize {
        self.sources.iter().map(|(_, report)| report.skipped).sum()
    }

    pub fn failed(&self) -> usize {
        self.sources.iter().map(|(_, report)| report.failed).sum()
    }
}

/// The window a scheduled run covers.
pub fn rolling_window() -> DateWindow {
    let today = sources::today();
    let from = today
        .checked_sub(jiff::Span::new().days(BEHIND_DAYS))
        .unwrap_or(today);
    let to = today
        .checked_add(jiff::Span::new().days(AHEAD_DAYS))
        .unwrap_or(today);

    DateWindow::new(from, to)
}

/// Ingests every given source over `window`, then places anything new on the map.
pub async fn run_once(
    ctx: &Ctx,
    selected: &[Box<dyn Source>],
    window: DateWindow,
    unchanged: WhenUnchanged,
) -> Result<Summary> {
    let mut summary = Summary::default();

    for source in selected {
        tracing::info!(
            source = source.id(),
            from = %window.from,
            to = %window.to,
            "ingesting"
        );
        let report = sources::ingest(ctx, source.as_ref(), window, unchanged).await?;
        summary.sources.push((source.id(), report));
    }

    // Looking for coordinates is only worth it when something new arrived; when every
    // document was unchanged there can be no new place to put on the map.
    if summary.documents() > 0 {
        summary.located = Some(locate::hospitals(ctx).await?);
    }

    Ok(summary)
}

/// Runs the updater on a loop, for as long as the server is up.
///
/// A pass that fails is logged and the next one still happens: a source being briefly
/// unreachable must not take the updater down with it, and must never take the server
/// down. Passes never overlap, because this is one task running them in turn.
pub async fn run_forever(config: Config, db: Db, interval: Duration) {
    let ctx = match Ctx::with_db(&config, CachePolicy::PreferCache, db) {
        Ok(ctx) => ctx,
        Err(error) => {
            tracing::error!(%error, "background updater could not start");
            return;
        }
    };

    let mut ticker = tokio::time::interval(interval);
    // A pass that overruns its slot delays the next one rather than firing immediately
    // afterwards, so a slow source cannot turn into a burst of requests.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // The first tick completes at once, so a freshly started server refreshes
        // rather than serving whatever was last written until the interval elapses.
        ticker.tick().await;

        let window = rolling_window();
        match run_once(&ctx, &sources::all(), window, WhenUnchanged::Skip).await {
            Ok(summary) => tracing::info!(
                documents = summary.documents(),
                unchanged = summary.skipped(),
                failed = summary.failed(),
                located = summary.located.map_or(0, |report| report.located),
                next_in_minutes = interval.as_secs() / 60,
                "update finished"
            ),
            Err(error) => tracing::error!(%error, "update failed; will try again"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rolling_window_covers_yesterday_and_the_week_ahead() {
        let window = rolling_window();
        let today = sources::today();

        assert!(window.contains(today));
        assert!(
            window.contains(
                today
                    .checked_sub(jiff::Span::new().days(1))
                    .expect("yesterday")
            )
        );
        assert!(
            window.contains(
                today
                    .checked_add(jiff::Span::new().days(7))
                    .expect("a week")
            )
        );
        // and no further, so a run does not reach for documents that cannot exist.
        assert!(!window.contains(today.checked_add(jiff::Span::new().days(8)).expect("later")));
        assert!(
            !window.contains(
                today
                    .checked_sub(jiff::Span::new().days(2))
                    .expect("earlier")
            )
        );
    }

    #[test]
    fn a_summary_adds_up_what_every_source_did() {
        let summary = Summary {
            sources: vec![
                (
                    "a",
                    IngestReport {
                        documents: 2,
                        skipped: 5,
                        failed: 1,
                        ..IngestReport::default()
                    },
                ),
                (
                    "b",
                    IngestReport {
                        documents: 3,
                        skipped: 1,
                        ..IngestReport::default()
                    },
                ),
            ],
            located: None,
        };

        assert_eq!(
            (summary.documents(), summary.skipped(), summary.failed()),
            (5, 6, 1)
        );
    }
}

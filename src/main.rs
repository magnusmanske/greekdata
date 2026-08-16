#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use greekdata::{
    Result,
    cache::CachePolicy,
    config::Config,
    db::{Db, query},
    server,
    sources::{self, Ctx, DateWindow},
};
use jiff::civil::Date;

#[derive(Parser)]
#[command(
    name = "greekdata",
    about = "Collects, normalizes and serves Greek public-interest data",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Never touch the network; work only from the on-disk cache.
    #[arg(long, global = true)]
    offline: bool,

    /// Re-download every document, ignoring the cache.
    #[arg(long, global = true, conflicts_with = "offline")]
    refresh: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch and store data for a range of dates.
    ///
    /// Defaults to today only. Pass an earlier `--from` to backfill history; be aware
    /// that a long range means a lot of requests to someone else's server.
    Ingest {
        /// Source slug. Omit to run every registered source.
        #[arg(long)]
        source: Option<String>,
        /// First date to ingest (YYYY-MM-DD). Defaults to today in Athens.
        #[arg(long)]
        from: Option<Date>,
        /// Last date to ingest (YYYY-MM-DD). Defaults to `--from`.
        #[arg(long)]
        to: Option<Date>,
        /// Shorthand for a window of this many days ending at `--to`.
        #[arg(long, conflicts_with = "from")]
        days: Option<u16>,
    },

    /// Serve the read-only HTTP API.
    Serve,

    /// Show data problems recorded during ingest.
    Report {
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },

    /// List the registered sources and who publishes them.
    Sources,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GREEKDATA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let policy = match (cli.offline, cli.refresh) {
        (true, _) => CachePolicy::CacheOnly,
        (_, true) => CachePolicy::Force,
        _ => CachePolicy::PreferCache,
    };

    match cli.command {
        Command::Ingest {
            source,
            from,
            to,
            days,
        } => {
            let ctx = Ctx::open(&config, policy).await?;
            ingest(&ctx, source.as_deref(), window(from, to, days)).await
        }
        Command::Serve => {
            let db = Db::open(&config.database_url).await?;
            server::serve(config, db).await
        }
        Command::Report { source, limit } => {
            let db = Db::open(&config.database_url).await?;
            report(&db, source.as_deref(), limit).await
        }
        Command::Sources => {
            list_sources();
            Ok(())
        }
    }
}

/// Works out the date range to ingest from the flags given.
fn window(from: Option<Date>, to: Option<Date>, days: Option<u16>) -> DateWindow {
    let today = sources::today();
    let end = to.unwrap_or(today);

    let start = match (from, days) {
        (Some(from), _) => from,
        (None, Some(days)) => {
            let span = jiff::Span::new().days(i64::from(days.saturating_sub(1)));
            end.checked_sub(span).unwrap_or(end)
        }
        (None, None) => end,
    };

    DateWindow::new(start, end)
}

async fn ingest(ctx: &Ctx, source_id: Option<&str>, window: DateWindow) -> Result<()> {
    let selected = match source_id {
        Some(id) => vec![sources::by_id(id)?],
        None => sources::all(),
    };

    for source in selected {
        tracing::info!(
            source = source.id(),
            from = %window.from,
            to = %window.to,
            "ingesting"
        );
        let report = sources::ingest(ctx, source.as_ref(), window).await?;
        println!(
            "{}: {} documents, {} entities, {} properties, {} warnings, {} failed",
            source.id(),
            report.documents,
            report.entities,
            report.properties,
            report.warnings,
            report.failed
        );
    }

    Ok(())
}

async fn report(db: &Db, source_id: Option<&str>, limit: i64) -> Result<()> {
    let issues = query::issues(db, source_id, limit).await?;
    if issues.is_empty() {
        println!("No issues recorded.");
        return Ok(());
    }

    for issue in &issues {
        println!(
            "{:<8} {:<24} {:<20} {}",
            issue.severity, issue.source_id, issue.code, issue.detail
        );
    }
    println!("\n{} issue(s) shown.", issues.len());

    Ok(())
}

fn list_sources() {
    for source in sources::all() {
        let attribution = source.attribution();
        println!(
            "{:<24} {:<12} {}",
            source.id(),
            source.group(),
            attribution.publisher
        );
        println!("{:<24} {:<12} {}", "", "", attribution.homepage);
    }
}

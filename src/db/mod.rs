//! SQLite storage: connection setup, schema migration, and the read/write query sets.

pub mod ingest;
pub mod query;

use crate::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

/// A pooled handle to the database, with the schema already migrated.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Opens (creating if needed) the database at `url` and applies pending migrations.
    pub async fn open(url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool })
    }

    /// An in-memory database, for tests.
    pub async fn open_in_memory() -> Result<Self> {
        Self::open("sqlite::memory:").await
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_to_a_fresh_database() {
        let db = Db::open_in_memory().await.expect("open");
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(db.pool())
                .await
                .expect("list tables");
        let names: Vec<&str> = tables.iter().map(|(name,)| name.as_str()).collect();

        for expected in [
            "entity",
            "entity_alias",
            "property",
            "snapshot",
            "ingest_issue",
        ] {
            assert!(names.contains(&expected), "missing table {expected}");
        }
    }
}

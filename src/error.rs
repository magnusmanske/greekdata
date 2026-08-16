use std::path::PathBuf;

/// Every fallible operation in the crate returns this error; library code never panics.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("request to {url} failed")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("{url} returned HTTP {status}")]
    HttpStatus { url: String, status: u16 },

    #[error("{url} returned {size} bytes, over the {limit} byte limit")]
    ResponseTooLarge {
        url: String,
        size: usize,
        limit: usize,
    },

    #[error("no cached copy of {0} and fetching is disabled")]
    CacheMiss(String),

    #[error("could not parse {context}: {message}")]
    Parse { context: String, message: String },

    #[error("unknown source `{0}`")]
    UnknownSource(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Convenience constructor for the very common parse-failure case.
    pub fn parse(context: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Parse {
            context: context.into(),
            message: message.into(),
        }
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

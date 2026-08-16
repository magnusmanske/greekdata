use crate::{Error, Result};
use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

/// Runtime configuration, read from the environment with sensible defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// SQLite connection string, e.g. `sqlite://greekdata.db`.
    pub database_url: String,
    /// Directory holding cached copies of fetched source documents.
    pub cache_dir: PathBuf,
    /// Identifies the crawler to the sites we fetch from.
    pub user_agent: String,
    /// Minimum gap between two network requests, so we stay a polite guest.
    pub request_delay: Duration,
    /// Address the API server binds to.
    pub bind: SocketAddr,
    /// Origins allowed to call the API from a browser. Empty means same-origin only.
    pub cors_origins: Vec<String>,
    /// How often the running server refreshes its data in the background.
    /// `None` leaves updating to the command line.
    pub update_interval: Option<Duration>,
}

const DEFAULT_USER_AGENT: &str = concat!(
    "greekdata/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/magnusmanske/greekdata)"
);

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: "sqlite://greekdata.db".into(),
            cache_dir: PathBuf::from("cache"),
            user_agent: DEFAULT_USER_AGENT.into(),
            request_delay: Duration::from_millis(1500),
            bind: SocketAddr::from(([127, 0, 0, 1], 3000)),
            cors_origins: Vec::new(),
            update_interval: Some(Duration::from_secs(3 * 60 * 60)),
        }
    }
}

impl Config {
    /// Overlays `GREEKDATA_*` environment variables onto the defaults.
    pub fn from_env() -> Result<Self> {
        let mut config = Self::default();

        if let Ok(url) = env::var("GREEKDATA_DATABASE_URL") {
            config.database_url = url;
        }
        if let Ok(dir) = env::var("GREEKDATA_CACHE_DIR") {
            config.cache_dir = PathBuf::from(dir);
        }
        if let Ok(agent) = env::var("GREEKDATA_USER_AGENT") {
            config.user_agent = agent;
        }
        if let Ok(ms) = env::var("GREEKDATA_REQUEST_DELAY_MS") {
            let ms = ms.parse().map_err(|_| {
                Error::Config(format!("GREEKDATA_REQUEST_DELAY_MS is not a number: {ms}"))
            })?;
            config.request_delay = Duration::from_millis(ms);
        }
        if let Ok(bind) = env::var("GREEKDATA_BIND") {
            config.bind = bind
                .parse()
                .map_err(|_| Error::Config(format!("GREEKDATA_BIND is not an address: {bind}")))?;
        }
        if let Ok(origins) = env::var("GREEKDATA_CORS_ORIGINS") {
            config.cors_origins = origins
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(String::from)
                .collect();
        }

        if let Ok(minutes) = env::var("GREEKDATA_UPDATE_INTERVAL_MINUTES") {
            let minutes: u64 = minutes.parse().map_err(|_| {
                Error::Config(format!(
                    "GREEKDATA_UPDATE_INTERVAL_MINUTES is not a number: {minutes}"
                ))
            })?;
            config.update_interval = update_interval(minutes);
        }

        Ok(config)
    }
}

/// Zero minutes turns background updating off; anything else is a period.
pub fn update_interval(minutes: u64) -> Option<Duration> {
    (minutes > 0).then(|| Duration::from_secs(minutes * 60))
}

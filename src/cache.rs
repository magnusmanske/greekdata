//! Fetches source documents through an on-disk cache.
//!
//! Every document we download is kept, so re-running an ingest, running the test suite,
//! or working offline never re-hits an upstream server. Cache entries are keyed by the
//! full request (method, URL and body), so the POST-per-date sources cache correctly too.

use crate::{Error, Result, config::Config};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// Refuse absurdly large responses rather than buffering them into memory.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// How hard to try to avoid the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CachePolicy {
    /// Use any cached copy, however old. The default: source documents for a past date
    /// do not change, and reissues arrive under a new URL.
    #[default]
    PreferCache,
    /// Ask the server whether the cached copy is still current (GET only).
    Revalidate,
    /// Always download afresh.
    Force,
    /// Never touch the network; a cache miss is an error. Used by tests.
    CacheOnly,
}

impl CachePolicy {
    /// How keen this policy is to reach the network.
    fn eagerness(self) -> u8 {
        match self {
            Self::CacheOnly => 0,
            Self::PreferCache => 1,
            Self::Revalidate => 2,
            Self::Force => 3,
        }
    }

    /// Combines the configured policy with a per-document hint.
    ///
    /// `CacheOnly` always wins: when the caller has asked to stay offline, no individual
    /// document may override that. Otherwise the more eager of the two applies, so a
    /// source can insist on re-fetching something it knows may have changed.
    pub fn combine(self, hint: Self) -> Self {
        if self == Self::CacheOnly {
            return Self::CacheOnly;
        }
        if hint.eagerness() > self.eagerness() {
            hint
        } else {
            self
        }
    }
}

/// A fetched document, from the network or from disk.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: String,
    pub body: Vec<u8>,
    pub sha256: String,
    pub fetched_at: Timestamp,
    pub from_cache: bool,
}

impl Fetched {
    /// The body as text, replacing any invalid UTF-8 rather than failing: some of these
    /// documents are hand-assembled and occasionally contain stray bytes.
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }
}

/// What we remember about a cached response, stored beside the body.
#[derive(Debug, Serialize, Deserialize)]
struct CacheMeta {
    url: String,
    method: String,
    sha256: String,
    fetched_at: Timestamp,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
}

/// A polite, caching HTTP client.
pub struct Fetcher {
    client: reqwest::Client,
    root: PathBuf,
    policy: CachePolicy,
    delay: Duration,
    last_request: Mutex<Option<Instant>>,
}

impl Fetcher {
    pub fn new(config: &Config, policy: CachePolicy) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|source| Error::Http {
                url: "<client>".into(),
                source,
            })?;

        Ok(Self {
            client,
            root: config.cache_dir.clone(),
            policy,
            delay: config.request_delay,
            last_request: Mutex::new(None),
        })
    }

    pub fn policy(&self) -> CachePolicy {
        self.policy
    }

    /// Fetches `url`, caching under the `namespace` subdirectory (normally a source id).
    pub async fn get(&self, namespace: &str, url: &str) -> Result<Fetched> {
        self.get_with(namespace, url, CachePolicy::PreferCache)
            .await
    }

    /// As [`Fetcher::get`], but with a per-document policy hint.
    pub async fn get_with(&self, namespace: &str, url: &str, hint: CachePolicy) -> Result<Fetched> {
        self.fetch(namespace, "GET", url, None, hint).await
    }

    /// Stores `body` in the cache as though it had been fetched from `url`.
    ///
    /// For feeding in a document obtained some other way — a file downloaded by hand,
    /// or a fixture a test wants the pipeline to read.
    pub async fn seed(&self, namespace: &str, url: &str, body: &[u8]) -> Result<()> {
        let key = cache_key("GET", url, None);
        let dir = self.root.join(sanitize_namespace(namespace));
        let meta = CacheMeta {
            url: url.to_string(),
            method: "GET".to_string(),
            sha256: hex_digest(body),
            fetched_at: Timestamp::now(),
            etag: None,
            last_modified: None,
            content_type: None,
        };

        write_cache(
            &dir,
            &dir.join(format!("{key}.bin")),
            &dir.join(format!("{key}.json")),
            body,
            &meta,
        )
        .await
    }

    /// Submits a form via POST. The body is part of the cache key.
    pub async fn post_form(
        &self,
        namespace: &str,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<Fetched> {
        self.post_form_with(namespace, url, form, CachePolicy::PreferCache)
            .await
    }

    /// As [`Fetcher::post_form`], but with a per-document policy hint.
    pub async fn post_form_with(
        &self,
        namespace: &str,
        url: &str,
        form: &[(&str, &str)],
        hint: CachePolicy,
    ) -> Result<Fetched> {
        let body = encode_form(form);
        self.fetch(namespace, "POST", url, Some(body), hint).await
    }

    async fn fetch(
        &self,
        namespace: &str,
        method: &str,
        url: &str,
        body: Option<String>,
        hint: CachePolicy,
    ) -> Result<Fetched> {
        let policy = self.policy.combine(hint);
        let key = cache_key(method, url, body.as_deref());
        let dir = self.root.join(sanitize_namespace(namespace));
        let body_path = dir.join(format!("{key}.bin"));
        let meta_path = dir.join(format!("{key}.json"));

        let cached = read_cache(&body_path, &meta_path).await?;

        // A conditional request only makes sense for a cacheable GET.
        let can_revalidate = method == "GET" && cached.is_some();
        match (policy, &cached) {
            (CachePolicy::PreferCache, Some(hit)) => return Ok(hit.clone()),
            (CachePolicy::CacheOnly, Some(hit)) => return Ok(hit.clone()),
            (CachePolicy::CacheOnly, None) => return Err(Error::CacheMiss(url.to_string())),
            // A caller asking to revalidate something that cannot be revalidated — a
            // form submission — wants current data, so re-send it rather than quietly
            // handing back a copy that may be out of date.
            _ => {}
        }

        self.wait_turn().await;

        let mut request = match method {
            "POST" => self
                .client
                .post(url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(body.clone().unwrap_or_default()),
            _ => self.client.get(url),
        };

        let meta = read_meta(&meta_path).await?;
        if policy == CachePolicy::Revalidate
            && can_revalidate
            && let Some(meta) = &meta
        {
            if let Some(etag) = &meta.etag {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            if let Some(last_modified) = &meta.last_modified {
                request = request.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
            }
        }

        let response = request.send().await.map_err(|source| Error::Http {
            url: url.to_string(),
            source,
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(hit) = cached
        {
            tracing::debug!(url, "cache entry revalidated");
            return Ok(hit);
        }

        if !response.status().is_success() {
            return Err(Error::HttpStatus {
                url: url.to_string(),
                status: response.status().as_u16(),
            });
        }

        if let Some(length) = response.content_length()
            && length > MAX_RESPONSE_BYTES as u64
        {
            return Err(Error::ResponseTooLarge {
                url: url.to_string(),
                size: length as usize,
                limit: MAX_RESPONSE_BYTES,
            });
        }

        let headers = response.headers().clone();
        let bytes = response.bytes().await.map_err(|source| Error::Http {
            url: url.to_string(),
            source,
        })?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(Error::ResponseTooLarge {
                url: url.to_string(),
                size: bytes.len(),
                limit: MAX_RESPONSE_BYTES,
            });
        }

        let fetched = Fetched {
            url: url.to_string(),
            sha256: hex_digest(&bytes),
            body: bytes.to_vec(),
            fetched_at: Timestamp::now(),
            from_cache: false,
        };

        let meta = CacheMeta {
            url: url.to_string(),
            method: method.to_string(),
            sha256: fetched.sha256.clone(),
            fetched_at: fetched.fetched_at,
            etag: header_string(&headers, reqwest::header::ETAG),
            last_modified: header_string(&headers, reqwest::header::LAST_MODIFIED),
            content_type: header_string(&headers, reqwest::header::CONTENT_TYPE),
        };
        write_cache(&dir, &body_path, &meta_path, &fetched.body, &meta).await?;

        Ok(fetched)
    }

    /// Sleeps as long as needed to keep at least `delay` between network requests.
    async fn wait_turn(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.delay {
                tokio::time::sleep(self.delay - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(String::from)
}

async fn read_meta(meta_path: &Path) -> Result<Option<CacheMeta>> {
    match tokio::fs::read(meta_path).await {
        Ok(raw) => Ok(serde_json::from_slice(&raw).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::io(meta_path, source)),
    }
}

async fn read_cache(body_path: &Path, meta_path: &Path) -> Result<Option<Fetched>> {
    let Some(meta) = read_meta(meta_path).await? else {
        return Ok(None);
    };
    let body = match tokio::fs::read(body_path).await {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::io(body_path, source)),
    };

    Ok(Some(Fetched {
        url: meta.url,
        sha256: meta.sha256,
        body,
        fetched_at: meta.fetched_at,
        from_cache: true,
    }))
}

/// Writes body and metadata via temporary files, so an interrupted run cannot leave a
/// truncated entry that a later run would trust.
async fn write_cache(
    dir: &Path,
    body_path: &Path,
    meta_path: &Path,
    body: &[u8],
    meta: &CacheMeta,
) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|source| Error::io(dir, source))?;

    write_atomic(body_path, body).await?;
    write_atomic(meta_path, &serde_json::to_vec_pretty(meta)?).await
}

async fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    tokio::fs::write(&temp, contents)
        .await
        .map_err(|source| Error::io(&temp, source))?;
    tokio::fs::rename(&temp, path)
        .await
        .map_err(|source| Error::io(path, source))
}

fn cache_key(method: &str, url: &str, body: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(b"\n");
    hasher.update(url.as_bytes());
    hasher.update(b"\n");
    hasher.update(body.unwrap_or_default().as_bytes());
    hex(&hasher.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Keeps a source id from escaping the cache directory via `..` or a path separator.
fn sanitize_namespace(namespace: &str) -> String {
    namespace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Minimal `application/x-www-form-urlencoded` encoder, so the crate does not gain a
/// dependency for the handful of short form fields these sources need.
pub fn encode_form(form: &[(&str, &str)]) -> String {
    form.iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_separates_get_from_post_and_body_from_body() {
        let get = cache_key("GET", "https://example.org/", None);
        let post = cache_key("POST", "https://example.org/", None);
        let post_a = cache_key("POST", "https://example.org/", Some("Date=2026-08-17"));
        let post_b = cache_key("POST", "https://example.org/", Some("Date=2026-08-18"));

        assert_ne!(get, post);
        assert_ne!(post, post_a);
        assert_ne!(post_a, post_b);
    }

    #[test]
    fn namespace_cannot_escape_the_cache_directory() {
        assert_eq!(sanitize_namespace("../../etc"), "______etc");
        assert_eq!(sanitize_namespace("fsa-attica_1"), "fsa-attica_1");
        assert!(!sanitize_namespace("a/../b").contains(['/', '.']));
    }

    #[test]
    fn offline_mode_cannot_be_overridden_by_a_document_hint() {
        assert_eq!(
            CachePolicy::CacheOnly.combine(CachePolicy::Force),
            CachePolicy::CacheOnly
        );
    }

    #[test]
    fn a_document_hint_can_only_make_fetching_more_eager() {
        assert_eq!(
            CachePolicy::PreferCache.combine(CachePolicy::Force),
            CachePolicy::Force
        );
        assert_eq!(
            CachePolicy::Force.combine(CachePolicy::PreferCache),
            CachePolicy::Force
        );
    }

    #[test]
    fn form_encoding_escapes_greek_and_separators() {
        let encoded = encode_form(&[("Date", "2026-08-17"), ("q", "α β&γ")]);
        assert_eq!(encoded, "Date=2026-08-17&q=%CE%B1+%CE%B2%26%CE%B3");
    }

    #[tokio::test]
    async fn cache_only_policy_reports_a_miss_instead_of_fetching() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            cache_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        let fetcher = Fetcher::new(&config, CachePolicy::CacheOnly).expect("fetcher");

        let error = fetcher
            .get("test", "https://example.org/never-fetched")
            .await
            .expect_err("should miss");
        assert!(matches!(error, Error::CacheMiss(_)));
    }

    #[tokio::test]
    async fn a_written_entry_is_read_back_without_the_network() {
        let dir = tempfile::tempdir().expect("temp dir");
        let namespace = dir.path().join("test");
        let key = cache_key("GET", "https://example.org/doc", None);
        let body_path = namespace.join(format!("{key}.bin"));
        let meta_path = namespace.join(format!("{key}.json"));
        let meta = CacheMeta {
            url: "https://example.org/doc".into(),
            method: "GET".into(),
            sha256: hex_digest(b"hello"),
            fetched_at: Timestamp::now(),
            etag: None,
            last_modified: None,
            content_type: None,
        };
        write_cache(&namespace, &body_path, &meta_path, b"hello", &meta)
            .await
            .expect("write cache");

        let config = Config {
            cache_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        let fetcher = Fetcher::new(&config, CachePolicy::CacheOnly).expect("fetcher");
        let hit = fetcher
            .get("test", "https://example.org/doc")
            .await
            .expect("cache hit");

        assert!(hit.from_cache);
        assert_eq!(hit.text(), "hello");
    }
}

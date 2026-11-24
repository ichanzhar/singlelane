//! HTTP transport for the GitHub REST API on reqwest (rustls with the ring provider):
//! authentication header per request, API version pinning, retries with backoff,
//! `Retry-After`, request-id logging and `Link` pagination.
//!
//! singlelane manages tokens itself (it needs the raw installation token for git and `gh`
//! too), so only a plain HTTP client is needed. Retries back off, retry a failed POST only on
//! rate limits (a POST that failed with 5xx may have been applied), and give up on waits longer
//! than [`RetryPolicy::max_delay`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, Url};

use crate::error::{GithubError, Result, clean_message};
use crate::secret::Secret;

/// The public GitHub API.
pub const GITHUB_API: &str = "https://api.github.com";

/// The REST API version sent with every request (`X-GitHub-Api-Version`). The latest version
/// GitHub lists; see SPEC 5.1 before changing it.
pub const API_VERSION: &str = "2026-03-10";

/// `User-Agent` (GitHub rejects requests without one).
const USER_AGENT: &str = concat!("singlelane/", env!("CARGO_PKG_VERSION"));

/// Retry behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub max_attempts: u32,
    /// First backoff for 5xx and transport errors; doubled on every retry.
    pub base_delay: Duration,
    /// Longest wait the client accepts. A rate limit that asks for longer is returned as an
    /// error instead (classified `network`, the item goes back to its queue).
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(90),
        }
    }
}

/// Waits between retries. The real one sleeps; tests record the delays instead.
#[async_trait]
pub trait Sleeper: Send + Sync {
    /// Waits for `d`.
    async fn sleep(&self, d: Duration);
}

/// [`Sleeper`] backed by `tokio::time::sleep`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// A completed request with a success status.
#[derive(Debug)]
pub struct Response {
    /// HTTP status.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderMap,
    /// Body text.
    pub body: String,
}

impl Response {
    /// Decodes the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self, method: &Method, path: &str) -> Result<T> {
        serde_json::from_str(&self.body).map_err(|e| GithubError::Decode {
            method: method.to_string(),
            path: path.to_string(),
            message: clean_message(&e.to_string()),
        })
    }

    /// The `rel="next"` URL of the `Link` header, if any.
    pub fn next_link(&self) -> Option<String> {
        next_link(&self.headers)
    }
}

/// The HTTP client. Cheap to clone.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    base: Url,
    retry: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
    clock: Arc<dyn singlelane_core::schedule::Clock>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("base", &self.base)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl Http {
    /// A client for `base_url` (normally [`GITHUB_API`]; a mock server in tests).
    pub fn new(
        base_url: &str,
        retry: RetryPolicy,
        clock: Arc<dyn singlelane_core::schedule::Clock>,
    ) -> Result<Self> {
        let invalid = |m: String| GithubError::Transport {
            method: "-".into(),
            path: base_url.to_string(),
            message: m,
        };
        let base = Url::parse(base_url.trim_end_matches('/'))
            .map_err(|e| invalid(format!("invalid base URL: {e}")))?;
        if !matches!(base.scheme(), "https" | "http") || base.host_str().is_none() {
            return Err(invalid(
                "base URL needs an http(s) scheme and a host".into(),
            ));
        }
        // rustls with ring (no aws-lc). reqwest takes the process-wide default provider; an
        // error only means one is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            // reqwest drops `Authorization` when a redirect leaves the host.
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| invalid(clean_message(&e.to_string())))?;
        Ok(Self {
            client,
            base,
            retry,
            sleeper: Arc::new(TokioSleeper),
            clock,
        })
    }

    /// Replaces the sleeper (tests).
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// Sends one request with retries. `target` is a path (`/repos/..`) or an absolute URL on
    /// the same host as the base URL (a pagination link). `auth` is the bearer credential
    /// (JWT or installation token). Non-success statuses become [`GithubError::Status`].
    pub async fn request(
        &self,
        method: Method,
        target: &str,
        auth: &Secret,
        body: Option<&serde_json::Value>,
    ) -> Result<Response> {
        let (url, path) = self.resolve(&method, target)?;
        let mut attempt = 1u32;
        loop {
            let outcome = self
                .send_once(&method, &url, &path, auth, body, attempt)
                .await;
            let delay = match &outcome {
                Ok(Attempt::Done(_)) => None,
                Ok(Attempt::Failed { delay, .. }) => *delay,
                Err(_) if idempotent(&method) => Some(backoff(self.retry.base_delay, attempt)),
                Err(_) => None,
            };
            let retry =
                delay.filter(|d| attempt < self.retry.max_attempts && *d <= self.retry.max_delay);
            match (outcome, retry) {
                (Ok(Attempt::Done(resp)), _) => return Ok(resp),
                (Ok(Attempt::Failed { error, .. }), None) | (Err(error), None) => {
                    return Err(error);
                }
                (Ok(Attempt::Failed { error, .. }), Some(d)) | (Err(error), Some(d)) => {
                    tracing::warn!(
                        method = %method,
                        path = %path,
                        attempt,
                        delay_ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
                        error = %error,
                        "GitHub request failed, retrying"
                    );
                    self.sleeper.sleep(d).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Resolves `target` against the base URL. Absolute URLs must point at the base host, so
    /// a pagination link can never send the token anywhere else. Returns the URL and the path
    /// with query, for logs and errors.
    fn resolve(&self, method: &Method, target: &str) -> Result<(Url, String)> {
        let refuse = |m: &str| GithubError::Refused(format!("{method} {target}: {m}"));
        let url = if target.starts_with('/') {
            let prefix = self.base.path().trim_end_matches('/');
            let mut joined = self.base.clone();
            let (path, query) = target
                .split_once('?')
                .map_or((target, None), |(p, q)| (p, Some(q)));
            joined.set_path(&format!("{prefix}{path}"));
            joined.set_query(query);
            joined
        } else {
            let url = Url::parse(target).map_err(|_| refuse("not a valid URL"))?;
            if url.scheme() != self.base.scheme()
                || url.host_str() != self.base.host_str()
                || url.port_or_known_default() != self.base.port_or_known_default()
            {
                return Err(refuse("URL is not on the GitHub API host"));
            }
            url
        };
        let path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        };
        Ok((url, path))
    }

    async fn send_once(
        &self,
        method: &Method,
        url: &Url,
        path: &str,
        auth: &Secret,
        body: Option<&serde_json::Value>,
        attempt: u32,
    ) -> Result<Attempt> {
        let mut auth_value = HeaderValue::from_str(&format!("Bearer {}", auth.expose()))
            .map_err(|_| GithubError::Key("credential is not a valid header value".into()))?;
        auth_value.set_sensitive(true);
        let transport = |e: &dyn std::fmt::Display| GithubError::Transport {
            method: method.to_string(),
            path: path.to_string(),
            message: clean_message(&e.to_string()),
        };
        let mut request = self
            .client
            .request(method.clone(), url.clone())
            .header(AUTHORIZATION, auth_value);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| transport(&e.without_url()))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|e| transport(&e.without_url()))?;
        let request_id = header_str(&headers, "x-github-request-id")
            .unwrap_or("-")
            .to_string();
        tracing::debug!(method = %method, path = %path, status, request_id = %request_id, "GitHub request");

        if (200..300).contains(&status) {
            return Ok(Attempt::Done(Response {
                status,
                headers,
                body: text,
            }));
        }
        let rate_limited = is_rate_limited(status, &headers, &text);
        let error = GithubError::Status {
            method: method.to_string(),
            path: path.to_string(),
            status,
            message: clean_message(&error_message(&text)),
            request_id,
            rate_limited,
        };
        let now = self.clock.now().as_second();
        let delay = retry_delay(
            method,
            status,
            &headers,
            rate_limited,
            now,
            backoff(self.retry.base_delay, attempt),
            backoff(RATE_LIMIT_FALLBACK, attempt),
        );
        Ok(Attempt::Failed { error, delay })
    }
}

enum Attempt {
    Done(Response),
    Failed {
        error: GithubError,
        delay: Option<Duration>,
    },
}

/// First wait for a rate limit without `Retry-After` or reset headers (GitHub: "wait for at
/// least one minute").
const RATE_LIMIT_FALLBACK: Duration = Duration::from_secs(60);

/// Methods that are safe to resend after a 5xx or a broken connection. A POST that failed
/// with 5xx may have been applied (a comment or PR created twice), so it is only retried when
/// GitHub refused it with a rate limit.
fn idempotent(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::PATCH
    )
}

/// `base * 2^(attempt-1)`, capped at 64x.
fn backoff(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(1u32 << attempt.saturating_sub(1).min(6))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// Whether a response is a primary or secondary rate limit.
pub(crate) fn is_rate_limited(status: u16, headers: &HeaderMap, body: &str) -> bool {
    match status {
        429 => true,
        403 => {
            headers.contains_key("retry-after")
                || header_str(headers, "x-ratelimit-remaining") == Some("0")
                || body.to_ascii_lowercase().contains("rate limit")
        }
        _ => false,
    }
}

/// How long to wait before retrying a failed response, or `None` to give up.
///
/// - Rate limits (any method): `Retry-After` seconds; else until `x-ratelimit-reset` when
///   `x-ratelimit-remaining` is 0; else `rate_limit_backoff` (one minute, doubled on every
///   further attempt: GitHub's guidance for secondary limits).
/// - 500, 502, 503, 504 on idempotent methods: `server_backoff` (exponential, computed by
///   the caller for this attempt).
fn retry_delay(
    method: &Method,
    status: u16,
    headers: &HeaderMap,
    rate_limited: bool,
    now_unix: i64,
    server_backoff: Duration,
    rate_limit_backoff: Duration,
) -> Option<Duration> {
    if rate_limited {
        if let Some(secs) = header_str(headers, "retry-after").and_then(|v| v.parse::<u64>().ok()) {
            return Some(Duration::from_secs(secs));
        }
        if header_str(headers, "x-ratelimit-remaining") == Some("0")
            && let Some(reset) =
                header_str(headers, "x-ratelimit-reset").and_then(|v| v.parse::<i64>().ok())
        {
            let wait = u64::try_from(reset.saturating_sub(now_unix).max(0)).unwrap_or(0);
            return Some(Duration::from_secs(wait + 1));
        }
        return Some(rate_limit_backoff);
    }
    if matches!(status, 500 | 502 | 503 | 504) && idempotent(method) {
        return Some(server_backoff);
    }
    None
}

/// GitHub's `message` field from an error body, or the body itself.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| body.to_string())
}

/// The `rel="next"` target of a `Link` header.
pub(crate) fn next_link(headers: &HeaderMap) -> Option<String> {
    let link = header_str(headers, "link")?;
    link.split(',').find_map(|part| {
        let mut pieces = part.split(';');
        let url = pieces.next()?.trim();
        let is_next = pieces.any(|p| {
            let p = p.trim();
            p == "rel=\"next\"" || p == "rel=next"
        });
        let url = url.strip_prefix('<')?.strip_suffix('>')?;
        is_next.then(|| url.to_string())
    })
}

/// Percent-encodes one path segment or query value (RFC 3986 unreserved characters pass).
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn next_link_is_parsed() {
        let h = headers(&[(
            "link",
            "<https://api.github.com/repositories/1/issues?page=2>; rel=\"next\", \
             <https://api.github.com/repositories/1/issues?page=5>; rel=\"last\"",
        )]);
        assert_eq!(
            next_link(&h).as_deref(),
            Some("https://api.github.com/repositories/1/issues?page=2")
        );
        let last = headers(&[(
            "link",
            "<https://api.github.com/x?page=1>; rel=\"prev\", <https://api.github.com/x?page=1>; rel=\"first\"",
        )]);
        assert_eq!(next_link(&last), None);
        assert_eq!(next_link(&HeaderMap::new()), None);
    }

    #[test]
    fn rate_limit_detection() {
        assert!(is_rate_limited(429, &HeaderMap::new(), ""));
        assert!(is_rate_limited(403, &headers(&[("retry-after", "30")]), ""));
        assert!(is_rate_limited(
            403,
            &headers(&[("x-ratelimit-remaining", "0")]),
            ""
        ));
        assert!(is_rate_limited(
            403,
            &HeaderMap::new(),
            r#"{"message":"You have exceeded a secondary rate limit."}"#
        ));
        assert!(!is_rate_limited(
            403,
            &HeaderMap::new(),
            r#"{"message":"Resource not accessible by integration"}"#
        ));
        assert!(!is_rate_limited(401, &headers(&[("retry-after", "1")]), ""));
    }

    #[test]
    fn retry_delays() {
        let one = Duration::from_secs(1);
        let min = Duration::from_secs(60);
        let get = Method::GET;
        let post = Method::POST;
        let h = headers(&[("retry-after", "7")]);
        assert_eq!(
            retry_delay(&post, 403, &h, true, 0, one, min),
            Some(Duration::from_secs(7))
        );
        let h = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1100"),
        ]);
        assert_eq!(
            retry_delay(&get, 403, &h, true, 1000, one, min),
            Some(Duration::from_secs(101))
        );
        assert_eq!(
            retry_delay(&get, 403, &HeaderMap::new(), true, 0, one, min),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            retry_delay(&get, 429, &HeaderMap::new(), true, 0, one, backoff(min, 2)),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            retry_delay(&get, 502, &HeaderMap::new(), false, 0, one, min),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay(&post, 502, &HeaderMap::new(), false, 0, one, min),
            None
        );
        assert_eq!(
            retry_delay(&get, 404, &HeaderMap::new(), false, 0, one, min),
            None
        );
        assert_eq!(
            retry_delay(&get, 401, &HeaderMap::new(), false, 0, one, min),
            None
        );
    }

    #[test]
    fn backoff_doubles() {
        let b = Duration::from_millis(100);
        assert_eq!(backoff(b, 1), Duration::from_millis(100));
        assert_eq!(backoff(b, 2), Duration::from_millis(200));
        assert_eq!(backoff(b, 3), Duration::from_millis(400));
        assert_eq!(backoff(b, 30), Duration::from_millis(6400));
    }

    #[test]
    fn encode_segments() {
        assert_eq!(encode("agent-ready"), "agent-ready");
        assert_eq!(encode("my app[bot]"), "my%20app%5Bbot%5D");
        assert_eq!(encode("o:agent/issue-1"), "o%3Aagent%2Fissue-1");
    }

    #[test]
    fn error_message_extraction() {
        assert_eq!(
            error_message(r#"{"message":"Not Found","documentation_url":"x"}"#),
            "Not Found"
        );
        assert_eq!(error_message("plain"), "plain");
    }
}

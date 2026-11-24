//! Errors of the GitHub layer.
//!
//! Every message is built from data that cannot contain a credential (method, path, status,
//! request id) plus the GitHub error message, which is redacted and truncated before it is
//! stored (G8). Tokens and the private key are never part of an error.

use singlelane_core::policy::redact::Redactor;
use thiserror::Error;

/// Longest GitHub error message kept in an error, in characters.
const MAX_MESSAGE_CHARS: usize = 300;

/// Result alias for the GitHub layer.
pub type Result<T, E = GithubError> = std::result::Result<T, E>;

/// A GitHub layer error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GithubError {
    /// GitHub answered with a non-success status (after retries).
    #[error("GitHub {method} {path}: HTTP {status}: {message} (request id {request_id})")]
    Status {
        /// HTTP method.
        method: String,
        /// Request path (no host, no credentials).
        path: String,
        /// HTTP status.
        status: u16,
        /// GitHub's error message, redacted and truncated.
        message: String,
        /// `x-github-request-id`, or `-`.
        request_id: String,
        /// Whether GitHub signalled a primary or secondary rate limit.
        rate_limited: bool,
    },
    /// The request did not complete (DNS, connect, TLS, timeout).
    #[error("GitHub {method} {path}: {message}")]
    Transport {
        /// HTTP method.
        method: String,
        /// Request path.
        path: String,
        /// What went wrong, redacted.
        message: String,
    },
    /// The response did not have the expected shape.
    #[error("GitHub {method} {path}: unexpected response: {message}")]
    Decode {
        /// HTTP method.
        method: String,
        /// Request path.
        path: String,
        /// The decode error.
        message: String,
    },
    /// The GitHub App private key can't be used, or a JWT can't be signed.
    #[error("GitHub App key: {0}")]
    Key(String),
    /// A request the factory must never make, refused before it is sent (for example a PR
    /// from a branch outside `agent/`).
    #[error("refused: {0}")]
    Refused(String),
}

impl GithubError {
    /// The HTTP status, if GitHub answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// HTTP 404.
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    /// A primary or secondary rate limit (HTTP 429, or 403 with a rate-limit signal).
    /// SPEC 6.8 classifies these as `network`.
    pub fn is_rate_limited(&self) -> bool {
        matches!(
            self,
            Self::Status {
                rate_limited: true,
                ..
            }
        )
    }

    /// Credentials were rejected: 401, or 403 that is not a rate limit, or an unusable key.
    /// SPEC 6.8 classifies these as `github_auth`.
    pub fn is_auth_failure(&self) -> bool {
        match self {
            Self::Status {
                status,
                rate_limited,
                ..
            } => *status == 401 || (*status == 403 && !rate_limited),
            Self::Key(_) => true,
            _ => false,
        }
    }

    /// The request never got an answer (candidate for `network`, after the host probe).
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport { .. })
    }
}

/// Redacts and truncates text that came from GitHub or from a lower-level error, before it is
/// stored in a [`GithubError`].
pub(crate) fn clean_message(text: &str) -> String {
    let redacted = Redactor::new().redact(text.trim());
    let mut out: String = redacted.chars().take(MAX_MESSAGE_CHARS).collect();
    if redacted.chars().count() > MAX_MESSAGE_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16, rate_limited: bool) -> GithubError {
        GithubError::Status {
            method: "GET".into(),
            path: "/app".into(),
            status: code,
            message: "m".into(),
            request_id: "-".into(),
            rate_limited,
        }
    }

    #[test]
    fn classification_helpers() {
        assert!(status(401, false).is_auth_failure());
        assert!(status(403, false).is_auth_failure());
        assert!(!status(403, true).is_auth_failure());
        assert!(status(403, true).is_rate_limited());
        assert!(status(429, true).is_rate_limited());
        assert!(status(404, false).is_not_found());
        assert!(!status(500, false).is_auth_failure());
        assert!(GithubError::Key("x".into()).is_auth_failure());
    }

    #[test]
    fn g8_clean_message_redacts_and_truncates() {
        let token = format!("ghs_{}", "a1B2c3D4e5".repeat(4));
        let m = clean_message(&format!("Bad credentials for {token}"));
        assert!(!m.contains(&token), "{m}");
        let long = "x".repeat(1000);
        assert_eq!(clean_message(&long).chars().count(), MAX_MESSAGE_CHARS + 1);
    }
}

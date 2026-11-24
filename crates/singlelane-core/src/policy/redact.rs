//! Redaction of secrets (G8).
//!
//! Applied to every piece of externally visible text: logs, comments, PR bodies, Telegram
//! messages and error messages. Two layers:
//! - known secret values (from the env files and the current installation token), replaced
//!   wherever they appear;
//! - secret-shaped strings: GitHub tokens (`ghs_`, `ghp_`, `gho_`, `ghu_`, `ghr_`,
//!   `github_pat_`), Anthropic keys (`sk-ant-`), JWTs, PEM blocks, Telegram bot tokens,
//!   credentials embedded in URLs and `Authorization` header values.

use std::sync::LazyLock;

use regex::Regex;

/// The replacement text.
pub const REDACTED: &str = "[REDACTED]";

/// Known secrets shorter than this are not redacted by value (too many false positives);
/// such values are not valid tokens for any service singlelane talks to anyway.
pub const MIN_KNOWN_SECRET_LEN: usize = 8;

/// Number of built-in patterns; a test checks they all compile.
#[cfg(test)]
const PATTERN_COUNT: usize = 9;

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // PEM blocks, including truncated ones without an END line.
        r"(?s)-----BEGIN [A-Z0-9 ]*-----.*?(?:-----END [A-Z0-9 ]*-----|\z)",
        // GitHub tokens.
        r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}",
        r"github_pat_[A-Za-z0-9_]{20,}",
        // Anthropic API keys and OAuth tokens.
        r"sk-ant-[A-Za-z0-9_\-]{8,}",
        // JWTs: base64url header starting with {" , payload, optional signature.
        r"eyJ[A-Za-z0-9_\-]{5,}\.[A-Za-z0-9_\-]{5,}\.[A-Za-z0-9_\-]*",
        // Telegram bot tokens.
        r"\d{6,12}:AA[A-Za-z0-9_\-]{30,}",
        // Credentials in URLs: scheme://user:secret@host
        r"(?i)\b([a-z][a-z0-9+.\-]*://[^/\s:@]+:)[^/\s@]+(@)",
        // Authorization headers (also inside `git -c http.extraHeader=...`).
        r"(?i)\b(authorization:\s*(?:bearer|basic|token)\s+)[A-Za-z0-9+/=._\-]+",
        // KEY=value for the known secret env vars.
        r"(?i)\b((?:anthropic_api_key|claude_code_oauth_token|telegram_bot_token|gh_token|github_token)\s*[=:]\s*)\S+",
    ]
    .into_iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
});

/// Redacts known secret values and secret-shaped strings.
#[derive(Clone, Default)]
pub struct Redactor {
    known: Vec<String>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("known_secrets", &self.known.len())
            .finish()
    }
}

impl Redactor {
    /// A redactor with no known secrets (pattern redaction only).
    pub fn new() -> Self {
        Self::default()
    }

    /// A redactor for the given known secret values.
    pub fn with_secrets<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut r = Self::new();
        for s in secrets {
            r.add_secret(s);
        }
        r
    }

    /// Adds a known secret value (for example a freshly minted installation token).
    pub fn add_secret(&mut self, secret: impl Into<String>) {
        let s: String = secret.into();
        let s = s.trim().to_string();
        if s.chars().count() >= MIN_KNOWN_SECRET_LEN && !self.known.contains(&s) {
            self.known.push(s);
            // Longest first, so a secret that contains another is replaced whole.
            self.known.sort_by_key(|k| std::cmp::Reverse(k.len()));
        }
    }

    /// Returns `text` with every secret replaced by [`REDACTED`].
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.known {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        for re in PATTERNS.iter() {
            if re.is_match(&out) {
                out = re
                    .replace_all(&out, |caps: &regex::Captures<'_>| {
                        let group = |i| caps.get(i).map_or("", |m| m.as_str());
                        format!("{}{REDACTED}{}", group(1), group(2))
                    })
                    .into_owned();
            }
        }
        out
    }

    /// Whether `text` still contains anything that would be redacted.
    pub fn contains_secret(&self, text: &str) -> bool {
        self.redact(text) != text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake, secret-shaped values built at runtime so no scanner flags this file.
    fn fake(prefix: &str, len: usize) -> String {
        let body: String = "aB3dE5fG7hJ9kL1mN2pQ4rS6tU8vW0xY"
            .chars()
            .cycle()
            .take(len)
            .collect();
        format!("{prefix}{body}")
    }

    #[test]
    fn g8_all_patterns_compile() {
        assert_eq!(PATTERNS.len(), PATTERN_COUNT);
    }

    #[test]
    fn g8_github_tokens_are_redacted() {
        let r = Redactor::new();
        for prefix in ["ghs_", "ghp_", "gho_", "ghu_", "ghr_"] {
            let tok = fake(prefix, 36);
            let out = r.redact(&format!("token={tok} done"));
            assert!(!out.contains(&tok), "{prefix}");
            assert!(out.contains(REDACTED));
        }
        let pat = fake("github_pat_", 70);
        assert!(!r.redact(&pat).contains(&pat));
    }

    #[test]
    fn g8_anthropic_keys_are_redacted() {
        let r = Redactor::new();
        let key = fake("sk-ant-api03-", 90);
        let oauth = fake("sk-ant-oat01-", 90);
        let out = r.redact(&format!("Invalid API key {key} / {oauth}"));
        assert!(!out.contains(&key) && !out.contains(&oauth));
        assert!(out.starts_with("Invalid API key "));
    }

    #[test]
    fn g8_jwt_is_redacted() {
        let r = Redactor::new();
        let jwt = format!(
            "eyJhbGciOiJSUzI1NiJ9.{}.{}",
            fake("eyJpYXQiOj", 30),
            fake("", 40)
        );
        let out = r.redact(&format!("Bearer {jwt}"));
        assert!(!out.contains(&jwt));
    }

    #[test]
    fn g8_pem_blocks_are_redacted() {
        let r = Redactor::new();
        let pem = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n{}\n-----END RSA PRIVATE KEY-----",
            fake("MIIE", 60),
            fake("", 60)
        );
        let out = r.redact(&format!("before\n{pem}\nafter"));
        assert_eq!(out, format!("before\n{REDACTED}\nafter"));
        let truncated = format!("-----BEGIN PRIVATE KEY-----\n{}", fake("MIIE", 60));
        assert_eq!(r.redact(&truncated), REDACTED);
    }

    #[test]
    fn g8_url_credentials_and_auth_headers_are_redacted() {
        let r = Redactor::new();
        let out = r.redact("fatal: https://x-access-token:s3cr3tvalue123@github.com/o/r.git");
        assert_eq!(
            out,
            format!("fatal: https://x-access-token:{REDACTED}@github.com/o/r.git")
        );
        let out = r.redact("-c http.extraHeader=Authorization: Basic eC1hY2Nlc3MtdG9rZW46dG9r");
        assert_eq!(
            out,
            format!("-c http.extraHeader=Authorization: Basic {REDACTED}")
        );
        let out = r.redact("ANTHROPIC_API_KEY=whatever-it-is");
        assert_eq!(out, format!("ANTHROPIC_API_KEY={REDACTED}"));
    }

    #[test]
    fn g8_telegram_bot_token_is_redacted() {
        let r = Redactor::new();
        let tok = fake("123456789:AA", 33);
        assert!(
            !r.redact(&format!("url /bot{tok}/sendMessage"))
                .contains(&tok)
        );
    }

    #[test]
    fn g8_known_secrets_are_redacted_wherever_they_appear() {
        let r =
            Redactor::with_secrets(["plain-secret-value", "short", "plain-secret-value-longer"]);
        let out = r.redact("a plain-secret-value-longer b plain-secret-value c short");
        assert_eq!(out, format!("a {REDACTED} b {REDACTED} c short"));
        assert!(r.contains_secret("xx plain-secret-value"));
        assert!(!r.contains_secret("nothing here"));
    }

    #[test]
    fn g8_tokens_glued_to_other_text_are_redacted() {
        let r = Redactor::new();
        let tok = fake("ghs_", 36);
        let out = r.redact(&format!("xx{tok}yy"));
        assert!(!out.contains("ghs_"), "{out}");
        let key = fake("sk-ant-oat01-", 40);
        assert!(!r.redact(&format!("key{key}")).contains("sk-ant-"));
    }

    #[test]
    fn g8_ordinary_text_is_untouched() {
        let r = Redactor::new();
        let text = "Fixed the ghost_button in src/sk-ant.rs; see https://github.com/o/r/pull/3 \
                    and eyJ alone. Time 12:30, issue #4.";
        assert_eq!(r.redact(text), text);
    }

    #[test]
    fn g8_debug_does_not_print_secrets() {
        let r = Redactor::with_secrets(["very-secret-value"]);
        assert!(!format!("{r:?}").contains("very-secret-value"));
    }
}

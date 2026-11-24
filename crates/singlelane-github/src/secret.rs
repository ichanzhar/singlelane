//! A string that never prints itself (G8).

use std::fmt;

/// A secret value: an installation token, a JWT, or an `Authorization` header value.
///
/// `Debug` prints `Secret([REDACTED])` and there is no `Display`, so a secret can't end up in
/// a log line or error message by accident. Use [`Secret::expose`] only at the point where the
/// value is handed to an HTTP header or a child process env var.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value. Never log or format it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g8_secret_debug_is_redacted() {
        // Built at runtime so secret scanners don't flag this file.
        let raw = format!("ghs_{}", "q7W9e2R4t6".repeat(4));
        let s = Secret::new(raw.clone());
        assert_eq!(format!("{s:?}"), "Secret([REDACTED])");
        assert_eq!(s.expose(), raw);
    }
}

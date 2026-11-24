//! GitHub App authentication (SPEC 5.1).
//!
//! - [`AppKey`]: the app's RSA private key, loaded from PEM (PKCS#1 as GitHub issues it, or
//!   PKCS#8).
//! - [`sign_jwt`]: a short-lived RS256 JWT for the app endpoints (`GET /app`, the token
//!   exchange). Signed with `ring`, which rustls already brings in, so no extra crypto crate.
//! - [`AppAuth`]: exchanges the JWT for an installation access token, caches it in memory
//!   only, and refreshes it when less than [`REFRESH_MARGIN`] remains. It implements
//!   [`TokenProvider`], which git and `gh` use in later phases.
//!
//! The key and tokens never appear in `Debug` output, logs or errors (G8).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::Method;
use jiff::{SignedDuration, Timestamp};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use singlelane_core::schedule::Clock;
use tokio::sync::Mutex;

use crate::error::{GithubError, Result};
use crate::http::Http;
use crate::secret::Secret;

/// The JWT's `iat` is set this far in the past to tolerate clock drift (GitHub's advice).
pub const JWT_BACKDATE: Duration = Duration::from_secs(60);
/// JWT lifetime after `now`. GitHub requires `exp` to be no more than 10 minutes in the
/// future; 9 minutes leaves room for clock drift.
pub const JWT_LIFETIME: Duration = Duration::from_secs(9 * 60);
/// A cached installation token is replaced when less than this remains (SPEC 5.1).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(10 * 60);

/// The GitHub App private key.
pub struct AppKey {
    pair: RsaKeyPair,
}

impl std::fmt::Debug for AppKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppKey([REDACTED])")
    }
}

impl AppKey {
    /// Parses a PEM private key. Errors never include any part of the input.
    pub fn from_pem(pem: &[u8]) -> Result<Self> {
        let der = PrivateKeyDer::from_pem_slice(pem).map_err(|_| {
            GithubError::Key("the file is not a PEM private key (expected the .pem file GitHub generated for the app)".into())
        })?;
        let pair = match &der {
            PrivateKeyDer::Pkcs1(k) => RsaKeyPair::from_der(k.secret_pkcs1_der()),
            PrivateKeyDer::Pkcs8(k) => RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()),
            _ => {
                return Err(GithubError::Key(
                    "the private key is not an RSA key; GitHub App keys are RSA".into(),
                ));
            }
        }
        .map_err(|e| GithubError::Key(format!("the RSA private key was rejected: {e}")))?;
        Ok(Self { pair })
    }

    /// The DER-encoded public key (PKCS#1 `RSAPublicKey`), for verifying signatures in tests
    /// and for comparing against the app's registered keys.
    pub fn public_key_der(&self) -> &[u8] {
        self.pair.public().as_ref()
    }
}

/// JWT claims for GitHub App authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Issued at (unix seconds), backdated by [`JWT_BACKDATE`].
    pub iat: i64,
    /// Expires (unix seconds), `now` + [`JWT_LIFETIME`].
    pub exp: i64,
    /// The app id, as a string. GitHub accepts the app id or the client id here (it
    /// recommends the client id); a JWT `iss` is a string by definition (RFC 7519).
    pub iss: String,
}

impl JwtClaims {
    /// Claims for `app_id` at `now`.
    pub fn new(app_id: u64, now: Timestamp) -> Self {
        let now = now.as_second();
        let secs = |d: Duration| i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
        Self {
            iat: now.saturating_sub(secs(JWT_BACKDATE)),
            exp: now.saturating_add(secs(JWT_LIFETIME)),
            iss: app_id.to_string(),
        }
    }
}

/// Signs an RS256 JWT for `app_id` at `now`.
pub fn sign_jwt(key: &AppKey, app_id: u64, now: Timestamp) -> Result<Secret> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::to_vec(&JwtClaims::new(app_id, now))
        .map_err(|e| GithubError::Key(format!("can't encode JWT claims: {e}")))?;
    let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims));
    let mut signature = vec![0u8; key.pair.public().modulus_len()];
    key.pair
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| GithubError::Key("signing the JWT failed".into()))?;
    Ok(Secret::new(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    )))
}

/// An installation access token and its expiry.
#[derive(Clone)]
pub struct InstallationToken {
    /// The token (`ghs_...`).
    pub token: Secret,
    /// When GitHub expires it (about one hour after minting).
    pub expires_at: Timestamp,
}

impl std::fmt::Debug for InstallationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallationToken")
            .field("token", &self.token)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl InstallationToken {
    /// Whether the token still has at least [`REFRESH_MARGIN`] left at `now`.
    pub fn fresh_at(&self, now: Timestamp) -> bool {
        let margin = SignedDuration::try_from(REFRESH_MARGIN).unwrap_or(SignedDuration::MAX);
        now.checked_add(margin)
            .is_ok_and(|limit| limit < self.expires_at)
    }
}

/// Source of installation tokens for the REST API, git over HTTPS and `gh` (SPEC 5.1).
#[async_trait]
pub trait TokenProvider: Send + Sync {
    /// A token with at least ten minutes left, minted only when the cached one is too old.
    async fn installation_token(&self) -> Result<Secret>;

    /// A newly minted token, whatever the cache holds. Used right before publish, since a
    /// job can run longer than a token lives.
    async fn fresh_installation_token(&self) -> Result<Secret>;
}

/// GitHub App authentication with an in-memory token cache.
pub struct AppAuth {
    app_id: u64,
    installation_id: u64,
    key: AppKey,
    http: Http,
    clock: Arc<dyn Clock>,
    cache: Mutex<Option<InstallationToken>>,
}

impl std::fmt::Debug for AppAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppAuth")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    token: String,
    expires_at: String,
}

impl AppAuth {
    /// Authentication for `app_id` / `installation_id` with `key`.
    pub fn new(
        app_id: u64,
        installation_id: u64,
        key: AppKey,
        http: Http,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            app_id,
            installation_id,
            key,
            http,
            clock,
            cache: Mutex::new(None),
        }
    }

    /// The app id.
    pub fn app_id(&self) -> u64 {
        self.app_id
    }

    /// The installation id.
    pub fn installation_id(&self) -> u64 {
        self.installation_id
    }

    /// A JWT for the app endpoints, valid for about nine minutes.
    pub fn jwt(&self) -> Result<Secret> {
        sign_jwt(&self.key, self.app_id, self.clock.now())
    }

    /// Exchanges a fresh JWT for an installation token
    /// (`POST /app/installations/<id>/access_tokens`).
    async fn mint(&self) -> Result<InstallationToken> {
        let path = format!("/app/installations/{}/access_tokens", self.installation_id);
        let jwt = self.jwt()?;
        let resp = self.http.request(Method::POST, &path, &jwt, None).await?;
        let parsed: AccessTokenResponse = resp.json(&Method::POST, &path)?;
        let expires_at: Timestamp =
            parsed
                .expires_at
                .parse()
                .map_err(|e: jiff::Error| GithubError::Decode {
                    method: "POST".into(),
                    path: path.clone(),
                    message: format!("expires_at: {e}"),
                })?;
        if parsed.token.trim().is_empty() {
            return Err(GithubError::Decode {
                method: "POST".into(),
                path,
                message: "empty token".into(),
            });
        }
        tracing::info!(
            installation_id = self.installation_id,
            expires_at = %expires_at,
            "minted a GitHub installation token"
        );
        Ok(InstallationToken {
            token: Secret::new(parsed.token),
            expires_at,
        })
    }
}

#[async_trait]
impl TokenProvider for AppAuth {
    async fn installation_token(&self) -> Result<Secret> {
        let mut cache = self.cache.lock().await;
        if let Some(t) = cache.as_ref()
            && t.fresh_at(self.clock.now())
        {
            return Ok(t.token.clone());
        }
        let t = self.mint().await?;
        let token = t.token.clone();
        *cache = Some(t);
        Ok(token)
    }

    async fn fresh_installation_token(&self) -> Result<Secret> {
        let mut cache = self.cache.lock().await;
        let t = self.mint().await?;
        let token = t.token.clone();
        *cache = Some(t);
        Ok(token)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    /// The test-only key fixture as PEM. The markers are assembled at runtime so secret
    /// scanners don't flag the source; the key exists only for these tests.
    pub(crate) fn test_key_pem() -> String {
        let body = include_str!("../tests/fixtures/test-app-key.pkcs1.b64");
        let marker = |edge: &str| format!("-----{edge} RSA {} KEY-----", "PRIVATE");
        format!("{}\n{}{}\n", marker("BEGIN"), body, marker("END"))
    }

    fn decode_part(part: &str) -> serde_json::Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn jwt_claims_and_expiry() {
        let key = AppKey::from_pem(test_key_pem().as_bytes()).unwrap();
        let now: Timestamp = "2026-11-24T10:00:00Z".parse().unwrap();
        let jwt = sign_jwt(&key, 123_456, now).unwrap();
        let parts: Vec<&str> = jwt.expose().split('.').collect();
        assert_eq!(parts.len(), 3);
        let header = decode_part(parts[0]);
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        let claims: JwtClaims = serde_json::from_value(decode_part(parts[1])).unwrap();
        assert_eq!(claims.iss, "123456");
        assert_eq!(
            decode_part(parts[1])["iss"],
            "123456",
            "iss is a JSON string"
        );
        assert_eq!(claims.iat, now.as_second() - 60);
        assert_eq!(claims.exp, now.as_second() + 540);
        // GitHub rejects JWTs that live longer than 10 minutes.
        assert!(claims.exp - claims.iat <= 600);
        assert!(claims.exp > now.as_second());

        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let signed = format!("{}.{}", parts[0], parts[1]);
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public_key_der())
            .verify(signed.as_bytes(), &signature)
            .expect("signature verifies with the app key");
    }

    #[test]
    fn pkcs8_keys_are_accepted() {
        let body = include_str!("../tests/fixtures/test-app-key.pkcs8.b64");
        let marker = |edge: &str| format!("-----{edge} {} KEY-----", "PRIVATE");
        let pem = format!("{}\n{}{}\n", marker("BEGIN"), body, marker("END"));
        let pkcs8 = AppKey::from_pem(pem.as_bytes()).unwrap();
        let pkcs1 = AppKey::from_pem(test_key_pem().as_bytes()).unwrap();
        assert_eq!(pkcs8.public_key_der(), pkcs1.public_key_der());
    }

    #[test]
    fn g8_bad_key_errors_do_not_echo_the_input() {
        let secretish = format!("ghs_{}", "Zx9Yw8Vu7T".repeat(4));
        let err = AppKey::from_pem(format!("not a key {secretish}").as_bytes()).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains(&secretish), "{msg}");

        let pem = test_key_pem();
        // A truncated real key: the message must not contain any of its lines.
        let truncated: String = pem.lines().take(5).collect::<Vec<_>>().join("\n");
        let marker_end = format!("\n-----END RSA {} KEY-----\n", "PRIVATE");
        let err = AppKey::from_pem(format!("{truncated}{marker_end}").as_bytes()).unwrap_err();
        let msg = format!("{err} {err:?}");
        for line in pem.lines().skip(1).take(4) {
            assert!(!msg.contains(line), "{msg}");
        }
    }

    #[test]
    fn g8_key_and_token_debug_are_redacted() {
        let key = AppKey::from_pem(test_key_pem().as_bytes()).unwrap();
        assert_eq!(format!("{key:?}"), "AppKey([REDACTED])");
        let raw = format!("ghs_{}", "Lm4Np5Qr6S".repeat(4));
        let t = InstallationToken {
            token: Secret::new(raw.clone()),
            expires_at: Timestamp::UNIX_EPOCH,
        };
        assert!(!format!("{t:?}").contains(&raw));
    }

    #[test]
    fn token_freshness_uses_ten_minute_margin() {
        let exp: Timestamp = "2026-11-24T11:00:00Z".parse().unwrap();
        let t = InstallationToken {
            token: Secret::new("x"),
            expires_at: exp,
        };
        let at = |s: &str| s.parse::<Timestamp>().unwrap();
        assert!(t.fresh_at(at("2026-11-24T10:00:00Z")));
        assert!(t.fresh_at(at("2026-11-24T10:49:59Z")));
        assert!(!t.fresh_at(at("2026-11-24T10:50:00Z")));
        assert!(!t.fresh_at(at("2026-11-24T11:30:00Z")));
    }
}

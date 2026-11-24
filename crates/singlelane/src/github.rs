//! Building the real GitHub client from the config.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use jiff::Timestamp;
use singlelane_core::config::Config;
use singlelane_core::policy::redact::Redactor;
use singlelane_core::schedule::Clock;
use singlelane_github::http::{GITHUB_API, Http, RetryPolicy};
use singlelane_github::{AppAuth, AppKey, OctoGitHub};

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// The GitHub client and its token provider.
#[derive(Debug)]
pub struct Client {
    /// The REST client.
    pub gh: OctoGitHub,
    /// Installation tokens (also used by `doctor`).
    pub auth: Arc<AppAuth>,
}

/// Loads the private key. The error never contains the key material.
pub fn load_key(path: &Path) -> Result<AppKey> {
    let pem = std::fs::read(path)
        .with_context(|| format!("can't read the GitHub App private key {}", path.display()))?;
    AppKey::from_pem(&pem).with_context(|| format!("{}", path.display()))
}

/// Connects with the key at `github.private_key_path`.
pub fn connect(config: &Config) -> Result<Client> {
    let key = load_key(&config.github.private_key_path)?;
    connect_with_key(config, key)
}

/// Connects with an already loaded key.
pub fn connect_with_key(config: &Config, key: AppKey) -> Result<Client> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let http = Http::new(GITHUB_API, RetryPolicy::default(), Arc::clone(&clock))
        .context("can't set up the GitHub HTTP client")?;
    let auth = Arc::new(AppAuth::new(
        config.github.app_id,
        config.github.installation_id,
        key,
        http.clone(),
        clock,
    ));
    let gh = OctoGitHub::new(http, Arc::clone(&auth), Redactor::new());
    Ok(Client { gh, auth })
}

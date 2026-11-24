//! Configuration: types, TOML parsing and startup validation (SPEC 5).
//!
//! [`Config::from_toml`] parses and validates everything that can be checked from the text
//! alone and reports every problem at once. [`validate_startup`] adds the checks that need
//! facts from the host (is `data_dir` writable, the private key's file mode, the claude env
//! file's content); the binary collects those facts and passes them in, so this module stays
//! free of I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Span, SpanRelativeTo};
use serde::Deserialize;
use thiserror::Error;

use crate::notify::EventKind;
use crate::policy::ProtectedPaths;
use crate::repo::{RepoName, is_agent_branch, is_valid_login};
use crate::schedule::WorkHours;

/// Default config file location.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/singlelane/config.toml";

/// Default protected paths when a repo does not list any.
pub const DEFAULT_PROTECTED_PATHS: [&str; 4] = [
    ".github/workflows/**",
    ".github/actions/**",
    "CODEOWNERS",
    ".github/CODEOWNERS",
];

/// The validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The only GitHub login whose issues and text are trusted (G1).
    pub trusted_user: String,
    /// Time zone for work hours, ticks and daily jobs.
    pub timezone: TimeZone,
    /// Work hours (G9).
    pub work_hours: WorkHours,
    /// Tick interval, aligned to the wall clock.
    pub poll_interval: Duration,
    /// Attempts per item before it is blocked.
    pub max_attempts: u32,
    /// Consecutive counted failures that trip the breaker (G7).
    pub circuit_breaker_failures: u32,
    /// How long shutdown waits for a running job.
    pub shutdown_grace: Duration,
    /// How long failed workspaces are kept.
    pub keep_failed_workspaces: Duration,
    /// Data directory (work/, logs/, history.db, PAUSE, lock file).
    pub data_dir: PathBuf,
    /// Env file for the agent container (Claude credentials).
    pub claude_env_file: PathBuf,
    /// Nightly image update.
    pub image: ImageConfig,
    /// GitHub App.
    pub github: GithubConfig,
    /// Telegram notifications.
    pub telegram: TelegramConfig,
    /// Agent container.
    pub agent: AgentConfig,
    /// Repos, in config order.
    pub repos: Vec<RepoConfig>,
}

/// Claude Code release channel for the image build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeChannel {
    /// About a week old, skips bad releases.
    Stable,
    /// Newest release.
    Latest,
}

impl ClaudeChannel {
    /// Build-arg value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Latest => "latest",
        }
    }
}

/// `[image]`.
#[derive(Debug, Clone)]
pub struct ImageConfig {
    /// Local time of the nightly update; `None` disables it.
    pub update_at: Option<Time>,
    /// Claude Code channel.
    pub claude_channel: ClaudeChannel,
    /// Directory with the Dockerfile.
    pub dockerfile_dir: PathBuf,
    /// Run a one-turn Claude prompt in the smoke test.
    pub probe_claude: bool,
}

/// `[github]`.
#[derive(Debug, Clone)]
pub struct GithubConfig {
    /// GitHub App id.
    pub app_id: u64,
    /// Installation id.
    pub installation_id: u64,
    /// Private key PEM file.
    pub private_key_path: PathBuf,
    /// When `claude setup-token` was run (tokens last one year).
    pub claude_token_created: Option<Date>,
}

/// `[telegram]`.
#[derive(Debug, Clone)]
pub struct TelegramConfig {
    /// Enabled events.
    pub notify: BTreeSet<EventKind>,
    /// Local time of the daily summary; `None` disables it.
    pub daily_summary_at: Option<Time>,
    /// Review reminder period.
    pub review_reminder_after: Duration,
    /// Accept /status /pause /resume.
    pub commands: bool,
    /// Report outages longer than this once connectivity is back.
    pub network_alert_after: Duration,
}

/// `[agent]`.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Agent image reference.
    pub image: String,
    /// Claude model.
    pub model: String,
    /// Max turns.
    pub max_turns: u32,
    /// Job timeout.
    pub job_timeout: Duration,
    /// Memory limit (docker format, for example `12g`).
    pub memory: String,
    /// CPU limit.
    pub cpus: f64,
    /// `/dev/shm` size (docker format).
    pub shm_size: String,
    /// Max screenshots attached per job.
    pub max_screenshots: u32,
    /// Max size per screenshot.
    pub max_screenshot_bytes: u64,
    /// Override file for the implement prompt template.
    pub implement_prompt: Option<PathBuf>,
    /// Override file for the revise prompt template.
    pub revise_prompt: Option<PathBuf>,
}

/// `[[repo]]`.
#[derive(Debug, Clone)]
pub struct RepoConfig {
    /// `owner/name`.
    pub name: RepoName,
    /// Base branch.
    pub base_branch: String,
    /// Request review from `trusted_user` on new PRs.
    pub request_review: bool,
    /// Protected path globs (G4).
    pub protected_paths: ProtectedPaths,
    /// Sidecar services for the agent stack.
    pub sidecars: Vec<Sidecar>,
}

/// `[[repo.sidecar]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// Compose service name.
    pub name: String,
    /// Image.
    pub image: String,
    /// Environment (non-secret, development values only).
    pub env: BTreeMap<String, String>,
    /// tmpfs mounts.
    pub tmpfs: Vec<String>,
}

/// One configuration problem.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// The TOML does not parse or has unknown or mistyped keys.
    #[error("config is not valid TOML: {0}")]
    Toml(String),
    /// A field has an invalid value.
    #[error("{field}: {message}")]
    Invalid {
        /// Dotted field path.
        field: String,
        /// What is wrong.
        message: String,
    },
}

/// All problems found, reported together.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid configuration ({} problem(s)):", self.0.len())?;
        for e in &self.0 {
            writeln!(f, "  - {e}")?;
        }
        Ok(())
    }
}

impl ConfigErrors {
    /// Whether any error concerns `field` (exact or prefix match).
    pub fn mentions(&self, field: &str) -> bool {
        self.0.iter().any(|e| match e {
            ConfigError::Invalid { field: f, .. } => f.starts_with(field),
            ConfigError::Toml(m) => m.contains(field),
        })
    }
}

// ------------------------------------------------------------------ raw TOML

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    trusted_user: String,
    timezone: String,
    work_hours: RawWorkHours,
    #[serde(default = "d_poll")]
    poll_interval: String,
    #[serde(default = "d_attempts")]
    max_attempts: u32,
    #[serde(default = "d_breaker")]
    circuit_breaker_failures: u32,
    #[serde(default = "d_grace")]
    shutdown_grace: String,
    #[serde(default = "d_keep")]
    keep_failed_workspaces: String,
    #[serde(default = "d_data_dir")]
    data_dir: PathBuf,
    #[serde(default = "d_claude_env")]
    claude_env_file: PathBuf,
    #[serde(default)]
    image: RawImage,
    github: RawGithub,
    #[serde(default)]
    telegram: RawTelegram,
    #[serde(default)]
    agent: RawAgent,
    #[serde(default)]
    repo: Vec<RawRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkHours {
    start: String,
    end: String,
}

/// `[image]`. Omitting the whole section gives the defaults (update at 03:00); inside a
/// present section, omitting `update_at` disables the nightly update.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImage {
    #[serde(default)]
    update_at: Option<String>,
    #[serde(default = "d_channel")]
    claude_channel: String,
    #[serde(default = "d_dockerfile_dir")]
    dockerfile_dir: PathBuf,
    #[serde(default)]
    probe_claude: bool,
}

impl Default for RawImage {
    fn default() -> Self {
        Self {
            update_at: Some("03:00".into()),
            claude_channel: d_channel(),
            dockerfile_dir: d_dockerfile_dir(),
            probe_claude: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGithub {
    app_id: u64,
    installation_id: u64,
    #[serde(default = "d_private_key")]
    private_key_path: PathBuf,
    claude_token_created: Option<String>,
}

/// `[telegram]`. Same rule as `[image]` for `daily_summary_at`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTelegram {
    #[serde(default)]
    notify: Option<Vec<String>>,
    #[serde(default)]
    daily_summary_at: Option<String>,
    #[serde(default = "d_reminder")]
    review_reminder_after: String,
    #[serde(default = "d_true")]
    commands: bool,
    #[serde(default = "d_network_alert")]
    network_alert_after: String,
}

impl Default for RawTelegram {
    fn default() -> Self {
        Self {
            notify: None,
            daily_summary_at: Some("21:00".into()),
            review_reminder_after: d_reminder(),
            commands: true,
            network_alert_after: d_network_alert(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawAgent {
    image: String,
    model: String,
    max_turns: u32,
    job_timeout: String,
    memory: String,
    cpus: f64,
    shm_size: String,
    max_screenshots: u32,
    max_screenshot_bytes: u64,
    implement_prompt: Option<PathBuf>,
    revise_prompt: Option<PathBuf>,
}

impl Default for RawAgent {
    fn default() -> Self {
        Self {
            image: "singlelane-agent:latest".into(),
            model: "opus".into(),
            max_turns: 200,
            job_timeout: "90m".into(),
            memory: "12g".into(),
            cpus: 3.5,
            shm_size: "1g".into(),
            max_screenshots: 10,
            max_screenshot_bytes: 10_000_000,
            implement_prompt: None,
            revise_prompt: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepo {
    name: String,
    #[serde(default = "d_base")]
    base_branch: String,
    #[serde(default = "d_true")]
    request_review: bool,
    protected_paths: Option<Vec<String>>,
    #[serde(default)]
    sidecar: Vec<RawSidecar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSidecar {
    name: String,
    image: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    tmpfs: Vec<String>,
}

fn d_poll() -> String {
    "15m".into()
}
fn d_attempts() -> u32 {
    2
}
fn d_breaker() -> u32 {
    3
}
fn d_grace() -> String {
    "10m".into()
}
fn d_keep() -> String {
    "3d".into()
}
fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/singlelane")
}
fn d_claude_env() -> PathBuf {
    PathBuf::from("/etc/singlelane/claude.env")
}
fn d_private_key() -> PathBuf {
    PathBuf::from("/etc/singlelane/github-app.pem")
}
fn d_channel() -> String {
    "stable".into()
}
fn d_dockerfile_dir() -> PathBuf {
    PathBuf::from("/opt/singlelane/image")
}
fn d_reminder() -> String {
    "2d".into()
}
fn d_network_alert() -> String {
    "2h".into()
}
fn d_base() -> String {
    "main".into()
}
fn d_true() -> bool {
    true
}

// ------------------------------------------------------------------ validation helpers

#[derive(Default)]
struct Errors(Vec<ConfigError>);

impl Errors {
    fn add(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.0.push(ConfigError::Invalid {
            field: field.into(),
            message: message.into(),
        });
    }
}

/// Parses a duration like `15m`, `90m`, `2h`, `1h30m`, `3d` (jiff's friendly format; ISO
/// 8601 such as `PT15M` works too). A day is 24 hours. Zero, negative values and calendar
/// units (months, years) are rejected.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let span: Span = s.trim().parse().map_err(|e| format!("{s:?}: {e}"))?;
    let d: SignedDuration = span
        .to_duration(SpanRelativeTo::days_are_24_hours())
        .map_err(|e| format!("{s:?}: {e}"))?;
    if d.is_zero() || d.is_negative() {
        return Err(format!("{s:?}: must be greater than zero"));
    }
    Duration::try_from(d).map_err(|e| format!("{s:?}: {e}"))
}

/// Parses a local wall-clock time `HH:MM`.
pub fn parse_hhmm(s: &str) -> Result<Time, String> {
    let err = || format!("{s:?}: expected HH:MM");
    let (h, m) = s.split_once(':').ok_or_else(err)?;
    let two_digits = |p: &str| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit());
    if !two_digits(h) || !two_digits(m) {
        return Err(err());
    }
    let h: i8 = h.parse().map_err(|_| err())?;
    let m: i8 = m.parse().map_err(|_| err())?;
    Time::new(h, m, 0, 0).map_err(|_| err())
}

/// Parses a date `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<Date, String> {
    let err = || format!("{s:?}: expected YYYY-MM-DD");
    let shape = s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if !shape {
        return Err(err());
    }
    s.parse::<Date>().map_err(|_| err())
}

/// Docker size strings: digits with an optional `b`, `k`, `m` or `g` suffix.
fn valid_size(s: &str) -> bool {
    let s = s.trim().to_ascii_lowercase();
    let digits = s.trim_end_matches(['b', 'k', 'm', 'g']);
    s.len() - digits.len() <= 1
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits.bytes().any(|b| b != b'0')
}

fn valid_image_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_graphic())
}

/// Compose service names: lowercase letters, digits, `-` and `_`, starting with a letter.
fn valid_service_name(s: &str) -> bool {
    s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn valid_env_key(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Git branch names, conservatively (no spaces, no `..`, no control chars, no leading `-`).
fn valid_branch(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['-', '/'])
        && !s.ends_with(['/', '.'])
        && !s.contains("..")
        && !s.contains("@{")
        && !s.ends_with(".lock")
        && s.chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

fn check_abs(errs: &mut Errors, field: &str, p: &Path) {
    if !p.is_absolute() {
        errs.add(field, format!("{} must be an absolute path", p.display()));
    }
}

// ------------------------------------------------------------------ parsing

impl Config {
    /// Parses and validates a config file's text. Reports all problems at once.
    pub fn from_toml(text: &str) -> Result<Self, ConfigErrors> {
        let raw: RawConfig = toml::from_str(text)
            .map_err(|e| ConfigErrors(vec![ConfigError::Toml(e.message().to_string())]))?;
        let mut errs = Errors::default();

        // Top level.
        let trusted_user = raw.trusted_user.trim().to_string();
        if !is_valid_login(&trusted_user) {
            errs.add("trusted_user", "must be a GitHub user login");
        }
        let timezone = match TimeZone::get(raw.timezone.trim()) {
            Ok(tz) => Some(tz),
            Err(_) => {
                errs.add("timezone", format!("unknown time zone {:?}", raw.timezone));
                None
            }
        };
        let start = parse_hhmm(&raw.work_hours.start)
            .map_err(|e| errs.add("work_hours.start", e))
            .ok();
        let end = parse_hhmm(&raw.work_hours.end)
            .map_err(|e| errs.add("work_hours.end", e))
            .ok();
        if let (Some(s), Some(e)) = (start, end)
            && s >= e
        {
            errs.add("work_hours", "start must be before end");
        }
        let mut dur = |field: &str, s: &str| -> Duration {
            parse_duration(s).unwrap_or_else(|e| {
                errs.add(field, e);
                Duration::ZERO
            })
        };
        let poll_interval = dur("poll_interval", &raw.poll_interval);
        let shutdown_grace = dur("shutdown_grace", &raw.shutdown_grace);
        let keep_failed_workspaces = dur("keep_failed_workspaces", &raw.keep_failed_workspaces);
        let review_reminder_after = dur(
            "telegram.review_reminder_after",
            &raw.telegram.review_reminder_after,
        );
        let network_alert_after = dur(
            "telegram.network_alert_after",
            &raw.telegram.network_alert_after,
        );
        let job_timeout = dur("agent.job_timeout", &raw.agent.job_timeout);
        if !poll_interval.is_zero() {
            let secs = poll_interval.as_secs();
            if secs < 60
                || !secs.is_multiple_of(60)
                || poll_interval.subsec_nanos() != 0
                || !86_400u64.is_multiple_of(secs)
            {
                errs.add(
                    "poll_interval",
                    "must be whole minutes, at least 1m, and divide 24h (for example 5m, 15m, 1h)",
                );
            }
        }
        if raw.max_attempts == 0 {
            errs.add("max_attempts", "must be at least 1");
        }
        if raw.circuit_breaker_failures == 0 {
            errs.add("circuit_breaker_failures", "must be at least 1");
        }
        check_abs(&mut errs, "data_dir", &raw.data_dir);
        check_abs(&mut errs, "claude_env_file", &raw.claude_env_file);

        // [image]
        let update_at = raw.image.update_at.as_deref().and_then(|s| {
            parse_hhmm(s)
                .map_err(|e| errs.add("image.update_at", e))
                .ok()
        });
        let claude_channel = match raw.image.claude_channel.as_str() {
            "stable" => ClaudeChannel::Stable,
            "latest" => ClaudeChannel::Latest,
            other => {
                errs.add(
                    "image.claude_channel",
                    format!("{other:?}: expected \"stable\" or \"latest\""),
                );
                ClaudeChannel::Stable
            }
        };
        check_abs(&mut errs, "image.dockerfile_dir", &raw.image.dockerfile_dir);

        // [github]
        if raw.github.app_id == 0 {
            errs.add("github.app_id", "must be set");
        }
        if raw.github.installation_id == 0 {
            errs.add("github.installation_id", "must be set");
        }
        check_abs(
            &mut errs,
            "github.private_key_path",
            &raw.github.private_key_path,
        );
        let claude_token_created = raw.github.claude_token_created.as_deref().and_then(|s| {
            parse_date(s)
                .map_err(|e| errs.add("github.claude_token_created", e))
                .ok()
        });

        // [telegram]
        let notify = match &raw.telegram.notify {
            None => EventKind::ALL.into_iter().collect(),
            Some(names) => names
                .iter()
                .filter_map(|n| {
                    EventKind::parse(n)
                        .ok_or_else(|| errs.add("telegram.notify", format!("unknown event {n:?}")))
                        .ok()
                })
                .collect(),
        };
        let daily_summary_at = raw.telegram.daily_summary_at.as_deref().and_then(|s| {
            parse_hhmm(s)
                .map_err(|e| errs.add("telegram.daily_summary_at", e))
                .ok()
        });

        // [agent]
        let a = &raw.agent;
        if !valid_image_ref(&a.image) {
            errs.add("agent.image", "must be an image reference");
        }
        if a.model.trim().is_empty() || a.model.chars().any(char::is_whitespace) {
            errs.add("agent.model", "must be a model name without spaces");
        }
        if a.max_turns == 0 {
            errs.add("agent.max_turns", "must be at least 1");
        }
        if !valid_size(&a.memory) {
            errs.add(
                "agent.memory",
                format!("{:?}: expected a size like 12g", a.memory),
            );
        }
        if !valid_size(&a.shm_size) {
            errs.add(
                "agent.shm_size",
                format!("{:?}: expected a size like 1g", a.shm_size),
            );
        }
        if !(a.cpus.is_finite() && a.cpus > 0.0 && a.cpus <= 256.0) {
            errs.add("agent.cpus", "must be a positive number");
        }
        if a.max_screenshot_bytes == 0 {
            errs.add("agent.max_screenshot_bytes", "must be greater than zero");
        }
        for (field, p) in [
            ("agent.implement_prompt", &a.implement_prompt),
            ("agent.revise_prompt", &a.revise_prompt),
        ] {
            if let Some(p) = p {
                check_abs(&mut errs, field, p);
            }
        }

        // [[repo]]
        if raw.repo.is_empty() {
            errs.add("repo", "at least one [[repo]] is required");
        }
        let mut repos = Vec::new();
        let mut seen: Vec<RepoName> = Vec::new();
        for (i, r) in raw.repo.iter().enumerate() {
            let f = |k: &str| format!("repo[{i}].{k}");
            let name = match RepoName::parse(r.name.trim()) {
                Ok(n) => n,
                Err(e) => {
                    errs.add(f("name"), e.to_string());
                    continue;
                }
            };
            if seen.contains(&name) {
                errs.add(f("name"), format!("{name} is listed twice"));
            }
            seen.push(name.clone());
            if !valid_branch(&r.base_branch) {
                errs.add(
                    f("base_branch"),
                    format!("{:?} is not a valid branch", r.base_branch),
                );
            } else if is_agent_branch(&r.base_branch) || r.base_branch == "agent" {
                errs.add(f("base_branch"), "must not be an agent/ branch (G3)");
            }
            let globs: Vec<String> = r.protected_paths.clone().unwrap_or_else(|| {
                DEFAULT_PROTECTED_PATHS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
            let protected_paths = match ProtectedPaths::new(&globs) {
                Ok(p) => p,
                Err(e) => {
                    errs.add(f("protected_paths"), e.to_string());
                    continue;
                }
            };
            let mut sidecars = Vec::new();
            let mut names = BTreeSet::new();
            for (j, s) in r.sidecar.iter().enumerate() {
                let sf = |k: &str| format!("repo[{i}].sidecar[{j}].{k}");
                if !valid_service_name(&s.name) {
                    errs.add(
                        sf("name"),
                        format!("{:?}: lowercase letters, digits, - and _", s.name),
                    );
                } else if s.name == "agent" {
                    errs.add(sf("name"), "\"agent\" is reserved for the agent service");
                } else if !names.insert(s.name.clone()) {
                    errs.add(sf("name"), format!("{:?} is listed twice", s.name));
                }
                if !valid_image_ref(&s.image) {
                    errs.add(sf("image"), "must be an image reference");
                }
                for k in s.env.keys() {
                    if !valid_env_key(k) {
                        errs.add(sf("env"), format!("{k:?} is not a valid variable name"));
                    }
                }
                for t in &s.tmpfs {
                    if !t.starts_with('/') {
                        errs.add(
                            sf("tmpfs"),
                            format!("{t:?} must be an absolute container path"),
                        );
                    }
                }
                sidecars.push(Sidecar {
                    name: s.name.clone(),
                    image: s.image.clone(),
                    env: s.env.clone(),
                    tmpfs: s.tmpfs.clone(),
                });
            }
            repos.push(RepoConfig {
                name,
                base_branch: r.base_branch.clone(),
                request_review: r.request_review,
                protected_paths,
                sidecars,
            });
        }

        let (Some(timezone), Some(start), Some(end), true) =
            (timezone, start, end, errs.0.is_empty())
        else {
            return Err(ConfigErrors(errs.0));
        };

        Ok(Config {
            trusted_user,
            timezone,
            work_hours: WorkHours { start, end },
            poll_interval,
            max_attempts: raw.max_attempts,
            circuit_breaker_failures: raw.circuit_breaker_failures,
            shutdown_grace,
            keep_failed_workspaces,
            data_dir: raw.data_dir,
            claude_env_file: raw.claude_env_file,
            image: ImageConfig {
                update_at,
                claude_channel,
                dockerfile_dir: raw.image.dockerfile_dir,
                probe_claude: raw.image.probe_claude,
            },
            github: GithubConfig {
                app_id: raw.github.app_id,
                installation_id: raw.github.installation_id,
                private_key_path: raw.github.private_key_path,
                claude_token_created,
            },
            telegram: TelegramConfig {
                notify,
                daily_summary_at,
                review_reminder_after,
                commands: raw.telegram.commands,
                network_alert_after,
            },
            agent: AgentConfig {
                image: raw.agent.image,
                model: raw.agent.model,
                max_turns: raw.agent.max_turns,
                job_timeout,
                memory: raw.agent.memory,
                cpus: raw.agent.cpus,
                shm_size: raw.agent.shm_size,
                max_screenshots: raw.agent.max_screenshots,
                max_screenshot_bytes: raw.agent.max_screenshot_bytes,
                implement_prompt: raw.agent.implement_prompt,
                revise_prompt: raw.agent.revise_prompt,
            },
            repos,
        })
    }

    /// The repo config for `name`, if configured.
    pub fn repo(&self, name: &RepoName) -> Option<&RepoConfig> {
        self.repos.iter().find(|r| &r.name == name)
    }

    /// Configured repo names in config order.
    pub fn repo_names(&self) -> Vec<RepoName> {
        self.repos.iter().map(|r| r.name.clone()).collect()
    }
}

// ------------------------------------------------------------------ env files and startup

/// A parsed env file (`KEY=VALUE` lines). Values are never shown by `Debug`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct EnvFile {
    vars: BTreeMap<String, String>,
}

impl fmt::Debug for EnvFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvFile")
            .field("keys", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A malformed env file line. Carries the line number only, never the content.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("env file line {line}: expected KEY=VALUE with a valid variable name")]
pub struct EnvFileError {
    /// 1-based line number.
    pub line: usize,
}

impl EnvFile {
    /// Parses docker-style env file text: `KEY=VALUE`, optional `export `, `#` comments,
    /// blank lines, and values optionally wrapped in matching single or double quotes.
    pub fn parse(text: &str) -> Result<Self, EnvFileError> {
        let mut vars = BTreeMap::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            let Some((key, value)) = line.split_once('=') else {
                return Err(EnvFileError { line: i + 1 });
            };
            let key = key.trim();
            if !valid_env_key(key) {
                return Err(EnvFileError { line: i + 1 });
            }
            let value = value.trim();
            let unquoted = ['"', '\'']
                .into_iter()
                .find_map(|q| value.strip_prefix(q)?.strip_suffix(q))
                .unwrap_or(value);
            vars.insert(key.to_string(), unquoted.to_string());
        }
        Ok(Self { vars })
    }

    /// A non-empty value for `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// Variable names.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }

    /// All non-empty values, for the redactor (G8).
    pub fn secret_values(&self) -> Vec<String> {
        self.vars
            .values()
            .filter(|v| !v.is_empty())
            .cloned()
            .collect()
    }
}

/// Claude auth env var for an API key.
pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
/// Claude auth env var for an OAuth token from `claude setup-token`.
pub const CLAUDE_CODE_OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Which Claude credential the env file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeAuth {
    /// `ANTHROPIC_API_KEY`.
    ApiKey,
    /// `CLAUDE_CODE_OAUTH_TOKEN`.
    OauthToken,
}

/// The claude env file must set exactly one Claude auth variable (SPEC 5).
pub fn check_claude_auth(env: &EnvFile) -> Result<ClaudeAuth, String> {
    match (env.get(ANTHROPIC_API_KEY), env.get(CLAUDE_CODE_OAUTH_TOKEN)) {
        (Some(_), None) => Ok(ClaudeAuth::ApiKey),
        (None, Some(_)) => Ok(ClaudeAuth::OauthToken),
        (Some(_), Some(_)) => Err(format!(
            "both {ANTHROPIC_API_KEY} and {CLAUDE_CODE_OAUTH_TOKEN} are set; keep exactly one"
        )),
        (None, None) => Err(format!(
            "neither {ANTHROPIC_API_KEY} nor {CLAUDE_CODE_OAUTH_TOKEN} is set; set exactly one"
        )),
    }
}

/// Host facts for [`validate_startup`], collected by the binary.
#[derive(Debug, Clone, Default)]
pub struct StartupFacts {
    /// Whether `data_dir` exists (or was created) and is writable.
    pub data_dir_writable: bool,
    /// Unix mode bits of the private key file; `None` if it does not exist or is unreadable.
    pub private_key_mode: Option<u32>,
    /// Text of `claude_env_file`; `None` if it can't be read.
    pub claude_env_text: Option<String>,
}

/// Result of a successful startup validation.
#[derive(Debug, Clone)]
pub struct Startup {
    /// The Claude credential type in use.
    pub claude_auth: ClaudeAuth,
    /// The parsed claude env file (its values go to the redactor).
    pub claude_env: EnvFile,
}

/// Startup checks that need host facts (SPEC 5): `data_dir` writable, private key present
/// and not accessible by group or others, claude env file with exactly one auth variable.
pub fn validate_startup(config: &Config, facts: &StartupFacts) -> Result<Startup, ConfigErrors> {
    let mut errs = Errors::default();
    if !facts.data_dir_writable {
        errs.add(
            "data_dir",
            format!("{} is not writable", config.data_dir.display()),
        );
    }
    let key = config.github.private_key_path.display();
    match facts.private_key_mode {
        None => errs.add(
            "github.private_key_path",
            format!("{key} does not exist or can't be read"),
        ),
        Some(mode) if mode & 0o077 != 0 => errs.add(
            "github.private_key_path",
            format!(
                "{key} has mode {:o}; it must be 600 (no access for group or others)",
                mode & 0o777
            ),
        ),
        Some(_) => {}
    }
    let env_path = config.claude_env_file.display();
    let mut result = None;
    match facts.claude_env_text.as_deref() {
        None => errs.add("claude_env_file", format!("{env_path} can't be read")),
        Some(text) => match EnvFile::parse(text) {
            Err(e) => errs.add("claude_env_file", format!("{env_path}: {e}")),
            Ok(env) => match check_claude_auth(&env) {
                Ok(auth) => result = Some((auth, env)),
                Err(e) => errs.add("claude_env_file", format!("{env_path}: {e}")),
            },
        },
    }
    match result {
        Some((claude_auth, claude_env)) if errs.0.is_empty() => Ok(Startup {
            claude_auth,
            claude_env,
        }),
        _ => Err(ConfigErrors(errs.0)),
    }
}

/// `trusted_user` must not be the app's bot user (SPEC 5.1; checked once the bot login is
/// known from `GET /app`).
pub fn check_trusted_user_is_not_bot(trusted_user: &str, bot_login: &str) -> Result<(), String> {
    let bare = bot_login.strip_suffix("[bot]").unwrap_or(bot_login);
    if trusted_user.eq_ignore_ascii_case(bot_login) || trusted_user.eq_ignore_ascii_case(bare) {
        Err(format!(
            "trusted_user {trusted_user:?} is the app's bot user {bot_login:?}; it must be your own login"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
trusted_user = "your-github-login"
timezone = "Europe/Kyiv"
work_hours = { start = "07:00", end = "23:00" }
poll_interval = "15m"
max_attempts = 2
circuit_breaker_failures = 3
shutdown_grace = "10m"
keep_failed_workspaces = "3d"
data_dir = "/var/lib/singlelane"
claude_env_file = "/etc/singlelane/claude.env"

[image]
update_at = "03:00"
claude_channel = "stable"
dockerfile_dir = "/opt/singlelane/image"
probe_claude = false

[github]
app_id = 123456
installation_id = 12345678
private_key_path = "/etc/singlelane/github-app.pem"
claude_token_created = "2026-11-23"

[telegram]
notify = ["pr_opened", "blocked", "failed", "paused", "resumed", "token_expiry", "network_restored", "review_reminder", "task_released", "image_updated", "image_update_failed", "daily_summary"]
daily_summary_at = "21:00"
review_reminder_after = "2d"
commands = true
network_alert_after = "2h"

[agent]
image = "singlelane-agent:latest"
model = "opus"
max_turns = 200
job_timeout = "90m"
memory = "12g"
cpus = 3.5
shm_size = "1g"
max_screenshots = 10
max_screenshot_bytes = 10_000_000

[[repo]]
name = "owner/monorepo"
base_branch = "main"
request_review = true
protected_paths = [".github/workflows/**", ".github/actions/**", "CODEOWNERS", ".github/CODEOWNERS"]

  [[repo.sidecar]]
  name = "db"
  image = "postgres:17"
  env = { POSTGRES_PASSWORD = "dev", POSTGRES_DB = "app" }
  tmpfs = ["/var/lib/postgresql/data"]
"#;

    const MINIMAL: &str = r#"
trusted_user = "me"
timezone = "UTC"
work_hours = { start = "00:00", end = "23:59" }
[github]
app_id = 1
installation_id = 2
[[repo]]
name = "me/app"
"#;

    fn errors(text: &str) -> ConfigErrors {
        Config::from_toml(text).expect_err("config should be rejected")
    }

    #[test]
    fn spec_example_parses() {
        let c = Config::from_toml(EXAMPLE).unwrap();
        assert_eq!(c.trusted_user, "your-github-login");
        assert_eq!(c.timezone.iana_name(), Some("Europe/Kyiv"));
        assert_eq!(c.poll_interval, Duration::from_secs(900));
        assert_eq!(c.keep_failed_workspaces, Duration::from_secs(3 * 86_400));
        assert_eq!(c.agent.job_timeout, Duration::from_secs(90 * 60));
        assert_eq!(c.telegram.notify.len(), EventKind::ALL.len());
        assert_eq!(c.repos.len(), 1);
        assert_eq!(c.repos[0].sidecars[0].name, "db");
        assert!(c.repos[0].protected_paths.is_protected("CODEOWNERS"));
        assert_eq!(c.image.claude_channel, ClaudeChannel::Stable);
        assert_eq!(c.github.claude_token_created, Date::new(2026, 11, 23).ok());
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(c.max_attempts, 2);
        assert_eq!(c.data_dir, PathBuf::from("/var/lib/singlelane"));
        assert_eq!(c.repos[0].base_branch, "main");
        assert!(c.repos[0].request_review);
        assert!(
            c.repos[0]
                .protected_paths
                .is_protected(".github/workflows/ci.yml")
        );
        assert_eq!(c.agent.model, "opus");
        assert_eq!(c.image.update_at, Time::new(3, 0, 0, 0).ok());
        assert_eq!(c.telegram.notify.len(), EventKind::ALL.len());
    }

    #[test]
    fn omitting_daily_times_inside_a_section_disables_them() {
        let t = MINIMAL.replace(
            "[github]",
            "[image]\nprobe_claude = true\n[telegram]\ncommands = false\n[github]",
        );
        let c = Config::from_toml(&t).unwrap();
        assert_eq!(c.image.update_at, None);
        assert_eq!(c.telegram.daily_summary_at, None);
        assert!(c.image.probe_claude);
        assert!(!c.telegram.commands);
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(c.telegram.daily_summary_at, Time::new(21, 0, 0, 0).ok());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = errors(&format!("{MINIMAL}\nmax_parallel = 2\n"));
        assert!(matches!(e.0[0], ConfigError::Toml(_)));
    }

    #[test]
    fn unknown_time_zone() {
        assert!(errors(&MINIMAL.replace("\"UTC\"", "\"Mars/Base\"")).mentions("timezone"));
    }

    #[test]
    fn work_hours_start_must_be_before_end() {
        let t = MINIMAL.replace(
            r#"start = "00:00", end = "23:59""#,
            r#"start = "23:00", end = "07:00""#,
        );
        assert!(errors(&t).mentions("work_hours"));
        let t = MINIMAL.replace(
            r#"start = "00:00", end = "23:59""#,
            r#"start = "09:00", end = "09:00""#,
        );
        assert!(errors(&t).mentions("work_hours"));
        let t = MINIMAL.replace(r#""00:00""#, r#""7:00""#);
        assert!(errors(&t).mentions("work_hours.start"));
        let t = MINIMAL.replace(r#""23:59""#, r#""24:00""#);
        assert!(errors(&t).mentions("work_hours.end"));
    }

    #[test]
    fn repo_names_must_be_owner_slash_name() {
        let t = MINIMAL.replace(r#"name = "me/app""#, r#"name = "app""#);
        assert!(errors(&t).mentions("repo[0].name"));
        let t = format!("{MINIMAL}\n[[repo]]\nname = \"ME/APP\"\n");
        assert!(errors(&t).mentions("repo[1].name"));
        let t = MINIMAL.replace("[[repo]]\nname = \"me/app\"\n", "");
        assert!(errors(&t).mentions("repo"));
    }

    #[test]
    fn globs_must_compile() {
        let t = format!("{MINIMAL}protected_paths = [\"a[b\"]\n");
        assert!(errors(&t).mentions("repo[0].protected_paths"));
    }

    #[test]
    fn durations_must_parse() {
        for (field, bad) in [
            ("poll_interval", "soon"),
            ("shutdown_grace", "0s"),
            ("keep_failed_workspaces", "3 days later"),
        ] {
            let t = format!("{field} = \"{bad}\"\n{MINIMAL}");
            assert!(errors(&t).mentions(field), "{field}");
        }
        let t = MINIMAL.replace("[github]", "[agent]\njob_timeout = \"forever\"\n[github]");
        assert!(errors(&t).mentions("agent.job_timeout"));
    }

    #[test]
    fn poll_interval_must_align_to_the_day() {
        for bad in ["7m", "30s", "90s"] {
            let t = format!("poll_interval = \"{bad}\"\n{MINIMAL}");
            assert!(errors(&t).mentions("poll_interval"), "{bad}");
        }
        for good in ["1m", "5m", "15m", "1h", "24h"] {
            let t = format!("poll_interval = \"{good}\"\n{MINIMAL}");
            assert!(Config::from_toml(&t).is_ok(), "{good}");
        }
    }

    #[test]
    fn many_problems_are_reported_together() {
        let t = r#"
trusted_user = "-bad-"
timezone = "Nowhere"
work_hours = { start = "10:00", end = "09:00" }
poll_interval = "7m"
max_attempts = 0
data_dir = "relative/dir"
[image]
claude_channel = "nightly"
[github]
app_id = 0
installation_id = 0
[telegram]
notify = ["pr_opened", "everything"]
[agent]
memory = "lots"
cpus = 0
[[repo]]
name = "o/r"
base_branch = "agent/x"
  [[repo.sidecar]]
  name = "agent"
  image = "x"
"#;
        let e = errors(t);
        for f in [
            "trusted_user",
            "timezone",
            "work_hours",
            "poll_interval",
            "max_attempts",
            "data_dir",
            "image.claude_channel",
            "github.app_id",
            "github.installation_id",
            "telegram.notify",
            "agent.memory",
            "agent.cpus",
            "repo[0].base_branch",
            "repo[0].sidecar[0].name",
        ] {
            assert!(e.mentions(f), "missing error for {f}: {e}");
        }
    }

    #[test]
    fn duration_formats() {
        let ok = |s: &str, secs: u64| {
            assert_eq!(parse_duration(s), Ok(Duration::from_secs(secs)), "{s}")
        };
        ok("15m", 900);
        ok("90m", 5400);
        ok("2h", 7200);
        ok("1h30m", 5400);
        ok("3d", 3 * 86_400);
        ok("10000s", 10_000);
        ok("PT15M", 900);
        ok(" 2h ", 7200);
        for bad in ["", "soon", "0s", "-5m", "1 month", "1y", "15"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn time_and_date_formats() {
        assert_eq!(
            parse_hhmm("07:05"),
            Time::new(7, 5, 0, 0).map_err(|e| e.to_string())
        );
        for bad in [
            "7:00", "07:0", "24:00", "07:60", "0700", "07:00:00", "aa:bb", "+7:00",
        ] {
            assert!(parse_hhmm(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_date("2026-11-23"),
            Date::new(2026, 11, 23).map_err(|e| e.to_string())
        );
        for bad in [
            "20261123",
            "2026-11-31",
            "2026-1-01",
            "26-11-23",
            "2026-11-23T00:00",
        ] {
            assert!(parse_date(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn time_zone_aliases_resolve_from_the_bundled_database() {
        for tz in ["Europe/Kyiv", "Europe/Kiev", "UTC", "America/New_York"] {
            let t = MINIMAL.replace("\"UTC\"", &format!("{tz:?}"));
            assert!(Config::from_toml(&t).is_ok(), "{tz}");
        }
    }

    #[test]
    fn sizes() {
        for good in ["12g", "512m", "1G", "1000000", "64k"] {
            assert!(valid_size(good), "{good}");
        }
        for bad in ["", "g", "12gb", "1.5g", "0", "-1g", "12 g"] {
            assert!(!valid_size(bad), "{bad}");
        }
    }

    // ---- env file and startup ----

    fn facts(env: &str) -> StartupFacts {
        StartupFacts {
            data_dir_writable: true,
            private_key_mode: Some(0o100600),
            claude_env_text: Some(env.to_string()),
        }
    }

    #[test]
    fn env_file_parsing() {
        let env = EnvFile::parse("# comment\n\nexport A=1\nB = \"two words\"\nC='x'\nD=\nE=a=b\n")
            .unwrap();
        assert_eq!(env.get("A"), Some("1"));
        assert_eq!(env.get("B"), Some("two words"));
        assert_eq!(env.get("C"), Some("x"));
        assert_eq!(env.get("D"), None);
        assert_eq!(env.get("E"), Some("a=b"));
        assert_eq!(
            EnvFile::parse("A=1\nnot a line\n"),
            Err(EnvFileError { line: 2 })
        );
        assert_eq!(EnvFile::parse("1A=x"), Err(EnvFileError { line: 1 }));
    }

    #[test]
    fn env_file_debug_hides_values() {
        let env = EnvFile::parse("ANTHROPIC_API_KEY=placeholder-secret-value").unwrap();
        let dbg = format!("{env:?}");
        assert!(dbg.contains("ANTHROPIC_API_KEY"));
        assert!(!dbg.contains("placeholder-secret-value"));
    }

    #[test]
    fn claude_env_needs_exactly_one_auth_variable() {
        let c = Config::from_toml(MINIMAL).unwrap();
        let ok = validate_startup(&c, &facts("ANTHROPIC_API_KEY=placeholder-key\n")).unwrap();
        assert_eq!(ok.claude_auth, ClaudeAuth::ApiKey);
        let ok =
            validate_startup(&c, &facts("CLAUDE_CODE_OAUTH_TOKEN=placeholder-token\n")).unwrap();
        assert_eq!(ok.claude_auth, ClaudeAuth::OauthToken);

        let both = "ANTHROPIC_API_KEY=placeholder-key\nCLAUDE_CODE_OAUTH_TOKEN=placeholder-token\n";
        let e = validate_startup(&c, &facts(both)).unwrap_err();
        assert!(e.mentions("claude_env_file"));
        assert!(
            !e.to_string().contains("placeholder"),
            "values must not leak: {e}"
        );

        for neither in [
            "",
            "OTHER=1\n",
            "ANTHROPIC_API_KEY=\nCLAUDE_CODE_OAUTH_TOKEN=\"\"\n",
        ] {
            assert!(
                validate_startup(&c, &facts(neither))
                    .unwrap_err()
                    .mentions("claude_env_file")
            );
        }

        let mut f = facts("ANTHROPIC_API_KEY=x-placeholder");
        f.claude_env_text = None;
        assert!(
            validate_startup(&c, &f)
                .unwrap_err()
                .mentions("claude_env_file")
        );
    }

    #[test]
    fn private_key_mode_and_data_dir() {
        let c = Config::from_toml(MINIMAL).unwrap();
        let env = "ANTHROPIC_API_KEY=placeholder-key";
        for bad in [0o100640, 0o100604, 0o100644, 0o100660, 0o100610] {
            let mut f = facts(env);
            f.private_key_mode = Some(bad);
            assert!(
                validate_startup(&c, &f)
                    .unwrap_err()
                    .mentions("github.private_key_path"),
                "{bad:o}"
            );
        }
        for good in [0o100600, 0o100400] {
            let mut f = facts(env);
            f.private_key_mode = Some(good);
            assert!(validate_startup(&c, &f).is_ok(), "{good:o}");
        }
        let mut f = facts(env);
        f.private_key_mode = None;
        assert!(
            validate_startup(&c, &f)
                .unwrap_err()
                .mentions("github.private_key_path")
        );
        let mut f = facts(env);
        f.data_dir_writable = false;
        assert!(validate_startup(&c, &f).unwrap_err().mentions("data_dir"));
    }

    #[test]
    fn trusted_user_must_not_be_the_bot() {
        assert!(check_trusted_user_is_not_bot("me", "singlelane-me[bot]").is_ok());
        assert!(check_trusted_user_is_not_bot("singlelane-me", "singlelane-me[bot]").is_err());
        assert!(check_trusted_user_is_not_bot("SingleLane-Me[bot]", "singlelane-me[bot]").is_err());
    }
}

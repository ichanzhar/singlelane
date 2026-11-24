//! Failure classification (SPEC 6.8).
//!
//! Every failed job gets exactly one [`FailureClass`]. [`classify`] is a pure function of the
//! facts the host collected. Rules that keep a wrong guess from hiding a real failure:
//! - unknown errors are `agent_failed` (or `factory_bug` for internal errors), never `network`;
//! - `network` needs a failing host-side probe ([`FailureInput::host_probe_failed`]), since
//!   `failure.json` comes from inside the container and could be forged;
//! - more than 5 network requeues of one item in 24 h turn into `agent_failed`;
//! - Claude auth and usage-limit signals are only read from the agent's result event and
//!   Claude's own stderr, never from the general log tail, which includes tool output from
//!   the target repo (its tests may well print "invalid api key").

use std::fmt;
use std::sync::LazyLock;

use jiff::Timestamp;
use regex::Regex;

/// The failure classes of SPEC 6.8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureClass {
    /// DNS, connection refused/reset, TLS handshake timeouts.
    Network,
    /// Claude Code credentials invalid or expired.
    ClaudeAuth,
    /// Claude usage or rate limit reached.
    ClaudeUsageLimit,
    /// GitHub App credentials or installation broken.
    GithubAuth,
    /// Docker down, image missing, disk full, compose error.
    Infra,
    /// The agent ran but produced no usable result.
    AgentFailed,
    /// Panic or unexpected internal error.
    FactoryBug,
}

/// Where the item goes after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemOutcome {
    /// Back to its queue label; the attempt is not counted.
    Requeue,
    /// `agent-blocked` with a comment; the attempt is counted.
    Block,
}

/// What the factory does after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryAction {
    /// Keeps running.
    KeepRunning,
    /// Pauses until `resume` (which re-runs the matching doctor check).
    PauseUntilResume,
    /// Pauses until the usage limit resets, then resumes by itself.
    PauseUntilReset,
}

impl FailureClass {
    /// All classes.
    pub const ALL: [FailureClass; 7] = [
        FailureClass::Network,
        FailureClass::ClaudeAuth,
        FailureClass::ClaudeUsageLimit,
        FailureClass::GithubAuth,
        FailureClass::Infra,
        FailureClass::AgentFailed,
        FailureClass::FactoryBug,
    ];

    /// The snake_case name used in config, the history store and messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::ClaudeAuth => "claude_auth",
            Self::ClaudeUsageLimit => "claude_usage_limit",
            Self::GithubAuth => "github_auth",
            Self::Infra => "infra",
            Self::AgentFailed => "agent_failed",
            Self::FactoryBug => "factory_bug",
        }
    }

    /// Parses [`FailureClass::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// Where the item goes.
    pub fn item_outcome(self) -> ItemOutcome {
        match self {
            Self::AgentFailed | Self::FactoryBug => ItemOutcome::Block,
            _ => ItemOutcome::Requeue,
        }
    }

    /// Whether the attempt counts towards `max_attempts`.
    pub fn counts_attempt(self) -> bool {
        self.item_outcome() == ItemOutcome::Block
    }

    /// Whether the failure counts towards the circuit breaker (G7).
    pub fn counts_for_breaker(self) -> bool {
        matches!(self, Self::AgentFailed | Self::FactoryBug)
    }

    /// What the factory does.
    pub fn factory_action(self) -> FactoryAction {
        match self {
            Self::ClaudeAuth | Self::GithubAuth | Self::Infra => FactoryAction::PauseUntilResume,
            Self::ClaudeUsageLimit => FactoryAction::PauseUntilReset,
            Self::Network | Self::AgentFailed | Self::FactoryBug => FactoryAction::KeepRunning,
        }
    }

    /// Whether this class pauses the factory on its first occurrence (G7 fatal classes).
    pub fn is_fatal(self) -> bool {
        self.factory_action() == FactoryAction::PauseUntilResume
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the error surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// A GitHub REST call from the host.
    GithubApi,
    /// JWT signing or the installation token exchange.
    GithubToken,
    /// A git command on the host (clone, push).
    Git,
    /// docker / docker compose.
    Docker,
    /// The agent container (exit code, `out/status`, `failure.json`, `result.json`).
    Agent,
    /// The Telegram Bot API.
    Telegram,
    /// An internal error in singlelane.
    Internal,
    /// A caught panic in singlelane.
    Panic,
}

/// Facts from the agent container (`result.json`, `failure.json`), untrusted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentReport {
    /// agent-runner exit code.
    pub exit_code: Option<i32>,
    /// Whether agent-runner hit `JOB_TIMEOUT`.
    pub timed_out: bool,
    /// The result event's subtype (for example `error_max_turns`).
    pub result_subtype: Option<String>,
    /// The result event's error text, if any.
    pub result_message: Option<String>,
    /// Last lines of Claude Code's stderr.
    pub stderr_tail: String,
}

/// Everything the classifier looks at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureInput {
    /// Where the error surfaced.
    pub source: Option<ErrorSource>,
    /// HTTP status of a failed HTTP call, if any.
    pub http_status: Option<u16>,
    /// The host-side error message (full error chain).
    pub message: String,
    /// Exit code of a failed host-side command, if any.
    pub exit_code: Option<i32>,
    /// What the agent container reported.
    pub agent: Option<AgentReport>,
    /// Tail of the job log (container output). Only used for infra and network signals.
    pub log_tail: String,
    /// Whether a host-side connectivity probe to GitHub or Anthropic failed.
    pub host_probe_failed: bool,
    /// How often this item was requeued as `network` in the last 24 hours.
    pub network_requeues_24h: u32,
    /// The current time, to turn relative reset times into instants.
    pub now: Option<Timestamp>,
}

/// The classifier's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// The class.
    pub class: FailureClass,
    /// For `claude_usage_limit`: when the limit resets, if Claude said so.
    pub resets_at: Option<Timestamp>,
    /// A short reason for comments and messages (not redacted; redact before posting).
    pub reason: String,
}

/// Network requeues allowed per item per 24 h before a network failure counts as agent failure.
pub const MAX_NETWORK_REQUEUES_24H: u32 = 5;

fn re(p: &str) -> Option<Regex> {
    Regex::new(p).ok()
}

struct Patterns {
    claude_auth: Option<Regex>,
    usage_limit: Option<Regex>,
    usage_reset_epoch: Option<Regex>,
    github_rate_limit: Option<Regex>,
    disk_full: Option<Regex>,
    docker: Option<Regex>,
    network: Option<Regex>,
}

static PATTERNS: LazyLock<Patterns> = LazyLock::new(|| Patterns {
    claude_auth: re(
        r"(?i)invalid api key|invalid x-api-key|authentication_error|oauth token (?:has )?expired|token has expired|invalid bearer token|please run /login|not logged in|credit balance is too low|oauth_org_not_allowed|permission_error",
    ),
    usage_limit: re(
        r"(?i)usage limit|rate_limit_error|rate limit reached|limit will reset|too many requests",
    ),
    usage_reset_epoch: re(r"(?i)limit reached\|(\d{9,11})"),
    github_rate_limit: re(r"(?i)rate limit|abuse detection|secondary rate"),
    disk_full: re(r"(?i)no space left on device|disk quota exceeded"),
    docker: re(
        r"(?i)cannot connect to the docker daemon|is the docker daemon running|no such image|pull access denied|manifest unknown|image .* not found|unable to find image|error response from daemon|docker: command not found|unknown command .*compose|compose.*(?:invalid|error)",
    ),
    network: re(
        r"(?i)could not resolve host|temporary failure in name resolution|name or service not known|no such host|dns error|connection refused|connection reset|connection timed out|tls handshake timeout|handshake timed out|network is unreachable|no route to host|operation timed out|failed to connect|econnreset|econnrefused|enotfound|etimedout|eai_again|unexpected eof|broken pipe",
    ),
});

fn matches(re: &Option<Regex>, text: &str) -> bool {
    re.as_ref().is_some_and(|r| r.is_match(text))
}

fn short(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("");
    let mut s: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        s.push('…');
    }
    s
}

/// Classifies a failure. See the module docs for the rules.
pub fn classify(input: &FailureInput) -> Classification {
    let p = &*PATTERNS;
    let agent = input.agent.as_ref();
    // Claude's own words: result event and Claude's stderr. Not the general log tail.
    let claude_text = agent.map_or_else(String::new, |a| {
        format!(
            "{}\n{}\n{}",
            a.result_subtype.as_deref().unwrap_or(""),
            a.result_message.as_deref().unwrap_or(""),
            a.stderr_tail
        )
    });
    let host_text = &input.message;
    let all_text = format!("{host_text}\n{claude_text}\n{}", input.log_tail);
    let source = input.source;

    let verdict = |class: FailureClass, reason: String| Classification {
        class,
        resets_at: None,
        reason,
    };

    // A panic is always our bug, whatever text it carries.
    if source == Some(ErrorSource::Panic) {
        return verdict(
            FailureClass::FactoryBug,
            format!("panic: {}", short(host_text)),
        );
    }

    // Disk full anywhere on the host side or in the container.
    if matches(&p.disk_full, &all_text) {
        return verdict(FailureClass::Infra, "disk full".to_string());
    }

    // Claude auth and usage limits, from the agent's own report.
    if source == Some(ErrorSource::Agent) || agent.is_some() {
        if matches(&p.claude_auth, &claude_text) {
            return verdict(
                FailureClass::ClaudeAuth,
                format!("Claude Code authentication failed: {}", short(&claude_text)),
            );
        }
        if matches(&p.usage_limit, &claude_text) {
            let resets_at = p
                .usage_reset_epoch
                .as_ref()
                .and_then(|r| r.captures(&claude_text))
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<i64>().ok())
                .and_then(|secs| Timestamp::from_second(secs).ok())
                .filter(|t| input.now.is_none_or(|now| *t > now));
            return Classification {
                class: FailureClass::ClaudeUsageLimit,
                resets_at,
                reason: "Claude usage limit reached".to_string(),
            };
        }
    }

    // GitHub, observed by the host.
    if matches!(
        source,
        Some(ErrorSource::GithubApi | ErrorSource::GithubToken)
    ) {
        let rate_limited = input.http_status == Some(429)
            || (input.http_status == Some(403) && matches(&p.github_rate_limit, host_text));
        match input.http_status {
            _ if rate_limited => {
                // Host-observed and transient: treated like a network failure, no probe
                // needed because the agent cannot forge a GitHub response.
                return verdict(FailureClass::Network, "GitHub API rate limit".to_string());
            }
            Some(401 | 403) => {
                return verdict(
                    FailureClass::GithubAuth,
                    format!(
                        "GitHub rejected the app credentials (HTTP {}): {}",
                        input.http_status.unwrap_or_default(),
                        short(host_text)
                    ),
                );
            }
            Some(404) if source == Some(ErrorSource::GithubToken) => {
                return verdict(
                    FailureClass::GithubAuth,
                    "GitHub App installation not found (removed or suspended?)".to_string(),
                );
            }
            _ if source == Some(ErrorSource::GithubToken) && input.http_status.is_some() => {
                return verdict(
                    FailureClass::GithubAuth,
                    format!("installation token exchange failed: {}", short(host_text)),
                );
            }
            _ => {}
        }
    }

    // Docker / compose problems seen by the host.
    if source == Some(ErrorSource::Docker) {
        if matches(&p.network, host_text) && input.host_probe_failed {
            return network_or_backstop(input, short(host_text));
        }
        return verdict(FailureClass::Infra, format!("docker: {}", short(host_text)));
    }
    if matches(&p.docker, host_text) {
        return verdict(FailureClass::Infra, format!("docker: {}", short(host_text)));
    }

    // Network: needs a signal and a failing host probe.
    let network_text = format!("{host_text}\n{claude_text}");
    if matches(&p.network, &network_text) && input.host_probe_failed {
        return network_or_backstop(input, short(&network_text));
    }

    // Fallbacks: never network.
    match source {
        Some(ErrorSource::Internal) => verdict(
            FailureClass::FactoryBug,
            format!("internal error: {}", short(host_text)),
        ),
        _ => verdict(FailureClass::AgentFailed, agent_reason(input)),
    }
}

fn network_or_backstop(input: &FailureInput, detail: String) -> Classification {
    if input.network_requeues_24h >= MAX_NETWORK_REQUEUES_24H {
        Classification {
            class: FailureClass::AgentFailed,
            resets_at: None,
            reason: format!(
                "network failure repeated more than {MAX_NETWORK_REQUEUES_24H} times in 24 h: {detail}"
            ),
        }
    } else {
        Classification {
            class: FailureClass::Network,
            resets_at: None,
            reason: format!("network: {detail}"),
        }
    }
}

fn agent_reason(input: &FailureInput) -> String {
    if let Some(a) = &input.agent {
        if a.timed_out {
            return "the agent hit the job timeout".to_string();
        }
        if let Some(sub) = a
            .result_subtype
            .as_deref()
            .filter(|s| s.starts_with("error"))
        {
            return format!("the agent stopped with {sub}");
        }
    }
    let msg = short(&input.message);
    if msg.is_empty() {
        "the agent produced no usable result".to_string()
    } else {
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(message: &str, stderr: &str) -> FailureInput {
        FailureInput {
            source: Some(ErrorSource::Agent),
            message: "agent exited with status failed".into(),
            exit_code: Some(1),
            agent: Some(AgentReport {
                exit_code: Some(1),
                result_subtype: Some("error_during_execution".into()),
                result_message: Some(message.into()),
                stderr_tail: stderr.into(),
                ..AgentReport::default()
            }),
            ..FailureInput::default()
        }
    }

    fn host(source: ErrorSource, status: Option<u16>, message: &str) -> FailureInput {
        FailureInput {
            source: Some(source),
            http_status: status,
            message: message.into(),
            ..FailureInput::default()
        }
    }

    // ---- One fixture per row of the SPEC 6.8 table. ----

    #[test]
    fn fixture_network_dns_failure_with_failing_probe() {
        let mut i = host(
            ErrorSource::Git,
            None,
            "fatal: unable to access 'https://github.com/o/r.git/': Could not resolve host: github.com",
        );
        i.host_probe_failed = true;
        assert_eq!(classify(&i).class, FailureClass::Network);
    }

    #[test]
    fn fixture_network_connection_reset_inside_agent_with_failing_probe() {
        let mut i = agent("", "Error: read ECONNRESET\n    at TLSWrap.onStreamRead");
        i.host_probe_failed = true;
        assert_eq!(classify(&i).class, FailureClass::Network);
    }

    #[test]
    fn fixture_claude_auth_invalid_api_key() {
        let i = agent("Invalid API key · Please run /login", "");
        assert_eq!(classify(&i).class, FailureClass::ClaudeAuth);
    }

    #[test]
    fn fixture_claude_auth_expired_oauth_token() {
        let i = agent(
            "",
            r#"API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"OAuth token has expired."}}"#,
        );
        assert_eq!(classify(&i).class, FailureClass::ClaudeAuth);
    }

    #[test]
    fn fixture_claude_usage_limit_with_reset_time() {
        let mut i = agent("Claude AI usage limit reached|1795600800", "");
        i.now = Timestamp::from_second(1_795_590_000).ok();
        let c = classify(&i);
        assert_eq!(c.class, FailureClass::ClaudeUsageLimit);
        assert_eq!(c.resets_at, Timestamp::from_second(1_795_600_800).ok());
    }

    #[test]
    fn fixture_claude_usage_limit_without_reset_time() {
        let i = agent(
            "",
            r#"API Error: 429 {"type":"error","error":{"type":"rate_limit_error","message":"..."}}"#,
        );
        let c = classify(&i);
        assert_eq!(c.class, FailureClass::ClaudeUsageLimit);
        assert_eq!(c.resets_at, None);
    }

    #[test]
    fn fixture_github_auth_jwt_rejected() {
        let i = host(
            ErrorSource::GithubToken,
            Some(401),
            "A JSON web token could not be decoded",
        );
        assert_eq!(classify(&i).class, FailureClass::GithubAuth);
    }

    #[test]
    fn fixture_github_auth_installation_removed() {
        let i = host(ErrorSource::GithubToken, Some(404), "Not Found");
        assert_eq!(classify(&i).class, FailureClass::GithubAuth);
    }

    #[test]
    fn fixture_github_auth_403_that_is_not_a_rate_limit() {
        let i = host(
            ErrorSource::GithubApi,
            Some(403),
            "Resource not accessible by integration",
        );
        assert_eq!(classify(&i).class, FailureClass::GithubAuth);
    }

    #[test]
    fn fixture_github_rate_limit_is_not_github_auth() {
        let i = host(
            ErrorSource::GithubApi,
            Some(403),
            "API rate limit exceeded for installation ID 1",
        );
        assert_eq!(classify(&i).class, FailureClass::Network);
    }

    #[test]
    fn fixture_infra_docker_daemon_down() {
        let i = host(
            ErrorSource::Docker,
            None,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
        );
        assert_eq!(classify(&i).class, FailureClass::Infra);
    }

    #[test]
    fn fixture_infra_image_missing() {
        let i = host(
            ErrorSource::Docker,
            None,
            "Error response from daemon: No such image: singlelane-agent:latest",
        );
        assert_eq!(classify(&i).class, FailureClass::Infra);
    }

    #[test]
    fn fixture_infra_disk_full() {
        let mut i = agent("", "");
        i.log_tail = "write /work/build/x: no space left on device".into();
        assert_eq!(classify(&i).class, FailureClass::Infra);
    }

    #[test]
    fn fixture_infra_compose_error() {
        let i = host(
            ErrorSource::Docker,
            None,
            "service \"db\" refers to undefined volume pgdata: invalid compose project",
        );
        assert_eq!(classify(&i).class, FailureClass::Infra);
    }

    #[test]
    fn fixture_agent_failed_no_usable_result() {
        let i = FailureInput {
            source: Some(ErrorSource::Agent),
            message: "status done but no new commits".into(),
            agent: Some(AgentReport {
                exit_code: Some(0),
                result_subtype: Some("success".into()),
                ..AgentReport::default()
            }),
            ..FailureInput::default()
        };
        assert_eq!(classify(&i).class, FailureClass::AgentFailed);
    }

    #[test]
    fn fixture_agent_failed_timeout() {
        let i = FailureInput {
            source: Some(ErrorSource::Agent),
            agent: Some(AgentReport {
                exit_code: Some(124),
                timed_out: true,
                ..AgentReport::default()
            }),
            ..FailureInput::default()
        };
        let c = classify(&i);
        assert_eq!(c.class, FailureClass::AgentFailed);
        assert!(c.reason.contains("timeout"));
    }

    #[test]
    fn fixture_agent_failed_max_turns() {
        let mut i = agent("", "");
        i.agent = Some(AgentReport {
            result_subtype: Some("error_max_turns".into()),
            ..AgentReport::default()
        });
        assert_eq!(classify(&i).class, FailureClass::AgentFailed);
    }

    #[test]
    fn fixture_factory_bug_panic() {
        let i = host(
            ErrorSource::Panic,
            None,
            "panicked at src/job.rs:10: index out of bounds",
        );
        assert_eq!(classify(&i).class, FailureClass::FactoryBug);
    }

    #[test]
    fn fixture_factory_bug_internal_error() {
        let i = host(
            ErrorSource::Internal,
            None,
            "history store: database is locked",
        );
        assert_eq!(classify(&i).class, FailureClass::FactoryBug);
    }

    // ---- Safety rules. ----

    #[test]
    fn unknown_errors_never_classify_as_network() {
        let unknown = [
            "",
            "something odd happened",
            "exit status 1",
            "connection: keep-alive",
        ];
        for source in [
            None,
            Some(ErrorSource::Agent),
            Some(ErrorSource::Git),
            Some(ErrorSource::GithubApi),
            Some(ErrorSource::Telegram),
            Some(ErrorSource::Internal),
        ] {
            for msg in unknown {
                for probe in [false, true] {
                    let i = FailureInput {
                        source,
                        message: msg.into(),
                        host_probe_failed: probe,
                        ..FailureInput::default()
                    };
                    assert_ne!(
                        classify(&i).class,
                        FailureClass::Network,
                        "{source:?} {msg:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn network_signal_without_failing_probe_is_agent_failed() {
        let i = agent("", "Error: getaddrinfo EAI_AGAIN api.anthropic.com");
        assert_eq!(classify(&i).class, FailureClass::AgentFailed);
    }

    #[test]
    fn repeated_network_requeues_become_agent_failed() {
        let mut i = host(ErrorSource::Git, None, "Connection reset by peer");
        i.host_probe_failed = true;
        i.network_requeues_24h = MAX_NETWORK_REQUEUES_24H - 1;
        assert_eq!(classify(&i).class, FailureClass::Network);
        i.network_requeues_24h = MAX_NETWORK_REQUEUES_24H;
        assert_eq!(classify(&i).class, FailureClass::AgentFailed);
    }

    #[test]
    fn claude_auth_text_in_repo_test_output_is_not_claude_auth() {
        let mut i = agent("", "");
        i.agent = Some(AgentReport {
            exit_code: Some(1),
            result_subtype: Some("error_max_turns".into()),
            ..AgentReport::default()
        });
        i.log_tail = "FAIL ApiKeyTest: expected 'invalid api key' error".into();
        assert_eq!(classify(&i).class, FailureClass::AgentFailed);
    }

    #[test]
    fn class_properties_match_the_table() {
        use FailureClass as C;
        for c in C::ALL {
            assert_eq!(C::parse(c.as_str()), Some(c));
        }
        assert_eq!(C::Network.item_outcome(), ItemOutcome::Requeue);
        assert!(!C::Network.counts_for_breaker() && !C::Network.is_fatal());
        assert_eq!(
            C::ClaudeAuth.factory_action(),
            FactoryAction::PauseUntilResume
        );
        assert_eq!(
            C::ClaudeUsageLimit.factory_action(),
            FactoryAction::PauseUntilReset
        );
        assert!(!C::ClaudeUsageLimit.counts_for_breaker());
        assert_eq!(
            C::GithubAuth.factory_action(),
            FactoryAction::PauseUntilResume
        );
        assert_eq!(C::Infra.factory_action(), FactoryAction::PauseUntilResume);
        for c in [C::AgentFailed, C::FactoryBug] {
            assert_eq!(c.item_outcome(), ItemOutcome::Block);
            assert!(c.counts_for_breaker() && c.counts_attempt());
            assert_eq!(c.factory_action(), FactoryAction::KeepRunning);
        }
        for c in [
            C::Network,
            C::ClaudeAuth,
            C::ClaudeUsageLimit,
            C::GithubAuth,
            C::Infra,
        ] {
            assert!(!c.counts_attempt());
        }
    }
}

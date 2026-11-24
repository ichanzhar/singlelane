//! The files the agent container leaves in `/out` (SPEC 6.6 and 7).
//!
//! `agent-runner` writes them inside the container and the host reads them, so the names and
//! the JSON shapes live here, in one place. Everything in these files comes from inside the
//! container and is untrusted: the host treats it as a report, never as a decision.
//!
//! Both JSON types tolerate missing and unknown fields, because the image (with its
//! `agent-runner`) and the host binary are updated separately.

use serde::{Deserialize, Serialize};

/// `done`, `blocked`, or anything else (= failed). Written by the agent, or by `agent-runner`
/// when the agent left none.
pub const STATUS_FILE: &str = "status";
/// Run statistics from Claude Code's final `result` event ([`RunResult`]).
pub const RESULT_FILE: &str = "result.json";
/// Raw failure facts for classification on the host ([`FailureReport`]).
pub const FAILURE_JSON_FILE: &str = "failure.json";
/// A one-line human-readable failure reason.
pub const FAILURE_TEXT_FILE: &str = "failure.txt";
/// Claude Code's stream-json output, one event per line.
pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// How many trailing lines of Claude Code's stderr go into [`FailureReport::stderr_tail`].
pub const STDERR_TAIL_LINES: usize = 50;

/// `agent-runner` exit code: the agent finished with `done` or `blocked`.
pub const EXIT_OK: i32 = 0;
/// `agent-runner` exit code: the run failed.
pub const EXIT_FAILED: i32 = 1;
/// `agent-runner` exit code: the runner could not start Claude Code (bad environment, missing
/// prompt, git setup).
pub const EXIT_SETUP: i32 = 2;
/// `agent-runner` exit code: the run failed because it hit `JOB_TIMEOUT` (same code as
/// coreutils `timeout`).
pub const EXIT_TIMEOUT: i32 = 124;

/// The content of the status file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    /// The agent finished the task.
    Done,
    /// The agent needs an answer from the owner (`question.md`).
    Blocked,
    /// Anything else, including a missing or empty file.
    Failed,
}

impl AgentStatus {
    /// Parses the status file's text. Surrounding whitespace and letter case are ignored;
    /// anything other than `done` or `blocked` is [`AgentStatus::Failed`].
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            _ => Self::Failed,
        }
    }

    /// The word written to the status file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }

    /// Whether the run counts as failed.
    pub fn is_failed(self) -> bool {
        self == Self::Failed
    }
}

/// `result.json`: run statistics. Always written, also when Claude Code produced no `result`
/// event (then only the fields measured by `agent-runner` are set).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunResult {
    /// The last result event's `subtype` (`success`, `error_max_turns`, ...). `success` does
    /// not mean the run worked: check [`RunResult::is_error`].
    pub subtype: Option<String>,
    /// The last result event's `is_error`.
    pub is_error: Option<bool>,
    /// Agentic turns (`num_turns`, summed over all result events of the run).
    pub turns: Option<u64>,
    /// Estimated cost in USD (`total_cost_usd` of the last result event, which is cumulative).
    pub cost_usd: Option<f64>,
    /// `duration_ms`, summed over all result events.
    pub duration_ms: Option<u64>,
    /// `duration_api_ms`, summed over all result events.
    pub duration_api_ms: Option<u64>,
    /// Why Claude Code's loop ended (`terminal_reason`: `completed`, `max_turns`, `api_error`...).
    pub terminal_reason: Option<String>,
    /// Claude Code's session id.
    pub session_id: Option<String>,
    /// Claude Code's version, from the `system/init` event.
    pub claude_code_version: Option<String>,
    /// The model Claude Code resolved, from the `system/init` event.
    pub model: Option<String>,
    /// How many `result` events the stream carried (0 = none, for example after a timeout).
    pub result_events: u32,
    /// Wall-clock time Claude Code ran, measured by `agent-runner`.
    pub wall_ms: u64,
    /// Whether `agent-runner` stopped Claude Code because of `JOB_TIMEOUT`.
    pub timed_out: bool,
    /// Claude Code's exit code (absent when it was killed by a signal or never started).
    pub exit_code: Option<i32>,
}

/// `failure.json`: the raw facts of a failed run. `agent-runner` reports, the host classifies
/// (SPEC 6.8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FailureReport {
    /// The human-readable reason, the same text as `failure.txt`.
    pub reason: String,
    /// Claude Code's exit code (absent when it was killed by a signal or never started).
    pub exit_code: Option<i32>,
    /// The signal that ended Claude Code, if any.
    pub signal: Option<i32>,
    /// Whether `agent-runner` stopped Claude Code because of `JOB_TIMEOUT`.
    pub timed_out: bool,
    /// Whether `agent-runner` itself was told to stop (SIGTERM or SIGINT) and took Claude Code
    /// down with it.
    pub terminated: bool,
    /// Set when Claude Code never ran: bad environment, unreadable prompt, git setup failure,
    /// or the `claude` binary could not be started.
    pub setup_error: Option<String>,
    /// The last result event's `subtype`.
    pub result_subtype: Option<String>,
    /// The last result event's `is_error`.
    pub result_is_error: Option<bool>,
    /// The error text of the last result event: its `errors` joined, or its `result` text when
    /// `is_error` is set.
    pub result_message: Option<String>,
    /// The last result event's `terminal_reason`.
    pub terminal_reason: Option<String>,
    /// The last result event's `startup_failure_reason`.
    pub startup_failure_reason: Option<String>,
    /// HTTP status of the Anthropic API error that ended the run (401, 429, 529...).
    pub api_error_status: Option<u16>,
    /// The API's error type (`authentication_error`, `rate_limit_error`...).
    pub api_error_type: Option<String>,
    /// Claude Code's error category for the API failure that ended the run
    /// (`authentication_failed`, `billing_error`, `rate_limit`, `overloaded`...).
    pub error_category: Option<String>,
    /// When a rejected usage limit resets, in Unix seconds, if Claude Code reported it.
    pub rate_limit_resets_at: Option<i64>,
    /// The last [`STDERR_TAIL_LINES`] lines of Claude Code's stderr, redacted.
    pub stderr_tail: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_done_and_blocked_only() {
        assert_eq!(AgentStatus::parse("done"), AgentStatus::Done);
        assert_eq!(AgentStatus::parse("done\n"), AgentStatus::Done);
        assert_eq!(AgentStatus::parse("  blocked \n"), AgentStatus::Blocked);
        assert_eq!(AgentStatus::parse("Done"), AgentStatus::Done);
        for other in ["", "failed", "done and dusted", "ok", "\n"] {
            assert_eq!(AgentStatus::parse(other), AgentStatus::Failed, "{other:?}");
        }
    }

    #[test]
    fn status_round_trips() {
        for s in [AgentStatus::Done, AgentStatus::Blocked, AgentStatus::Failed] {
            assert_eq!(AgentStatus::parse(s.as_str()), s);
        }
        assert!(AgentStatus::Failed.is_failed());
        assert!(!AgentStatus::Blocked.is_failed());
    }

    #[test]
    fn exit_codes_are_zero_only_for_success() {
        assert_eq!(EXIT_OK, 0);
        for code in [EXIT_FAILED, EXIT_SETUP, EXIT_TIMEOUT] {
            assert_ne!(code, 0);
        }
    }
}

//! Reads Claude Code's `--output-format stream-json` events (one JSON object per line).
//!
//! The parser only collects facts and never fails: a line that is not JSON, or an event with
//! missing or oddly typed fields, is counted and skipped (SPEC 7: "never fail the job because
//! of a parse problem").
//!
//! Field names were checked on 2026-11-24 against
//! - <https://code.claude.com/docs/en/headless> (stream-json, `system/init`, `system/api_retry`),
//! - <https://code.claude.com/docs/en/agent-sdk/typescript> (`SDKResultMessage`,
//!   `SDKAssistantMessage`, `SDKRateLimitEvent`),
//! - <https://code.claude.com/docs/en/errors> (authentication and usage-limit messages),
//!
//! and against real output of Claude Code 2.1.331 for the authentication failures. What that
//! showed and the docs do not spell out:
//! - an API failure ends the run with a result event of `subtype: "success"` and
//!   `is_error: true`, plus `api_error_status`, `api_error_type` and
//!   `terminal_reason: "api_error"`. So `subtype` alone never tells whether a run worked;
//! - the assistant message that carries the error text has `error` (the category, for example
//!   `authentication_failed`), `api_error_status` and `api_error_type` at the top level.
//!
//! Samples of every case are in `tests/fixtures/`.

use serde_json::Value;

/// Longest error text kept from an event.
const MAX_MESSAGE_CHARS: usize = 2000;

/// The facts of one `result` event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultEvent {
    /// `subtype`: `success`, `error_max_turns`, `error_during_execution`, ...
    pub subtype: Option<String>,
    /// `is_error`.
    pub is_error: Option<bool>,
    /// `num_turns`.
    pub num_turns: Option<u64>,
    /// `total_cost_usd` (cumulative for the run).
    pub total_cost_usd: Option<f64>,
    /// `duration_ms`.
    pub duration_ms: Option<u64>,
    /// `duration_api_ms`.
    pub duration_api_ms: Option<u64>,
    /// `session_id`.
    pub session_id: Option<String>,
    /// `terminal_reason`.
    pub terminal_reason: Option<String>,
    /// `api_error_status`.
    pub api_error_status: Option<u16>,
    /// `api_error_type`.
    pub api_error_type: Option<String>,
    /// `startup_failure_reason`.
    pub startup_failure_reason: Option<String>,
    /// The error text: `errors` joined, or `result` when `is_error` is set.
    pub error_message: Option<String>,
}

/// An API failure reported by an assistant message or a `system/api_retry` event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiError {
    /// Claude Code's error category (`authentication_failed`, `rate_limit`, `overloaded`...).
    pub category: Option<String>,
    /// HTTP status, if the API answered.
    pub status: Option<u16>,
    /// The API's error type (`authentication_error`...).
    pub error_type: Option<String>,
}

/// Collects facts from the event stream.
#[derive(Debug, Clone, Default)]
pub struct StreamParser {
    lines: u64,
    unparsed: u64,
    result_events: u32,
    last_result: Option<ResultEvent>,
    turns: Option<u64>,
    duration_ms: Option<u64>,
    duration_api_ms: Option<u64>,
    claude_code_version: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
    pending_error: Option<ApiError>,
    rate_limit_resets_at: Option<i64>,
}

impl StreamParser {
    /// A parser that has seen nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one line of output (with or without its line ending).
    pub fn feed(&mut self, line: &[u8]) {
        let line = line.trim_ascii();
        if line.is_empty() {
            return;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(event) => self.feed_event(&event),
            Err(_) => self.feed_unparsed(),
        }
    }

    /// Feeds one line that the caller has already parsed.
    pub fn feed_event(&mut self, event: &Value) {
        if !event.is_object() {
            self.feed_unparsed();
            return;
        }
        self.lines += 1;
        if self.session_id.is_none() {
            self.session_id = string(event, "session_id");
        }
        match event.get("type").and_then(Value::as_str) {
            Some("system") => self.on_system(event),
            Some("assistant") => self.on_assistant(event),
            Some("rate_limit_event") => self.on_rate_limit(event),
            Some("result") => self.on_result(event),
            _ => {}
        }
    }

    /// Counts a non-empty line that is not a JSON object.
    pub fn feed_unparsed(&mut self) {
        self.lines += 1;
        self.unparsed += 1;
    }

    fn on_system(&mut self, event: &Value) {
        match event.get("subtype").and_then(Value::as_str) {
            Some("init") => {
                self.claude_code_version = string(event, "claude_code_version");
                self.model = string(event, "model");
            }
            // Claude Code retries by itself; the error only matters if nothing succeeds
            // afterwards (see `on_assistant`).
            Some("api_retry") => {
                self.pending_error = Some(ApiError {
                    category: string(event, "error"),
                    status: status(event, "error_status"),
                    error_type: None,
                });
            }
            _ => {}
        }
    }

    fn on_assistant(&mut self, event: &Value) {
        match string(event, "error") {
            Some(category) => {
                self.pending_error = Some(ApiError {
                    category: Some(category),
                    status: status(event, "api_error_status"),
                    error_type: string(event, "api_error_type"),
                });
            }
            // A normal answer: the API works (again), earlier retries and limits are history.
            None => {
                self.pending_error = None;
                self.rate_limit_resets_at = None;
            }
        }
    }

    fn on_rate_limit(&mut self, event: &Value) {
        let Some(info) = event.get("rate_limit_info") else {
            return;
        };
        if info.get("status").and_then(Value::as_str) == Some("rejected") {
            self.rate_limit_resets_at = info
                .get("resetsAt")
                .and_then(number_as_i64)
                // Unix seconds; tolerate milliseconds.
                .map(|t| if t > 100_000_000_000 { t / 1000 } else { t })
                .filter(|t| *t > 0);
        } else {
            self.rate_limit_resets_at = None;
        }
    }

    fn on_result(&mut self, event: &Value) {
        let is_error = event.get("is_error").and_then(Value::as_bool);
        let errors: Vec<&str> = event
            .get("errors")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let error_message = if !errors.is_empty() {
            Some(errors.join("; "))
        } else if is_error == Some(true) {
            string(event, "result").filter(|s| !s.trim().is_empty())
        } else {
            None
        };
        let result = ResultEvent {
            subtype: string(event, "subtype"),
            is_error,
            num_turns: event.get("num_turns").and_then(number_as_u64),
            total_cost_usd: event
                .get("total_cost_usd")
                .and_then(Value::as_f64)
                .filter(|c| c.is_finite() && *c >= 0.0),
            duration_ms: event.get("duration_ms").and_then(number_as_u64),
            duration_api_ms: event.get("duration_api_ms").and_then(number_as_u64),
            session_id: string(event, "session_id"),
            terminal_reason: string(event, "terminal_reason"),
            api_error_status: status(event, "api_error_status"),
            api_error_type: string(event, "api_error_type"),
            startup_failure_reason: string(event, "startup_failure_reason"),
            error_message: error_message.map(|m| truncate(&m)),
        };
        // A run can carry several result events (a follow-up turn after background work).
        // Turns and durations are per event, the cost is cumulative.
        self.turns = sum(self.turns, result.num_turns);
        self.duration_ms = sum(self.duration_ms, result.duration_ms);
        self.duration_api_ms = sum(self.duration_api_ms, result.duration_api_ms);
        self.result_events = self.result_events.saturating_add(1);
        self.last_result = Some(result);
    }

    /// The last `result` event, if any.
    pub fn last_result(&self) -> Option<&ResultEvent> {
        self.last_result.as_ref()
    }

    /// How many `result` events were seen.
    pub fn result_events(&self) -> u32 {
        self.result_events
    }

    /// `num_turns` summed over all result events.
    pub fn turns(&self) -> Option<u64> {
        self.turns
    }

    /// `duration_ms` summed over all result events.
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// `duration_api_ms` summed over all result events.
    pub fn duration_api_ms(&self) -> Option<u64> {
        self.duration_api_ms
    }

    /// Claude Code's version from `system/init`.
    pub fn claude_code_version(&self) -> Option<&str> {
        self.claude_code_version.as_deref()
    }

    /// The resolved model from `system/init`.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The session id (from the result event, else from the first event that had one).
    pub fn session_id(&self) -> Option<&str> {
        self.last_result
            .as_ref()
            .and_then(|r| r.session_id.as_deref())
            .or(self.session_id.as_deref())
    }

    /// The API failure the run ended on: set only if no successful answer followed it.
    pub fn terminal_api_error(&self) -> Option<&ApiError> {
        self.pending_error.as_ref()
    }

    /// When a rejected usage limit resets (Unix seconds), if the run ended on one.
    pub fn rate_limit_resets_at(&self) -> Option<i64> {
        self.rate_limit_resets_at
    }

    /// Non-empty lines seen.
    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Lines that were not a JSON object.
    pub fn unparsed_lines(&self) -> u64 {
        self.unparsed
    }
}

fn string(event: &Value, key: &str) -> Option<String> {
    event.get(key).and_then(Value::as_str).map(str::to_string)
}

fn status(event: &Value, key: &str) -> Option<u16> {
    event
        .get(key)
        .and_then(number_as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .filter(|s| (100..=599).contains(s))
}

fn number_as_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0 && *f < 9.0e15)
            .map(|f| f.round() as u64)
    })
}

fn number_as_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && f.abs() < 9.0e15)
            .map(|f| f.round() as i64)
    })
}

fn sum(total: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (total, value) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    }
}

/// Cuts `text` to [`MAX_MESSAGE_CHARS`] characters.
pub fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return text.to_string();
    }
    let mut s: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(fixture: &str) -> StreamParser {
        let mut p = StreamParser::new();
        for line in fixture.lines() {
            p.feed(line.as_bytes());
        }
        p
    }

    #[test]
    fn success_fixture_yields_turns_cost_duration_and_subtype() {
        let p = parse(include_str!("../tests/fixtures/success.jsonl"));
        let r = p.last_result().expect("result event");
        assert_eq!(r.subtype.as_deref(), Some("success"));
        assert_eq!(r.is_error, Some(false));
        assert_eq!(r.terminal_reason.as_deref(), Some("completed"));
        assert_eq!(r.error_message, None);
        assert_eq!(p.turns(), Some(3));
        assert_eq!(p.duration_ms(), Some(48_210));
        assert_eq!(p.duration_api_ms(), Some(41_530));
        assert_eq!(r.total_cost_usd, Some(0.4127));
        assert_eq!(p.result_events(), 1);
        assert_eq!(p.claude_code_version(), Some("2.1.331"));
        assert_eq!(p.model(), Some("claude-opus-5-5"));
        assert_eq!(p.session_id(), Some("00000000-0000-4000-8000-000000000001"));
        assert_eq!(p.terminal_api_error(), None);
        assert_eq!(p.unparsed_lines(), 0);
    }

    #[test]
    fn invalid_api_key_is_a_success_subtype_with_is_error() {
        let p = parse(include_str!("../tests/fixtures/auth_invalid_api_key.jsonl"));
        let r = p.last_result().expect("result event");
        assert_eq!(r.subtype.as_deref(), Some("success"));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(r.api_error_status, Some(401));
        assert_eq!(r.api_error_type.as_deref(), Some("authentication_error"));
        assert_eq!(r.terminal_reason.as_deref(), Some("api_error"));
        assert_eq!(
            r.error_message.as_deref(),
            Some("Invalid API key · Fix external API key")
        );
        let e = p.terminal_api_error().expect("api error");
        assert_eq!(e.category.as_deref(), Some("authentication_failed"));
        assert_eq!(e.status, Some(401));
        assert_eq!(e.error_type.as_deref(), Some("authentication_error"));
    }

    #[test]
    fn not_logged_in_goes_through_api_retries_first() {
        let p = parse(include_str!("../tests/fixtures/auth_not_logged_in.jsonl"));
        let r = p.last_result().expect("result event");
        assert_eq!(
            r.error_message.as_deref(),
            Some("Not logged in · Please run /login")
        );
        assert_eq!(
            p.terminal_api_error().and_then(|e| e.category.as_deref()),
            Some("authentication_failed")
        );
    }

    #[test]
    fn usage_limit_carries_the_reset_time() {
        let p = parse(include_str!("../tests/fixtures/usage_limit.jsonl"));
        let r = p.last_result().expect("result event");
        assert_eq!(r.is_error, Some(true));
        assert_eq!(r.api_error_status, Some(429));
        assert_eq!(
            p.terminal_api_error().and_then(|e| e.category.as_deref()),
            Some("rate_limit")
        );
        assert_eq!(p.rate_limit_resets_at(), Some(1_795_600_800));
        // The work done before the limit still counts.
        assert_eq!(p.turns(), Some(12));
    }

    #[test]
    fn max_turns_and_startup_failures_keep_their_error_subtype() {
        let p = parse(include_str!("../tests/fixtures/error_max_turns.jsonl"));
        let r = p.last_result().expect("result event");
        assert_eq!(r.subtype.as_deref(), Some("error_max_turns"));
        assert_eq!(r.terminal_reason.as_deref(), Some("max_turns"));
        assert_eq!(p.turns(), Some(200));
        assert_eq!(p.terminal_api_error(), None);

        let p = parse(include_str!("../tests/fixtures/startup_failure.jsonl"));
        let r = p.last_result().expect("result event");
        assert_eq!(r.subtype.as_deref(), Some("error_during_execution"));
        assert_eq!(r.startup_failure_reason.as_deref(), Some("bypass_root"));
        assert!(
            r.error_message
                .as_deref()
                .is_some_and(|m| m.contains("root/sudo"))
        );
    }

    #[test]
    fn a_recovered_api_error_is_not_reported() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"system","subtype":"api_retry","attempt":1,"error_status":529,"error":"overloaded"}"#);
        assert_eq!(
            p.terminal_api_error().and_then(|e| e.category.as_deref()),
            Some("overloaded")
        );
        p.feed(br#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1795600800}}"#);
        assert_eq!(p.rate_limit_resets_at(), Some(1_795_600_800));
        p.feed(br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#);
        assert_eq!(p.terminal_api_error(), None);
        assert_eq!(p.rate_limit_resets_at(), None);
    }

    #[test]
    fn rate_limit_event_tolerates_milliseconds_and_warnings() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1795600800000}}"#);
        assert_eq!(p.rate_limit_resets_at(), Some(1_795_600_800));
        p.feed(br#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1795600800}}"#);
        assert_eq!(p.rate_limit_resets_at(), None);
        p.feed(br#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#);
        assert_eq!(p.rate_limit_resets_at(), None);
        p.feed(br#"{"type":"rate_limit_event"}"#);
        assert_eq!(p.rate_limit_resets_at(), None);
    }

    #[test]
    fn several_result_events_sum_turns_and_keep_the_last_cost() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"result","subtype":"success","is_error":false,"num_turns":40,"duration_ms":1000,"duration_api_ms":800,"total_cost_usd":1.5}"#);
        p.feed(br#"{"type":"result","subtype":"success","is_error":false,"num_turns":0,"duration_ms":10,"duration_api_ms":0,"total_cost_usd":1.5}"#);
        p.feed(br#"{"type":"result","subtype":"success","is_error":false,"num_turns":2,"duration_ms":500,"duration_api_ms":400,"total_cost_usd":1.75}"#);
        assert_eq!(p.result_events(), 3);
        assert_eq!(p.turns(), Some(42));
        assert_eq!(p.duration_ms(), Some(1510));
        assert_eq!(p.duration_api_ms(), Some(1200));
        assert_eq!(p.last_result().and_then(|r| r.total_cost_usd), Some(1.75));
    }

    #[test]
    fn garbage_never_panics_and_is_counted() {
        let mut p = StreamParser::new();
        for line in [
            &b"not json at all"[..],
            b"{\"type\":\"result\",\"subtype\":",
            b"[1,2,3]",
            b"\"just a string\"",
            b"42",
            b"null",
            b"\xff\xfe\x00 binary",
            b"",
            b"   \r\n",
            b"{}",
            br#"{"type":7}"#,
            br#"{"type":"result"}"#,
            br#"{"type":"result","subtype":3,"is_error":"yes","num_turns":"many","total_cost_usd":"free","duration_ms":-5,"errors":"nope","api_error_status":99999}"#,
            br#"{"type":"assistant","error":{"nested":true}}"#,
            br#"{"type":"system","subtype":"api_retry","error":12,"error_status":"x"}"#,
            br#"{"type":"rate_limit_event","rate_limit_info":"none"}"#,
        ] {
            p.feed(line);
        }
        assert_eq!(p.unparsed_lines(), 7);
        assert_eq!(p.result_events(), 2);
        let r = p.last_result().expect("result event");
        assert_eq!(r.subtype, None);
        assert_eq!(r.is_error, None);
        assert_eq!(r.num_turns, None);
        assert_eq!(r.total_cost_usd, None);
        assert_eq!(r.duration_ms, None);
        assert_eq!(r.api_error_status, None);
        assert_eq!(r.error_message, None);
        assert_eq!(p.turns(), None);
    }

    #[test]
    fn fractional_and_float_numbers_are_accepted() {
        let mut p = StreamParser::new();
        p.feed(br#"{"type":"result","num_turns":3.0,"duration_ms":1234.6,"total_cost_usd":0}"#);
        let r = p.last_result().expect("result event");
        assert_eq!(r.num_turns, Some(3));
        assert_eq!(r.duration_ms, Some(1235));
        assert_eq!(r.total_cost_usd, Some(0.0));
    }

    #[test]
    fn long_error_text_is_cut() {
        let long = "x".repeat(5000);
        let mut p = StreamParser::new();
        p.feed(
            format!(r#"{{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["{long}"]}}"#)
                .as_bytes(),
        );
        let message = p
            .last_result()
            .and_then(|r| r.error_message.clone())
            .expect("message");
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS + 1);
    }
}

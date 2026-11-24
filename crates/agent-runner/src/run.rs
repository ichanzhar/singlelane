//! One agent run: start Claude Code, tee its output, and leave the outcome in `/out`.
//!
//! Whatever happens, the run ends with a status file: `done` or `blocked` written by the
//! agent, or `failed` written here together with `failure.txt` and `failure.json`. The host
//! reads those and decides what the failure means (SPEC 6.6, 6.8); this side only reports.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use serde_json::Value;
use singlelane_core::agent_out::{
    AgentStatus, EXIT_FAILED, EXIT_OK, EXIT_SETUP, EXIT_TIMEOUT, FAILURE_JSON_FILE,
    FAILURE_TEXT_FILE, FailureReport, RESULT_FILE, RunResult, STATUS_FILE, STDERR_TAIL_LINES,
    TRANSCRIPT_FILE,
};
use singlelane_core::notify::format_duration;
use singlelane_core::policy::redact::Redactor;

use crate::config::Config;
use crate::git;
use crate::process::{ChildExit, Limits, LineSink, Shutdown, run_in_group};
use crate::stream::{StreamParser, truncate};

/// Longest stderr line kept for `failure.json`.
const MAX_STDERR_LINE_CHARS: usize = 2000;

/// Makes Claude Code report a refusal to start as a `result` event instead of stderr only
/// (<https://code.claude.com/docs/en/agent-sdk/typescript>, `startup_failure_reason`).
const STARTUP_FAILURE_RESULTS: (&str, &str) = ("CLAUDE_CODE_STARTUP_FAILURE_RESULTS", "1");

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What the status file says now.
    pub status: AgentStatus,
    /// The process exit code: 0 for done or blocked, non-zero for failed.
    pub exit_code: i32,
    /// Why the run failed, if it did.
    pub reason: Option<String>,
}

/// Runs the agent once. Never panics and never returns early without a status file, unless
/// the output directory itself cannot be written.
pub fn run(cfg: &Config, shutdown: &Shutdown) -> Outcome {
    let out = OutDir::new(&cfg.out_dir);
    if let Err(e) = out.prepare() {
        let reason = format!(
            "can't prepare the output directory {}: {e}",
            cfg.out_dir.display()
        );
        log(&reason);
        return Outcome {
            status: AgentStatus::Failed,
            exit_code: EXIT_SETUP,
            reason: Some(reason),
        };
    }

    let prompt = match fs::read_to_string(&cfg.prompt_file) {
        Ok(p) if !p.trim().is_empty() => p,
        Ok(_) => {
            return setup_failure(
                cfg,
                &out,
                format!("the prompt file {} is empty", cfg.prompt_file.display()),
            );
        }
        Err(e) => {
            return setup_failure(
                cfg,
                &out,
                format!(
                    "can't read the prompt file {}: {e}",
                    cfg.prompt_file.display()
                ),
            );
        }
    };
    if let Err(e) = git::configure(cfg) {
        return setup_failure(cfg, &out, e.to_string());
    }

    let tee = Arc::new(Mutex::new(Tee::new(&out, cfg.redactor.clone())));
    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
    let exit = run_in_group(
        &mut claude_command(cfg, &prompt),
        Limits {
            timeout: cfg.job_timeout,
            kill_grace: cfg.kill_grace,
        },
        shutdown,
        stdout_sink(&tee),
        stderr_sink(&stderr_tail, cfg.redactor.clone()),
    );
    let exit = match exit {
        Ok(exit) => exit,
        Err(e) => {
            return setup_failure(
                cfg,
                &out,
                format!("can't start {}: {e}", cfg.claude.display()),
            );
        }
    };
    if !exit.output_complete {
        log("Claude Code's output did not end after it exited; continuing with what was read");
    }

    let parser = lock(&tee).parser.clone();
    let stderr_tail: Vec<String> = lock(&stderr_tail).iter().cloned().collect();
    finish(cfg, &out, &exit, &parser, stderr_tail)
}

/// `claude -p <prompt> --dangerously-skip-permissions --model <m> --max-turns <n>
/// --output-format stream-json --verbose`, in the work dir (SPEC 7).
fn claude_command(cfg: &Config, prompt: &str) -> Command {
    let mut command = Command::new(&cfg.claude);
    command
        .arg("-p")
        .arg(prompt)
        .arg("--dangerously-skip-permissions")
        .arg("--model")
        .arg(&cfg.model)
        .arg("--max-turns")
        .arg(cfg.max_turns.to_string())
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .current_dir(&cfg.work_dir)
        .env(STARTUP_FAILURE_RESULTS.0, STARTUP_FAILURE_RESULTS.1)
        .envs(cfg.child_env.iter().map(|(k, v)| (k, v)));
    command
}

fn finish(
    cfg: &Config,
    out: &OutDir,
    exit: &ChildExit,
    parser: &StreamParser,
    stderr_tail: Vec<String>,
) -> Outcome {
    let last = parser.last_result();
    let result = RunResult {
        subtype: last.and_then(|r| r.subtype.clone()),
        is_error: last.and_then(|r| r.is_error),
        turns: parser.turns(),
        cost_usd: last.and_then(|r| r.total_cost_usd),
        duration_ms: parser.duration_ms(),
        duration_api_ms: parser.duration_api_ms(),
        terminal_reason: last.and_then(|r| r.terminal_reason.clone()),
        session_id: parser.session_id().map(str::to_string),
        claude_code_version: parser.claude_code_version().map(str::to_string),
        model: parser.model().map(str::to_string),
        result_events: parser.result_events(),
        wall_ms: u64::try_from(exit.wall.as_millis()).unwrap_or(u64::MAX),
        timed_out: exit.timed_out,
        exit_code: exit.exit_code,
    };
    out.write_json(RESULT_FILE, &result);

    let status_text = fs::read_to_string(out.path(STATUS_FILE)).ok();
    let status = status_text
        .as_deref()
        .map_or(AgentStatus::Failed, AgentStatus::parse);
    if !status.is_failed() {
        // The agent's own verdict stands, even if Claude Code was stopped afterwards (it can
        // sit waiting for a dev server it started in the background).
        if exit.timed_out {
            log(&format!(
                "the job timeout was reached after the agent had written status {:?}",
                status.as_str()
            ));
        }
        return Outcome {
            status,
            exit_code: EXIT_OK,
            reason: None,
        };
    }

    let api_error = parser.terminal_api_error();
    let message = last
        .and_then(|r| r.error_message.as_deref())
        .map(|m| cfg.redactor.redact(m));
    let mut report = FailureReport {
        reason: String::new(),
        exit_code: exit.exit_code,
        signal: exit.signal,
        timed_out: exit.timed_out,
        terminated: exit.terminated,
        setup_error: None,
        result_subtype: last.and_then(|r| r.subtype.clone()),
        result_is_error: last.and_then(|r| r.is_error),
        result_message: message,
        terminal_reason: last.and_then(|r| r.terminal_reason.clone()),
        startup_failure_reason: last.and_then(|r| r.startup_failure_reason.clone()),
        api_error_status: last
            .and_then(|r| r.api_error_status)
            .or_else(|| api_error.and_then(|e| e.status)),
        api_error_type: last
            .and_then(|r| r.api_error_type.clone())
            .or_else(|| api_error.and_then(|e| e.error_type.clone())),
        error_category: api_error.and_then(|e| e.category.clone()),
        rate_limit_resets_at: parser.rate_limit_resets_at(),
        stderr_tail,
    };
    report.reason =
        one_line(
            &cfg.redactor
                .redact(&failure_reason(cfg, exit, status_text.as_deref(), &report)),
        );
    let exit_code = if exit.timed_out {
        EXIT_TIMEOUT
    } else {
        EXIT_FAILED
    };
    write_failure(out, report, exit_code)
}

/// The human-readable reason for `failure.txt`.
fn failure_reason(
    cfg: &Config,
    exit: &ChildExit,
    status_text: Option<&str>,
    report: &FailureReport,
) -> String {
    if exit.timed_out {
        let how = if exit.killed {
            "killed after it ignored SIGTERM"
        } else {
            "stopped"
        };
        return format!(
            "Claude Code hit the job timeout of {} and was {how}",
            format_duration(cfg.job_timeout)
        );
    }
    if exit.terminated {
        return "agent-runner was told to stop before Claude Code finished".to_string();
    }
    if let Some(text) = status_text.map(str::trim).filter(|t| !t.is_empty()) {
        let shown: String = text.chars().take(80).collect();
        return format!("the agent wrote the status {shown:?}, which is neither done nor blocked");
    }
    let ended = match (exit.exit_code, exit.signal) {
        (Some(0), _) => "Claude Code finished".to_string(),
        (Some(code), _) => format!("Claude Code exited with code {code}"),
        (None, Some(signal)) => format!("Claude Code was killed by signal {signal}"),
        (None, None) => "Claude Code ended".to_string(),
    };
    let detail = match (&report.result_message, &report.result_subtype) {
        (Some(message), _) => format!(": {message}"),
        (None, Some(subtype)) if subtype != "success" => format!(" (result: {subtype})"),
        (None, None) if report.exit_code == Some(0) => " and reported no result".to_string(),
        _ => String::new(),
    };
    format!("{ended} without writing a status{detail}")
}

fn setup_failure(cfg: &Config, out: &OutDir, error: String) -> Outcome {
    let error = one_line(&cfg.redactor.redact(&error));
    let report = FailureReport {
        reason: format!("agent-runner could not start Claude Code: {error}"),
        setup_error: Some(error),
        ..FailureReport::default()
    };
    write_failure(out, report, EXIT_SETUP)
}

fn write_failure(out: &OutDir, report: FailureReport, exit_code: i32) -> Outcome {
    log(&format!("failed: {}", report.reason));
    out.write_text(STATUS_FILE, &format!("{}\n", AgentStatus::Failed.as_str()));
    out.write_text(FAILURE_TEXT_FILE, &format!("{}\n", report.reason));
    out.write_json(FAILURE_JSON_FILE, &report);
    Outcome {
        status: AgentStatus::Failed,
        exit_code,
        reason: Some(report.reason),
    }
}

/// The output directory (`/out`).
struct OutDir {
    dir: PathBuf,
}

impl OutDir {
    fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Creates the directory and removes what an earlier run may have left, so that a stale
    /// `done` can never be taken for this run's status.
    fn prepare(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        for name in [
            STATUS_FILE,
            RESULT_FILE,
            FAILURE_JSON_FILE,
            FAILURE_TEXT_FILE,
            TRANSCRIPT_FILE,
        ] {
            match fs::remove_file(self.path(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn write_text(&self, name: &str, text: &str) {
        if let Err(e) = fs::write(self.path(name), text) {
            log(&format!("can't write {}: {e}", self.path(name).display()));
        }
    }

    fn write_json(&self, name: &str, value: &impl Serialize) {
        match serde_json::to_string_pretty(value) {
            Ok(json) => self.write_text(name, &format!("{json}\n")),
            Err(e) => log(&format!("can't serialize {name}: {e}")),
        }
    }
}

/// Where Claude Code's stdout goes: the container log, the transcript and the parser.
struct Tee {
    parser: StreamParser,
    transcript: Option<File>,
    stdout_open: bool,
    redactor: Redactor,
}

impl Tee {
    fn new(out: &OutDir, redactor: Redactor) -> Self {
        let path = out.path(TRANSCRIPT_FILE);
        let transcript = match File::create(&path) {
            Ok(f) => Some(f),
            Err(e) => {
                log(&format!("can't create {}: {e}", path.display()));
                None
            }
        };
        Self {
            parser: StreamParser::new(),
            transcript,
            stdout_open: true,
            redactor,
        }
    }

    fn line(&mut self, line: &[u8]) {
        // Tool output can contain the Claude credential (the agent can run `env`): neither
        // the container log nor the transcript may carry it (G8).
        let redacted = match serde_json::from_slice::<Value>(line.trim_ascii()) {
            Ok(mut event) => {
                self.parser.feed_event(&event);
                redact_event(&self.redactor, &mut event, line)
            }
            Err(_) => {
                if !line.trim_ascii().is_empty() {
                    self.parser.feed_unparsed();
                }
                redact_line(&self.redactor, line)
            }
        };
        if self.stdout_open {
            let mut stdout = io::stdout().lock();
            if stdout
                .write_all(&redacted)
                .and_then(|()| stdout.flush())
                .is_err()
            {
                self.stdout_open = false;
            }
        }
        if let Some(file) = self.transcript.as_mut()
            && let Err(e) = file.write_all(&redacted)
        {
            log(&format!("can't write the transcript, giving up on it: {e}"));
            self.transcript = None;
        }
    }
}

fn stdout_sink(tee: &Arc<Mutex<Tee>>) -> LineSink {
    let tee = Arc::clone(tee);
    Box::new(move |line: &[u8]| lock(&tee).line(line))
}

fn stderr_sink(tail: &Arc<Mutex<VecDeque<String>>>, redactor: Redactor) -> LineSink {
    let tail = Arc::clone(tail);
    Box::new(move |line: &[u8]| {
        let redacted = redact_line(&redactor, line);
        {
            let mut stderr = io::stderr().lock();
            // Nothing useful can be done if the container log is gone.
            let _ = stderr.write_all(&redacted);
        }
        let text = String::from_utf8_lossy(&redacted);
        let text = text.trim_end_matches(['\n', '\r']);
        let mut kept: String = text.chars().take(MAX_STDERR_LINE_CHARS).collect();
        if text.chars().count() > MAX_STDERR_LINE_CHARS {
            kept.push('…');
        }
        let mut tail = lock(&tail);
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(kept);
    })
}

/// Redacts one stream-json event. A line with nothing to redact is passed on byte for byte.
/// Otherwise the strings inside the event are redacted one by one and the event is written
/// again, so the line stays valid JSON (redacting the raw text could swallow a closing quote).
fn redact_event(redactor: &Redactor, event: &mut Value, line: &[u8]) -> Vec<u8> {
    if !redact_strings(redactor, event) {
        return line.to_vec();
    }
    let mut text = event.to_string();
    if keys_contain_secret(redactor, event) {
        // Never seen in practice. Redacting the text can cost the line its JSON shape, which
        // is the lesser evil.
        text = redactor.redact(&text);
    }
    let ending_at = line
        .iter()
        .rposition(|b| !matches!(b, b'\n' | b'\r'))
        .map_or(0, |i| i + 1);
    let mut out = text.into_bytes();
    out.extend_from_slice(&line[ending_at..]);
    out
}

/// Redacts every string value in `value`; returns whether anything changed.
fn redact_strings(redactor: &Redactor, value: &mut Value) -> bool {
    match value {
        Value::String(s) => {
            let redacted = redactor.redact(s);
            let changed = redacted != *s;
            if changed {
                *s = redacted;
            }
            changed
        }
        // No short-circuit: every string must be visited.
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= redact_strings(redactor, item);
            }
            changed
        }
        Value::Object(map) => {
            let mut changed = false;
            for item in map.values_mut() {
                changed |= redact_strings(redactor, item);
            }
            changed
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn keys_contain_secret(redactor: &Redactor, value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(|v| keys_contain_secret(redactor, v)),
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| redactor.contains_secret(k) || keys_contain_secret(redactor, v)),
        _ => false,
    }
}

/// Redacts one line of plain text, keeping its line ending.
fn redact_line(redactor: &Redactor, line: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(line);
    let body = text.trim_end_matches(['\n', '\r']);
    let ending = &text[body.len()..];
    let mut redacted = redactor.redact(body);
    redacted.push_str(ending);
    redacted.into_bytes()
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&joined)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means a pump thread panicked; what it collected is still good.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// agent-runner's own messages go to stderr; stdout carries Claude Code's stream only.
pub(crate) fn log(message: &str) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "agent-runner: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_line_keeps_the_line_ending() {
        let key = format!("sk-ant-{}", "a".repeat(40));
        let r = Redactor::new();
        let line = format!("{{\"text\":\"ANTHROPIC key {key}\"}}\r\n");
        let out = String::from_utf8(redact_line(&r, line.as_bytes())).expect("utf8");
        assert!(!out.contains(&key));
        assert!(out.ends_with("\"}\r\n"), "{out:?}");
        assert_eq!(redact_line(&r, b"plain\n"), b"plain\n");
        assert_eq!(redact_line(&r, b"no newline"), b"no newline");
        assert_eq!(redact_line(&r, b"\n"), b"\n");
    }

    #[test]
    fn redact_event_keeps_json_valid_and_untouched_lines_identical() {
        let key = format!("sk-ant-{}", "a".repeat(40));
        let r = Redactor::with_secrets([key.clone()]);

        let clean = b"{\"type\":\"user\",  \"z\":1,\"a\":[\"spaced  oddly\"]}\n";
        let mut event: Value = serde_json::from_slice(clean).expect("json");
        assert_eq!(redact_event(&r, &mut event, clean), clean);

        // The env-style pattern runs to the next whitespace: on raw JSON text it would eat
        // the closing quote and brackets.
        let line = format!(
            "{{\"type\":\"user\",\"content\":[{{\"text\":\"ANTHROPIC_API_KEY={key}\\nHOME=/home/agent\"}}],\"n\":2}}\r\n"
        );
        let mut event: Value = serde_json::from_str(line.trim()).expect("json");
        let out = String::from_utf8(redact_event(&r, &mut event, line.as_bytes())).expect("utf8");
        assert!(!out.contains(&key));
        assert!(out.ends_with("\r\n"));
        let parsed: Value = serde_json::from_str(out.trim()).expect("still JSON");
        assert_eq!(
            parsed["content"][0]["text"],
            "ANTHROPIC_API_KEY=[REDACTED]\nHOME=/home/agent"
        );
        assert_eq!(parsed["n"], 2);
    }

    #[test]
    fn redact_event_catches_a_secret_used_as_an_object_key() {
        let key = format!("sk-ant-{}", "b".repeat(40));
        let r = Redactor::with_secrets([key.clone()]);
        let line = format!("{{\"{key}\":\"value\",\"note\":\"token {key}\"}}\n");
        let mut event: Value = serde_json::from_str(line.trim()).expect("json");
        let out = String::from_utf8(redact_event(&r, &mut event, line.as_bytes())).expect("utf8");
        assert!(!out.contains(&key), "{out}");
    }

    #[test]
    fn redact_line_survives_invalid_utf8() {
        let out = redact_line(&Redactor::new(), b"\xff\xfeabc\n");
        assert!(out.ends_with(b"abc\n"));
    }

    #[test]
    fn one_line_flattens_and_cuts() {
        assert_eq!(one_line("a\n  b\tc\r\n"), "a b c");
        assert!(one_line(&"word ".repeat(1000)).chars().count() <= 2001);
    }
}

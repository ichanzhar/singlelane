//! The `agent-runner` binary itself: arguments, exit codes, signals and what reaches the
//! container log.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Job, agent_runner, classify_report, fake_claude, fixture};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use singlelane_core::agent_out::FailureReport;
use singlelane_core::failure::FailureClass;

/// `agent-runner` for a scratch job, with a clean environment.
fn runner(job: &Job) -> Command {
    let root = job.root();
    let mut command = Command::new(agent_runner());
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("AGENT_RUNNER_CLAUDE", fake_claude())
        .env("AGENT_RUNNER_WORK_DIR", root.join("work"))
        .env("AGENT_RUNNER_PROMPT", root.join("task/prompt.md"))
        .env("AGENT_RUNNER_OUT_DIR", root.join("out"))
        .envs(Job::base_env(root))
        .stdin(Stdio::null());
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn failure(job: &Job) -> FailureReport {
    serde_json::from_str(&job.read("failure.json")).expect("failure.json parses")
}

#[test]
fn version_flag_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let output = Command::new(agent_runner())
            .arg(flag)
            .output()
            .expect("run");
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            stdout(&output),
            format!("agent-runner {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn help_and_unknown_arguments() {
    let output = Command::new(agent_runner())
        .arg("--help")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("--smoke-test"));

    for args in [&["--frobnicate"][..], &["--version", "extra"][..]] {
        let output = Command::new(agent_runner())
            .args(args)
            .output()
            .expect("run");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("unknown arguments"));
        assert!(stdout(&output).is_empty());
    }
}

#[test]
fn exit_code_is_zero_for_done_and_blocked() {
    for status in ["done", "blocked"] {
        let job = Job::new();
        let output = runner(&job)
            .env("FAKE_CLAUDE_REPLAY", fixture("success.jsonl"))
            .env("FAKE_CLAUDE_STATUS", status)
            .output()
            .expect("run");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{status}: {}",
            stderr(&output)
        );
        assert_eq!(job.read("status"), status);
        // stdout is Claude Code's stream and nothing else.
        let fixture = std::fs::read_to_string(fixture("success.jsonl")).expect("fixture");
        assert_eq!(stdout(&output), fixture);
        assert_eq!(job.read("transcript.jsonl"), fixture);
    }
}

#[test]
fn exit_code_is_non_zero_for_failed() {
    let job = Job::new();
    let output = runner(&job)
        .env("FAKE_CLAUDE_REPLAY", fixture("auth_invalid_api_key.jsonl"))
        .env("FAKE_CLAUDE_EXIT", "1")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(job.read("status"), "failed\n");
    assert!(stderr(&output).contains("agent-runner: failed: Claude Code exited with code 1"));
    assert_eq!(
        classify_report(&failure(&job)).class,
        FailureClass::ClaudeAuth
    );
}

#[test]
fn job_timeout_from_the_environment_gives_exit_code_124() {
    let job = Job::new();
    let started = Instant::now();
    let output = runner(&job)
        .env("JOB_TIMEOUT", "1s")
        .env("AGENT_RUNNER_KILL_GRACE", "20s")
        .env("FAKE_CLAUDE_SLEEP_MS", "60000")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(124));
    assert!(started.elapsed() < Duration::from_secs(15));
    let failure = failure(&job);
    assert!(failure.timed_out);
    assert_eq!(
        failure.reason,
        "Claude Code hit the job timeout of 1s and was stopped"
    );
}

#[test]
fn a_bad_environment_still_leaves_a_failed_status() {
    let job = Job::new();
    let output = runner(&job).env("MAX_TURNS", "lots").output().expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("MAX_TURNS"));
    assert_eq!(job.read("status"), "failed\n");
    assert!(
        job.read("failure.txt")
            .contains("invalid environment: MAX_TURNS")
    );
    assert!(
        failure(&job)
            .setup_error
            .is_some_and(|e| e.contains("MAX_TURNS"))
    );
}

#[test]
fn sigterm_to_agent_runner_stops_claude_and_is_reported() {
    let job = Job::new();
    let background = job.root().join("background.pid");
    let mut child = runner(&job)
        .env("FAKE_CLAUDE_SLEEP_MS", "60000")
        .env("FAKE_CLAUDE_BACKGROUND", &background)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    // Wait until Claude Code is up (it writes the pid file just before it sleeps).
    let deadline = Instant::now() + Duration::from_secs(20);
    while !background.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(background.exists(), "the fake claude never started");
    std::thread::sleep(Duration::from_millis(100));

    kill_process(Pid::from_child(&child), Signal::TERM).expect("send SIGTERM");
    let status = child.wait().expect("wait");

    assert_eq!(status.code(), Some(1), "handled, not killed by the signal");
    let failure = failure(&job);
    assert!(failure.terminated);
    assert_eq!(job.read("status"), "failed\n");
    let pid: u32 = std::fs::read_to_string(&background)
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid");
    assert!(common::wait_until_dead(pid, Duration::from_secs(5)));
}

/// G8: the Claude credential never reaches the container log, the transcript or the failure
/// files, whether Claude Code prints it on stdout (a tool result after the agent ran `env`)
/// or on stderr.
#[test]
fn g8_claude_credential_is_redacted_from_logs_transcript_and_failure_files() {
    // Built at run time: neither value is a real credential, and no secret-shaped literal
    // sits in the repository.
    let api_key = format!("sk-ant-api03-{}", "x".repeat(48));
    let oauth_token = format!("opaque-oauth-token-{}", "y".repeat(24));

    let tool_result = json!({
        "type": "user",
        "message": {"role": "user", "content": [{
            "type": "tool_result",
            "tool_use_id": "toolu_placeholder",
            "content": format!("ANTHROPIC_API_KEY={api_key}\nCLAUDE_CODE_OAUTH_TOKEN={oauth_token}\nHOME=/home/agent"),
        }]},
    });
    let result = json!({
        "type": "result", "subtype": "error_during_execution", "is_error": true,
        "errors": [format!("request with token {oauth_token} was rejected")],
    });
    let job = Job::new();
    let output = runner(&job)
        .env("ANTHROPIC_API_KEY", &api_key)
        .env("CLAUDE_CODE_OAUTH_TOKEN", &oauth_token)
        .env("FAKE_CLAUDE_STDOUT", format!("{tool_result}\n{result}\n"))
        .env(
            "FAKE_CLAUDE_STDERR",
            format!("debug: x-api-key: {api_key}\nBearer {oauth_token}\n"),
        )
        .env("FAKE_CLAUDE_EXIT", "1")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));

    let sinks = [
        ("container stdout", stdout(&output)),
        ("container stderr", stderr(&output)),
        ("transcript.jsonl", job.read("transcript.jsonl")),
        ("failure.json", job.read("failure.json")),
        ("failure.txt", job.read("failure.txt")),
        ("result.json", job.read("result.json")),
    ];
    for (name, text) in &sinks {
        assert!(!text.contains(&api_key), "{name} leaks the API key");
        assert!(!text.contains(&oauth_token), "{name} leaks the OAuth token");
    }
    // Redacted, not dropped: the lines are still there and still JSON.
    let transcript = &sinks[2].1;
    assert_eq!(transcript.lines().count(), 2);
    for line in transcript.lines() {
        let event: Value = serde_json::from_str(line).expect("still valid JSON");
        assert!(event.to_string().contains("[REDACTED]"));
    }
    assert!(sinks[3].1.contains("[REDACTED]"));
    assert_eq!(failure(&job).stderr_tail.len(), 2);
}

// ---- --smoke-test ----

fn smoke_home(root: &Path, with_mcp: bool) {
    std::fs::create_dir_all(root.join(".claude/agents")).expect("mkdir");
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    std::fs::copy(
        assets.join("settings.json"),
        root.join(".claude/settings.json"),
    )
    .expect("copy settings");
    std::fs::copy(
        assets.join("agents/code-reviewer.md"),
        root.join(".claude/agents/code-reviewer.md"),
    )
    .expect("copy agent");
    if with_mcp {
        let config = json!({"mcpServers": {"playwright": {
            "type": "stdio",
            "command": fake_claude(),
            "args": ["--fake-mcp", "--browser", "chromium", "--headless", "--isolated"],
            "env": {},
        }}});
        std::fs::write(root.join(".claude.json"), config.to_string()).expect("write");
    }
}

fn smoke(home: &Path) -> Output {
    Command::new(agent_runner())
        .arg("--smoke-test")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("AGENT_RUNNER_CLAUDE", fake_claude())
        .stdin(Stdio::null())
        .output()
        .expect("run")
}

#[test]
fn smoke_test_passes_and_ends_with_a_json_report() {
    let home = tempfile::tempdir().expect("tempdir");
    smoke_home(home.path(), true);
    let output = smoke(home.path());
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));

    let out = stdout(&output);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert_eq!(lines[0], "PASS claude: 9.9.9 (Claude Code)");
    assert!(lines[..5].iter().all(|l| l.starts_with("PASS ")), "{out}");
    let report: Value = serde_json::from_str(lines[5]).expect("last line is JSON");
    assert_eq!(report["ok"], true);
    assert_eq!(report["claude_code_version"], "9.9.9");
    assert_eq!(report["agent_runner_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["checks"].as_array().map(Vec::len), Some(5));
}

#[test]
fn smoke_test_fails_with_exit_code_1_and_names_the_failing_step() {
    let home = tempfile::tempdir().expect("tempdir");
    smoke_home(home.path(), false);
    let output = smoke(home.path());
    assert_eq!(output.status.code(), Some(1));

    let out = stdout(&output);
    assert!(out.contains("PASS claude: "), "{out}");
    assert!(out.contains("FAIL mcp-config: "), "{out}");
    assert!(out.contains("FAIL chromium: not tried"), "{out}");
    let report: Value =
        serde_json::from_str(out.lines().last().expect("output")).expect("last line is JSON");
    assert_eq!(report["ok"], false);
}

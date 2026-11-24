//! One agent run from start to the files in `/out`, with the fake `claude` (SPEC 7).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

mod common;

use std::time::{Duration, Instant};

use agent_runner::process::Shutdown;
use agent_runner::run::run;
use common::{Job, PROMPT, path_str, read_json, wait_until_dead};
use singlelane_core::agent_out::{
    AgentStatus, EXIT_FAILED, EXIT_OK, EXIT_SETUP, EXIT_TIMEOUT, STDERR_TAIL_LINES,
};
use singlelane_core::failure::FailureClass;

// ---- done and blocked ----

#[test]
fn done_run_exits_zero_and_writes_result_and_transcript() {
    let mut job = Job::new();
    job.replay("success.jsonl").status("done\n");
    let outcome = job.run();

    assert_eq!(outcome.status, AgentStatus::Done);
    assert_eq!(outcome.exit_code, EXIT_OK);
    assert_eq!(outcome.reason, None);
    assert_eq!(job.read("status"), "done\n");

    let result = job.result();
    assert_eq!(result.subtype.as_deref(), Some("success"));
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.turns, Some(3));
    assert_eq!(result.cost_usd, Some(0.4127));
    assert_eq!(result.duration_ms, Some(48_210));
    assert_eq!(result.duration_api_ms, Some(41_530));
    assert_eq!(result.terminal_reason.as_deref(), Some("completed"));
    assert_eq!(result.claude_code_version.as_deref(), Some("2.1.331"));
    assert_eq!(result.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(result.result_events, 1);
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);

    // The transcript is the stream, line for line.
    let fixture = std::fs::read_to_string(common::fixture("success.jsonl")).expect("fixture");
    assert_eq!(job.read("transcript.jsonl"), fixture);

    assert!(!job.exists("failure.json"));
    assert!(!job.exists("failure.txt"));
}

#[test]
fn blocked_run_exits_zero_and_leaves_the_question_alone() {
    let mut job = Job::new();
    job.replay("success.jsonl").status("blocked");
    std::fs::create_dir_all(job.out("")).expect("mkdir out");
    let outcome = job.run();

    assert_eq!(outcome.status, AgentStatus::Blocked);
    assert_eq!(outcome.exit_code, EXIT_OK);
    assert_eq!(job.read("status"), "blocked");
    assert!(!job.exists("failure.json"));
}

#[test]
fn done_stands_even_if_claude_exits_non_zero_afterwards() {
    let mut job = Job::new();
    job.replay("success.jsonl")
        .status("done")
        .env("FAKE_CLAUDE_EXIT", "1");
    let outcome = job.run();
    assert_eq!(outcome.status, AgentStatus::Done);
    assert_eq!(outcome.exit_code, EXIT_OK);
    assert_eq!(job.result().exit_code, Some(1));
    assert!(!job.exists("failure.json"));
}

// ---- how Claude Code is started ----

#[test]
fn claude_is_started_as_specified_in_its_own_process_group() {
    let mut job = Job::new();
    job.cfg.model = "claude-opus-5-5".to_string();
    job.cfg.max_turns = 42;
    let record = job.record();
    job.status("done");
    assert_eq!(job.run().exit_code, EXIT_OK);

    let record = read_json(&record);
    let args: Vec<&str> = record["args"]
        .as_array()
        .expect("args")
        .iter()
        .map(|a| a.as_str().expect("string arg"))
        .collect();
    assert_eq!(
        args,
        [
            "-p",
            PROMPT,
            "--dangerously-skip-permissions",
            "--model",
            "claude-opus-5-5",
            "--max-turns",
            "42",
            "--output-format",
            "stream-json",
            "--verbose",
        ]
    );
    let work = job.root().join("work").canonicalize().expect("work dir");
    assert_eq!(record["cwd"].as_str(), Some(path_str(&work).as_str()));
    // Its own process group: the group id is its own pid, not the test's.
    assert_eq!(record["pgid"], record["pid"]);
    assert_ne!(
        record["pgid"].as_i64(),
        Some(i64::from(
            rustix::process::getpgid(None)
                .expect("own pgid")
                .as_raw_nonzero()
                .get()
        ))
    );
    // Claude Code waits for piped stdin before starting; it gets /dev/null.
    assert_eq!(record["stdin_closed"], true);
    assert_eq!(record["startup_failure_results"], "1");
}

#[test]
fn git_identity_and_safe_directory_are_set_once() {
    let mut job = Job::new();
    job.cfg.git_name = "example-app[bot]".to_string();
    job.cfg.git_email = "123456+example-app[bot]@users.noreply.github.com".to_string();
    job.status("done");
    assert_eq!(job.run().exit_code, EXIT_OK);
    assert_eq!(
        job.run().exit_code,
        EXIT_OK,
        "a second run in the same home"
    );

    let config = std::fs::read_to_string(job.root().join("gitconfig")).expect("git config");
    assert!(config.contains("name = example-app[bot]"), "{config}");
    assert!(
        config.contains("email = 123456+example-app[bot]@users.noreply.github.com"),
        "{config}"
    );
    let work = path_str(&job.root().join("work"));
    assert_eq!(
        config.matches(&format!("directory = {work}")).count(),
        1,
        "{config}"
    );
}

// ---- failures ----

#[test]
fn missing_status_becomes_failed_with_a_reason_and_the_facts() {
    let mut job = Job::new();
    job.replay("success.jsonl");
    let outcome = job.run();

    assert_eq!(outcome.status, AgentStatus::Failed);
    assert_eq!(outcome.exit_code, EXIT_FAILED);
    assert_eq!(job.read("status"), "failed\n");
    let reason = job.read("failure.txt");
    assert_eq!(reason, "Claude Code finished without writing a status\n");
    assert_eq!(outcome.reason.as_deref(), Some(reason.trim_end()));

    let failure = job.failure();
    assert_eq!(failure.reason, reason.trim_end());
    assert_eq!(failure.exit_code, Some(0));
    assert_eq!(failure.signal, None);
    assert!(!failure.timed_out && !failure.terminated);
    assert_eq!(failure.result_subtype.as_deref(), Some("success"));
    assert_eq!(failure.result_is_error, Some(false));
    assert_eq!(failure.result_message, None);
    assert_eq!(failure.error_category, None);
    assert_eq!(failure.api_error_status, None);
    assert_eq!(job.classify().class, FailureClass::AgentFailed);
    // The run statistics are there for failed runs too.
    assert_eq!(job.result().turns, Some(3));
}

#[test]
fn silent_exit_without_status_or_result_is_failed() {
    let job = Job::new();
    let outcome = job.run();
    assert_eq!(outcome.exit_code, EXIT_FAILED);
    assert_eq!(
        job.read("failure.txt"),
        "Claude Code finished without writing a status and reported no result\n"
    );
    let result = job.result();
    assert_eq!(result.result_events, 0);
    assert_eq!(result.subtype, None);
    assert_eq!(result.turns, None);
    assert_eq!(job.classify().class, FailureClass::AgentFailed);
}

#[test]
fn an_unknown_status_is_failed_and_named_in_the_reason() {
    let mut job = Job::new();
    job.replay("success.jsonl").status("almost done");
    let outcome = job.run();
    assert_eq!(outcome.exit_code, EXIT_FAILED);
    assert_eq!(job.read("status"), "failed\n");
    assert!(
        job.read("failure.txt").contains("\"almost done\""),
        "{}",
        job.read("failure.txt")
    );
}

#[test]
fn a_stale_status_from_an_earlier_run_does_not_count() {
    let job = Job::new();
    std::fs::create_dir_all(job.out("")).expect("mkdir out");
    for name in ["status", "result.json", "failure.json", "failure.txt"] {
        std::fs::write(job.out(name), "done").expect("write stale file");
    }
    let outcome = job.run();
    assert_eq!(outcome.status, AgentStatus::Failed);
    assert_eq!(job.read("status"), "failed\n");
    assert_eq!(job.failure().exit_code, Some(0));
}

/// SPEC 6.8: the Claude Code authentication failures, as fixtures, end as `claude_auth`.
#[test]
fn auth_failures_are_reported_so_that_the_host_classifies_claude_auth() {
    for (fixture, message) in [
        (
            "auth_invalid_api_key.jsonl",
            "Invalid API key · Fix external API key",
        ),
        (
            "auth_not_logged_in.jsonl",
            "Not logged in · Please run /login",
        ),
        (
            "auth_oauth_expired.jsonl",
            "Failed to authenticate. API Error: 401",
        ),
    ] {
        let mut job = Job::new();
        // Claude Code exits with 1 after an API failure.
        job.replay(fixture).env("FAKE_CLAUDE_EXIT", "1");
        let outcome = job.run();
        assert_eq!(outcome.exit_code, EXIT_FAILED, "{fixture}");

        let failure = job.failure();
        assert_eq!(failure.exit_code, Some(1), "{fixture}");
        // "success" with is_error: the subtype says nothing here.
        assert_eq!(failure.result_subtype.as_deref(), Some("success"));
        assert_eq!(failure.result_is_error, Some(true));
        assert_eq!(failure.terminal_reason.as_deref(), Some("api_error"));
        assert_eq!(failure.api_error_status, Some(401), "{fixture}");
        assert_eq!(
            failure.api_error_type.as_deref(),
            Some("authentication_error")
        );
        assert_eq!(
            failure.error_category.as_deref(),
            Some("authentication_failed")
        );
        let reported = failure.result_message.as_deref().unwrap_or_default();
        assert!(reported.starts_with(message), "{fixture}: {reported}");
        assert!(
            failure
                .reason
                .starts_with("Claude Code exited with code 1 without writing a status: "),
            "{}",
            failure.reason
        );

        let class = job.classify();
        assert_eq!(class.class, FailureClass::ClaudeAuth, "{fixture}");
        assert!(class.class.is_fatal());
    }
}

/// SPEC 6.8: a usage limit ends as `claude_usage_limit`, with the reset time.
#[test]
fn usage_limit_is_reported_with_its_reset_time() {
    let mut job = Job::new();
    job.replay("usage_limit.jsonl").env("FAKE_CLAUDE_EXIT", "1");
    assert_eq!(job.run().exit_code, EXIT_FAILED);

    let failure = job.failure();
    assert_eq!(failure.api_error_status, Some(429));
    assert_eq!(failure.error_category.as_deref(), Some("rate_limit"));
    assert_eq!(failure.rate_limit_resets_at, Some(1_795_600_800));
    assert_eq!(
        failure.result_message.as_deref(),
        Some("You've hit your session limit · resets 3:45pm")
    );

    let class = job.classify();
    assert_eq!(class.class, FailureClass::ClaudeUsageLimit);
    assert_eq!(class.resets_at.map(|t| t.as_second()), Some(1_795_600_800));
    // Turns and cost up to the limit are kept.
    let result = job.result();
    assert_eq!(result.turns, Some(12));
    assert_eq!(result.cost_usd, Some(2.9031));
}

#[test]
fn max_turns_and_startup_failures_are_agent_failures() {
    let mut job = Job::new();
    job.replay("error_max_turns.jsonl")
        .env("FAKE_CLAUDE_EXIT", "1");
    assert_eq!(job.run().exit_code, EXIT_FAILED);
    let failure = job.failure();
    assert_eq!(failure.result_subtype.as_deref(), Some("error_max_turns"));
    assert_eq!(failure.terminal_reason.as_deref(), Some("max_turns"));
    assert_eq!(
        failure.reason,
        "Claude Code exited with code 1 without writing a status (result: error_max_turns)"
    );
    let class = job.classify();
    assert_eq!(class.class, FailureClass::AgentFailed);
    assert!(class.reason.contains("error_max_turns"), "{}", class.reason);

    let mut job = Job::new();
    job.replay("startup_failure.jsonl")
        .env("FAKE_CLAUDE_EXIT", "1");
    assert_eq!(job.run().exit_code, EXIT_FAILED);
    let failure = job.failure();
    assert_eq!(
        failure.startup_failure_reason.as_deref(),
        Some("bypass_root")
    );
    assert!(
        failure
            .result_message
            .as_deref()
            .is_some_and(|m| m.contains("root/sudo"))
    );
    assert_eq!(job.classify().class, FailureClass::AgentFailed);
}

#[test]
fn output_that_is_not_stream_json_never_fails_the_run() {
    let mut job = Job::new();
    job.env(
        "FAKE_CLAUDE_STDOUT",
        "Warning: something chatty\n{\"type\":\"result\",\"subtype\":\n\u{1b}[31mred\u{1b}[0m\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":\"seven\"}\nlast line without newline",
    )
    .status("done");
    let outcome = job.run();
    assert_eq!(outcome.exit_code, EXIT_OK);
    let result = job.result();
    assert_eq!(result.subtype.as_deref(), Some("success"));
    assert_eq!(
        result.turns, None,
        "a malformed field is dropped, not fatal"
    );
    assert_eq!(result.result_events, 1);
    assert!(
        job.read("transcript.jsonl")
            .ends_with("last line without newline")
    );
}

#[test]
fn stderr_tail_keeps_the_last_lines() {
    let mut job = Job::new();
    job.env("FAKE_CLAUDE_STDERR_LINES", "80")
        .env("FAKE_CLAUDE_STDERR", "Error: something broke\n")
        .env("FAKE_CLAUDE_EXIT", "3");
    assert_eq!(job.run().exit_code, EXIT_FAILED);
    let failure = job.failure();
    assert_eq!(failure.exit_code, Some(3));
    assert_eq!(failure.stderr_tail.len(), STDERR_TAIL_LINES);
    assert_eq!(failure.stderr_tail[0], "stderr line 32");
    assert_eq!(
        failure.stderr_tail.last().map(String::as_str),
        Some("Error: something broke")
    );
    assert_eq!(
        failure.reason,
        "Claude Code exited with code 3 without writing a status"
    );
}

// ---- timeout ----

#[test]
fn job_timeout_stops_claude_and_reports_timed_out() {
    let mut job = Job::new();
    job.replay("success.jsonl")
        .env("FAKE_CLAUDE_SLEEP_MS", "60000")
        .timeout_ms(300, 20_000);
    let started = Instant::now();
    let outcome = job.run();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "SIGTERM ends it"
    );

    assert_eq!(outcome.status, AgentStatus::Failed);
    assert_eq!(outcome.exit_code, EXIT_TIMEOUT);
    assert_eq!(job.read("status"), "failed\n");
    let failure = job.failure();
    assert!(failure.timed_out);
    assert_eq!(failure.signal, Some(15));
    assert_eq!(failure.exit_code, None);
    assert_eq!(
        failure.reason,
        "Claude Code hit the job timeout of 0s and was stopped"
    );
    let result = job.result();
    assert!(result.timed_out);
    assert_eq!(result.exit_code, None);
    assert_eq!(
        result.turns,
        Some(3),
        "what was streamed before still counts"
    );

    let class = job.classify();
    assert_eq!(class.class, FailureClass::AgentFailed);
    assert_eq!(class.reason, "the agent hit the job timeout");
}

#[test]
fn sigkill_follows_when_sigterm_is_ignored_and_takes_the_whole_group() {
    let mut job = Job::new();
    let background = job.root().join("background.pid");
    job.env("FAKE_CLAUDE_IGNORE_TERM", "1")
        .env("FAKE_CLAUDE_BACKGROUND", &background)
        .env("FAKE_CLAUDE_SLEEP_MS", "60000")
        .timeout_ms(500, 400);
    let started = Instant::now();
    let outcome = job.run();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(900),
        "grace period: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");

    assert_eq!(outcome.exit_code, EXIT_TIMEOUT);
    let failure = job.failure();
    assert!(failure.timed_out);
    assert_eq!(failure.signal, Some(9));
    assert!(
        failure.reason.contains("killed after it ignored SIGTERM"),
        "{}",
        failure.reason
    );

    // The child Claude Code left behind is in the same group and is gone too.
    let pid: u32 = std::fs::read_to_string(&background)
        .expect("background pid")
        .trim()
        .parse()
        .expect("pid");
    assert!(wait_until_dead(pid, Duration::from_secs(5)));
}

#[test]
fn timeout_after_the_agent_wrote_done_keeps_done() {
    let mut job = Job::new();
    job.replay("success.jsonl")
        .status("done")
        .env("FAKE_CLAUDE_SLEEP_MS", "60000")
        .timeout_ms(500, 20_000);
    let outcome = job.run();
    assert_eq!(outcome.status, AgentStatus::Done);
    assert_eq!(outcome.exit_code, EXIT_OK);
    assert_eq!(job.read("status"), "done");
    assert!(job.result().timed_out);
    assert!(!job.exists("failure.json"));
}

#[test]
fn a_background_process_holding_stdout_does_not_hang_the_run() {
    let mut job = Job::new();
    let background = job.root().join("background.pid");
    job.replay("success.jsonl")
        .status("done")
        .env("FAKE_CLAUDE_BACKGROUND", &background);
    let started = Instant::now();
    let outcome = job.run();
    assert_eq!(outcome.exit_code, EXIT_OK);
    assert!(started.elapsed() < Duration::from_secs(4), "no drain wait");
    let pid: u32 = std::fs::read_to_string(&background)
        .expect("background pid")
        .trim()
        .parse()
        .expect("pid");
    assert!(wait_until_dead(pid, Duration::from_secs(5)));
}

#[test]
fn a_stop_request_takes_claude_down_and_is_reported() {
    let mut job = Job::new();
    job.env("FAKE_CLAUDE_SLEEP_MS", "60000");
    let shutdown = Shutdown::new();
    let trigger = shutdown.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        trigger.request();
    });
    let outcome = run(&job.cfg, &shutdown);
    handle.join().expect("join");

    assert_eq!(outcome.exit_code, EXIT_FAILED);
    let failure = job.failure();
    assert!(failure.terminated);
    assert!(!failure.timed_out);
    assert_eq!(
        failure.reason,
        "agent-runner was told to stop before Claude Code finished"
    );
}

// ---- setup failures ----

#[test]
fn a_missing_claude_binary_is_a_setup_failure() {
    let mut job = Job::new();
    job.cfg.claude = job.root().join("no-such-claude");
    let outcome = job.run();
    assert_eq!(outcome.exit_code, EXIT_SETUP);
    assert_eq!(job.read("status"), "failed\n");
    let failure = job.failure();
    assert!(
        failure
            .setup_error
            .as_deref()
            .is_some_and(|e| e.contains("can't start") && e.contains("no-such-claude")),
        "{failure:?}"
    );
    assert!(
        failure
            .reason
            .starts_with("agent-runner could not start Claude Code: ")
    );
    assert_eq!(failure.exit_code, None);
    // Not a network problem, not a credential problem: unknown errors stay agent_failed.
    assert_eq!(job.classify().class, FailureClass::AgentFailed);
}

#[test]
fn a_missing_or_empty_prompt_is_a_setup_failure() {
    let job = Job::new();
    std::fs::write(&job.cfg.prompt_file, "  \n").expect("write");
    assert_eq!(job.run().exit_code, EXIT_SETUP);
    assert!(job.read("failure.txt").contains("is empty"));

    std::fs::remove_file(&job.cfg.prompt_file).expect("remove");
    assert_eq!(job.run().exit_code, EXIT_SETUP);
    assert!(
        job.read("failure.txt")
            .contains("can't read the prompt file")
    );
    assert_eq!(job.read("status"), "failed\n");
}

#[test]
fn a_failing_git_setup_is_a_setup_failure() {
    let mut job = Job::new();
    job.cfg.git = job.root().join("no-such-git");
    assert_eq!(job.run().exit_code, EXIT_SETUP);
    assert!(job.read("failure.txt").contains("no-such-git"));

    let mut job = Job::new();
    job.cfg.git_name = "--global".to_string();
    assert_eq!(job.run().exit_code, EXIT_SETUP);
    assert!(job.read("failure.txt").contains("must not start with '-'"));
}

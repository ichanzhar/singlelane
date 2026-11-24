//! agent-runner: the entrypoint of the agent container (SPEC 7).
//!
//! - no arguments: run Claude Code on `/task/prompt.md` in `/work`, report in `/out`;
//! - `--version`: print the version;
//! - `--smoke-test`: check the image without calling the Claude API (SPEC 7.1).

use std::io::{self, Write};
use std::process::ExitCode;

use agent_runner::config::{self, Config};
use agent_runner::process::Shutdown;
use agent_runner::smoke::{SmokeConfig, smoke_test};
use agent_runner::{run, version_line};
use anyhow::{Context, Result};
use singlelane_core::agent_out::{
    AgentStatus, EXIT_FAILED, EXIT_OK, EXIT_SETUP, FAILURE_JSON_FILE, FAILURE_TEXT_FILE,
    FailureReport, STATUS_FILE,
};

const USAGE: &str = "\
Usage: agent-runner [--version | --smoke-test | --help]

Without arguments: runs Claude Code headless on /task/prompt.md in /work and leaves
status, result.json, transcript.jsonl (and failure.txt, failure.json) in /out.

Environment: CLAUDE_MODEL, MAX_TURNS, JOB_TIMEOUT, GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL,
and exactly one of ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN for Claude Code.

  --version      print the version
  --smoke-test   check claude, the Claude Code settings, the Playwright MCP config and
                 headless Chromium, without calling the Claude API
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => run_agent(),
        ["--version" | "-V"] => print(&version_line()),
        ["--help" | "-h"] => print(USAGE.trim_end()),
        ["--smoke-test"] => smoke(),
        _ => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(
                stderr,
                "agent-runner: unknown arguments {args:?}\n\n{USAGE}"
            );
            Ok(EXIT_SETUP)
        }
    };
    let code = code.unwrap_or_else(|e| {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "agent-runner: {e:#}");
        EXIT_SETUP
    });
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn print(text: &str) -> Result<i32> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{text}").context("can't write to stdout")?;
    Ok(EXIT_OK)
}

fn run_agent() -> Result<i32> {
    let lookup = |name: &str| std::env::var(name).ok();
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            // Leave a visible end state even for a broken environment.
            let reason = config::redactor(lookup).redact(&format!(
                "agent-runner could not start Claude Code: invalid environment: {e}"
            ));
            report_bad_environment(&config::out_dir(lookup), &reason);
            anyhow::bail!("{reason}");
        }
    };
    let shutdown = Shutdown::install().context("can't install the signal handlers")?;
    Ok(run::run(&cfg, &shutdown).exit_code)
}

fn report_bad_environment(out_dir: &std::path::Path, reason: &str) {
    let report = FailureReport {
        reason: reason.to_string(),
        setup_error: Some(reason.to_string()),
        ..FailureReport::default()
    };
    // Best effort: the caller reports the error on stderr and through the exit code as well.
    let _ = std::fs::create_dir_all(out_dir);
    let _ = std::fs::write(
        out_dir.join(STATUS_FILE),
        format!("{}\n", AgentStatus::Failed.as_str()),
    );
    let _ = std::fs::write(out_dir.join(FAILURE_TEXT_FILE), format!("{reason}\n"));
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        let _ = std::fs::write(out_dir.join(FAILURE_JSON_FILE), format!("{json}\n"));
    }
}

fn smoke() -> Result<i32> {
    let cfg = SmokeConfig::from_lookup(|name| std::env::var(name).ok());
    let report = smoke_test(&cfg);
    let mut stdout = io::stdout().lock();
    for line in report.lines() {
        writeln!(stdout, "{line}").context("can't write to stdout")?;
    }
    // The last line is machine-readable: `singlelane image-update` records the versions.
    let json = serde_json::to_string(&report).context("can't serialize the smoke test report")?;
    writeln!(stdout, "{json}").context("can't write to stdout")?;
    Ok(if report.ok { EXIT_OK } else { EXIT_FAILED })
}

//! A stand-in for the `claude` executable, used by agent-runner's tests only.
//!
//! It is a test fixture, not part of the product: the agent image is built with
//! `--bin agent-runner` and never contains it. Tests steer it with `FAKE_CLAUDE_*` variables:
//!
//! | Variable | Effect |
//! |---|---|
//! | `FAKE_CLAUDE_RECORD` | write the arguments, working directory, pid, process group and whether stdin is closed to this file, as JSON |
//! | `FAKE_CLAUDE_REPLAY` | print this file (a stream-json fixture) to stdout |
//! | `FAKE_CLAUDE_STDOUT` / `FAKE_CLAUDE_STDERR` | print this text |
//! | `FAKE_CLAUDE_STDERR_LINES` | print this many numbered lines to stderr |
//! | `FAKE_CLAUDE_STATUS_FILE` + `FAKE_CLAUDE_STATUS` | write the status file, as the agent would |
//! | `FAKE_CLAUDE_BACKGROUND` | start a long-running child that inherits stdout, write its pid to this file |
//! | `FAKE_CLAUDE_IGNORE_TERM` | ignore SIGTERM |
//! | `FAKE_CLAUDE_SLEEP_MS` | sleep before exiting |
//! | `FAKE_CLAUDE_EXIT` | the exit code |
//!
//! `--version` prints a version line. With `--fake-mcp` as the first argument it is a tiny MCP
//! server on stdio instead (`FAKE_MCP_MODE`: `ok`, `launch-error`, `silent`, `exit`).

use std::io::{self, BufRead, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if var("FAKE_CLAUDE_ROLE").as_deref() == Some("sleeper") {
        thread::sleep(Duration::from_secs(600));
        return ExitCode::SUCCESS;
    }
    let result = if args.first().map(String::as_str) == Some("--fake-mcp") {
        mcp_server(&args)
    } else if args.iter().any(|a| a == "--version") {
        version()
    } else {
        claude(&args)
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            let _ = writeln!(io::stderr().lock(), "fake-claude: {e}");
            ExitCode::from(70)
        }
    }
}

fn version() -> io::Result<u8> {
    let line = var("FAKE_CLAUDE_VERSION").unwrap_or_else(|| "9.9.9 (Claude Code)".to_string());
    writeln!(io::stdout().lock(), "{line}")?;
    Ok(exit_code())
}

fn exit_code() -> u8 {
    var("FAKE_CLAUDE_EXIT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn claude(args: &[String]) -> io::Result<u8> {
    if var("FAKE_CLAUDE_IGNORE_TERM").is_some() {
        // A registered handler replaces the default action: SIGTERM no longer ends the process.
        signal_hook::flag::register(
            signal_hook::consts::SIGTERM,
            Arc::new(AtomicBool::new(false)),
        )?;
    }
    if let Some(path) = var("FAKE_CLAUDE_RECORD") {
        let mut input = Vec::new();
        let stdin_closed = io::stdin().lock().read_to_end(&mut input).is_ok() && input.is_empty();
        let record = json!({
            "args": args,
            "cwd": std::env::current_dir()?,
            "pid": rustix::process::getpid().as_raw_nonzero().get(),
            "pgid": rustix::process::getpgid(None)?.as_raw_nonzero().get(),
            "stdin_closed": stdin_closed,
            "startup_failure_results": var("CLAUDE_CODE_STARTUP_FAILURE_RESULTS"),
        });
        std::fs::write(path, record.to_string())?;
    }
    {
        let mut stdout = io::stdout().lock();
        if let Some(path) = var("FAKE_CLAUDE_REPLAY") {
            stdout.write_all(&std::fs::read(path)?)?;
        }
        if let Some(text) = var("FAKE_CLAUDE_STDOUT") {
            stdout.write_all(text.as_bytes())?;
        }
        stdout.flush()?;
    }
    {
        let mut stderr = io::stderr().lock();
        if let Some(count) = var("FAKE_CLAUDE_STDERR_LINES").and_then(|v| v.parse::<u32>().ok()) {
            for i in 1..=count {
                writeln!(stderr, "stderr line {i}")?;
            }
        }
        if let Some(text) = var("FAKE_CLAUDE_STDERR") {
            stderr.write_all(text.as_bytes())?;
        }
    }
    if let (Some(path), Some(status)) = (var("FAKE_CLAUDE_STATUS_FILE"), var("FAKE_CLAUDE_STATUS"))
    {
        std::fs::write(path, status)?;
    }
    if let Some(path) = var("FAKE_CLAUDE_BACKGROUND") {
        // Same process group, inherited stdout: like a dev server the agent left running.
        let child = Command::new(std::env::current_exe()?)
            .env("FAKE_CLAUDE_ROLE", "sleeper")
            .stdin(Stdio::null())
            .spawn()?;
        std::fs::write(path, child.id().to_string())?;
    }
    if let Some(ms) = var("FAKE_CLAUDE_SLEEP_MS").and_then(|v| v.parse::<u64>().ok()) {
        // In slices, so that an ignored SIGTERM interrupting one sleep changes nothing.
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(exit_code())
}

/// Just enough of an MCP server for the smoke test: `initialize` and `tools/call`.
fn mcp_server(args: &[String]) -> io::Result<u8> {
    let mode = var("FAKE_MCP_MODE").unwrap_or_else(|| "ok".to_string());
    if let Some(path) = var("FAKE_MCP_RECORD") {
        let record = json!({"args": args, "cwd": std::env::current_dir()?});
        std::fs::write(path, record.to_string())?;
    }
    if mode == "exit" {
        return Ok(1);
    }
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue; // a notification
        };
        if mode == "silent" {
            continue;
        }
        let result = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "Playwright", "version": "0.0.0-fake"},
            }),
            Some("tools/call") if mode == "launch-error" => json!({
                "content": [{"type": "text", "text": "### Error\nError: Failed to launch chromium because executable doesn't exist at /opt/ms-playwright/chromium"}],
                "isError": true,
            }),
            Some("tools/call") => {
                let url = message
                    .pointer("/params/arguments/url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let title = url
                    .split_once("<title>")
                    .and_then(|(_, rest)| rest.split_once("</title>"))
                    .map_or("", |(title, _)| title);
                json!({"content": [{"type": "text", "text": format!("### Page\n- Page URL: {url}\n- Page Title: {title}")}]})
            }
            _ => json!({}),
        };
        // A notification first, as the real server sends them between responses.
        writeln!(
            stdout,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        )?;
        writeln!(
            stdout,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        )?;
        stdout.flush()?;
    }
    Ok(0)
}

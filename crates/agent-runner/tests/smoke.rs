//! `--smoke-test` against a fake home directory, a fake `claude` and a fake MCP server
//! (SPEC 7.1). The real check on the Pi starts the real Playwright MCP and Chromium.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use agent_runner::smoke::{Report, SmokeConfig, smoke_test};
use common::{fake_claude, path_str, read_json};
use serde_json::json;
use tempfile::TempDir;

/// A home directory laid out like the image's `/home/agent`.
fn home(mcp_mode: &str, args: &[&str]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".claude/agents")).expect("mkdir");
    std::fs::create_dir(root.join("scratch")).expect("mkdir");
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

    let mut server_args = vec!["--fake-mcp"];
    server_args.extend_from_slice(args);
    let claude_json = json!({
        "mcpServers": {
            "playwright": {
                "type": "stdio",
                "command": fake_claude(),
                "args": server_args,
                "env": {
                    "FAKE_MCP_MODE": mcp_mode,
                    "FAKE_MCP_RECORD": root.join("mcp-record.json"),
                },
            }
        }
    });
    std::fs::write(root.join(".claude.json"), claude_json.to_string()).expect("write");
    dir
}

const GOOD_ARGS: [&str; 5] = [
    "--browser",
    "chromium",
    "--headless",
    "--isolated",
    "--no-sandbox",
];

fn config(home: &Path) -> SmokeConfig {
    SmokeConfig {
        claude: fake_claude(),
        home: home.to_path_buf(),
        config_dir: home.join(".claude"),
        scratch_dir: home.join("scratch"),
        version_timeout: Duration::from_secs(20),
        browser_timeout: Duration::from_secs(20),
    }
}

fn failed(report: &Report) -> Vec<&str> {
    report
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| c.name)
        .collect()
}

fn detail<'a>(report: &'a Report, name: &str) -> &'a str {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.detail.as_str())
        .unwrap_or_else(|| panic!("no check named {name}"))
}

#[test]
fn a_complete_image_passes_every_check() {
    let home = home("ok", &GOOD_ARGS);
    let report = smoke_test(&config(home.path()));

    assert!(report.ok, "{:#?}", report.checks);
    assert_eq!(
        report.checks.iter().map(|c| c.name).collect::<Vec<_>>(),
        ["claude", "settings", "agents", "mcp-config", "chromium"]
    );
    assert_eq!(report.claude_code_version.as_deref(), Some("9.9.9"));
    assert_eq!(detail(&report, "claude"), "9.9.9 (Claude Code)");
    assert_eq!(detail(&report, "agents"), "code-reviewer");
    assert!(detail(&report, "chromium").contains("opened a page in headless Chromium"));

    // The MCP server was started exactly as configured, in the scratch directory.
    let record = read_json(&home.path().join("mcp-record.json"));
    let args: Vec<&str> = record["args"]
        .as_array()
        .expect("args")
        .iter()
        .filter_map(|a| a.as_str())
        .collect();
    assert_eq!(args[0], "--fake-mcp");
    assert_eq!(args[1..], GOOD_ARGS);
    let scratch = home.path().join("scratch").canonicalize().expect("scratch");
    assert_eq!(record["cwd"].as_str(), Some(path_str(&scratch).as_str()));
}

#[test]
fn a_browser_that_does_not_launch_fails_the_chromium_check() {
    let home = home("launch-error", &GOOD_ARGS);
    let report = smoke_test(&config(home.path()));
    assert!(!report.ok);
    assert_eq!(failed(&report), ["chromium"]);
    assert!(
        detail(&report, "chromium").contains("Failed to launch chromium"),
        "{}",
        detail(&report, "chromium")
    );
}

#[test]
fn an_mcp_server_that_dies_or_never_answers_fails_within_the_timeout() {
    let home_exit = home("exit", &GOOD_ARGS);
    let report = smoke_test(&config(home_exit.path()));
    assert_eq!(failed(&report), ["chromium"]);
    assert!(
        detail(&report, "chromium").contains("exited before answering")
            || detail(&report, "chromium").contains("closed its input"),
        "{}",
        detail(&report, "chromium")
    );

    let home_silent = home("silent", &GOOD_ARGS);
    let mut cfg = config(home_silent.path());
    cfg.browser_timeout = Duration::from_millis(500);
    let started = Instant::now();
    let report = smoke_test(&cfg);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(failed(&report), ["chromium"]);
    assert!(detail(&report, "chromium").contains("no answer to initialize in time"));
}

#[test]
fn a_wrong_browser_setting_fails_before_anything_is_launched() {
    let home = home("ok", &["--browser", "chrome", "--headless", "--isolated"]);
    let report = smoke_test(&config(home.path()));
    assert_eq!(failed(&report), ["mcp-config", "chromium"]);
    assert!(detail(&report, "mcp-config").contains("--browser chromium"));
    assert!(detail(&report, "chromium").starts_with("not tried"));
    assert!(!home.path().join("mcp-record.json").exists());
}

#[test]
fn every_missing_piece_is_reported_at_once() {
    let empty = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(empty.path());
    cfg.claude = empty.path().join("no-such-claude");
    let report = smoke_test(&cfg);
    assert!(!report.ok);
    assert_eq!(
        failed(&report),
        ["claude", "settings", "agents", "mcp-config", "chromium"]
    );
    assert_eq!(report.claude_code_version, None);
    assert_eq!(
        report.lines().len(),
        5,
        "one line per check: {:?}",
        report.lines()
    );
    assert!(report.lines().iter().all(|l| l.starts_with("FAIL ")));
}

//! `agent-runner --smoke-test`: checks an agent image without calling the Claude API.
//!
//! `singlelane image-update` runs it in a throwaway container of the candidate image
//! (SPEC 7.1). It checks that
//! - `claude --version` works,
//! - `~/.claude/settings.json` is there with its deny rules, and the subagents are installed,
//! - the Playwright MCP server is configured at user scope for chromium, headless, isolated,
//! - Chromium really launches headless: the configured MCP server is started and asked to
//!   open a page, exactly as Claude Code would do it.
//!
//! User-scoped MCP servers live in `~/.claude.json` under `mcpServers`
//! (<https://code.claude.com/docs/en/mcp>, "User scope"); the image writes the entry with
//! `claude mcp add --scope user`. The MCP side is the stdio transport of the Model Context
//! Protocol: one JSON-RPC message per line.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::process::kill_group_of;

/// The name of the MCP server entry.
pub const MCP_SERVER_NAME: &str = "playwright";
/// The MCP protocol revision the smoke test speaks.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// How long the MCP server gets to close the browser and exit after the check.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// The page title the smoke test looks for.
const MARKER: &str = "singlelane-smoke";

/// What to check and where.
#[derive(Debug, Clone)]
pub struct SmokeConfig {
    /// The Claude Code executable.
    pub claude: PathBuf,
    /// The home directory: `.claude.json` holds the user-scoped MCP servers.
    pub home: PathBuf,
    /// Claude Code's configuration directory (`~/.claude`, or `CLAUDE_CONFIG_DIR`).
    pub config_dir: PathBuf,
    /// The working directory for the MCP server (a scratch directory).
    pub scratch_dir: PathBuf,
    /// Time allowed for `claude --version`.
    pub version_timeout: Duration,
    /// Time allowed for the MCP server to start and for Chromium to open the page.
    pub browser_timeout: Duration,
}

impl SmokeConfig {
    /// The configuration for the container environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let home = get("HOME").map_or_else(|| PathBuf::from("/home/agent"), PathBuf::from);
        Self {
            claude: get("AGENT_RUNNER_CLAUDE")
                .map_or_else(|| PathBuf::from("claude"), PathBuf::from),
            config_dir: get("CLAUDE_CONFIG_DIR")
                .map_or_else(|| home.join(".claude"), PathBuf::from),
            home,
            scratch_dir: std::env::temp_dir(),
            version_timeout: Duration::from_secs(30),
            browser_timeout: Duration::from_secs(120),
        }
    }
}

/// One check of the smoke test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// A short stable name (`claude`, `settings`, `agents`, `mcp-config`, `chromium`).
    pub name: &'static str,
    /// Whether it passed.
    pub ok: bool,
    /// The version found, or what is wrong.
    pub detail: String,
}

/// The smoke test's result. Printed as the last line of output, as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Whether every check passed.
    pub ok: bool,
    /// agent-runner's version.
    pub agent_runner_version: &'static str,
    /// Claude Code's version (the first word of `claude --version`), if it ran.
    pub claude_code_version: Option<String>,
    /// The checks, in order.
    pub checks: Vec<Check>,
}

impl Report {
    /// The human-readable lines, one per check.
    pub fn lines(&self) -> Vec<String> {
        self.checks
            .iter()
            .map(|c| {
                format!(
                    "{} {}: {}",
                    if c.ok { "PASS" } else { "FAIL" },
                    c.name,
                    c.detail
                )
            })
            .collect()
    }
}

/// Runs all checks. A failing check never stops the others, except that Chromium cannot be
/// launched without a usable MCP configuration.
pub fn smoke_test(cfg: &SmokeConfig) -> Report {
    let mut checks = Vec::new();
    let mut claude_code_version = None;

    checks.push(match claude_version(cfg) {
        Ok(line) => {
            claude_code_version = line.split_whitespace().next().map(str::to_string);
            check("claude", Ok(line))
        }
        Err(e) => check("claude", Err(e)),
    });
    checks.push(check("settings", settings(&cfg.config_dir)));
    checks.push(check("agents", agents(&cfg.config_dir)));

    let server = mcp_server(&cfg.home);
    checks.push(check(
        "mcp-config",
        server
            .as_ref()
            .map(|s| format!("{} {}", s.command, s.args.join(" ")))
            .map_err(Clone::clone),
    ));
    checks.push(check(
        "chromium",
        match &server {
            Ok(server) => chromium_launches(cfg, server),
            Err(_) => Err("not tried: no usable Playwright MCP configuration".to_string()),
        },
    ));

    Report {
        ok: checks.iter().all(|c| c.ok),
        agent_runner_version: env!("CARGO_PKG_VERSION"),
        claude_code_version,
        checks,
    }
}

fn check(name: &'static str, result: Result<String, String>) -> Check {
    match result {
        Ok(detail) => Check {
            name,
            ok: true,
            detail,
        },
        Err(detail) => Check {
            name,
            ok: false,
            detail,
        },
    }
}

/// `claude --version`, for example `2.1.331 (Claude Code)`.
fn claude_version(cfg: &SmokeConfig) -> Result<String, String> {
    let program = cfg.claude.display();
    let mut child = Command::new(&cfg.claude)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("can't run {program} --version: {e}"))?;
    let lines = lines_of(child.stdout.take());
    let deadline = Instant::now() + cfg.version_timeout;
    let mut first_line = None;
    let status = loop {
        if first_line.is_none()
            && let Ok(line) = lines.try_recv()
        {
            first_line = Some(line);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group_of(&mut child);
                return Err(format!(
                    "{program} --version did not finish within {} s",
                    cfg.version_timeout.as_secs()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                kill_group_of(&mut child);
                return Err(format!("can't wait for {program} --version: {e}"));
            }
        }
    };
    let first_line = first_line.or_else(|| lines.recv_timeout(Duration::from_secs(2)).ok());
    if !status.success() {
        return Err(format!("{program} --version failed ({status})"));
    }
    first_line
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .ok_or_else(|| format!("{program} --version printed nothing"))
}

/// `settings.json` must parse and carry the deny rules from `assets/settings.json`.
fn settings(config_dir: &Path) -> Result<String, String> {
    let path = config_dir.join("settings.json");
    let json = read_json(&path)?;
    let deny = json
        .pointer("/permissions/deny")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if deny == 0 {
        return Err(format!("{} has no permissions.deny rules", path.display()));
    }
    Ok(format!("{} ({deny} deny rules)", path.display()))
}

/// At least one subagent definition must be installed.
fn agents(config_dir: &Path) -> Result<String, String> {
    let dir = config_dir.join("agents");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| format!("can't read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".md").map(str::to_string))
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(format!("{} has no subagent definitions", dir.display()));
    }
    Ok(names.join(", "))
}

/// A stdio MCP server entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct McpServer {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

/// Reads the user-scoped `playwright` server and checks its options.
fn mcp_server(home: &Path) -> Result<McpServer, String> {
    let path = home.join(".claude.json");
    let json = read_json(&path)?;
    let entry = json
        .get("mcpServers")
        .and_then(|s| s.get(MCP_SERVER_NAME))
        .ok_or_else(|| {
            format!(
                "{} has no user-scoped MCP server named {MCP_SERVER_NAME:?}",
                path.display()
            )
        })?;
    if let Some(kind) = entry.get("type").and_then(Value::as_str)
        && kind != "stdio"
    {
        return Err(format!(
            "MCP server {MCP_SERVER_NAME:?} has type {kind:?}, expected stdio"
        ));
    }
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| format!("MCP server {MCP_SERVER_NAME:?} has no command"))?
        .to_string();
    let args: Vec<String> = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let env: BTreeMap<String, String> = entry
        .get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();

    // Chrome is not available on arm64 and is Playwright MCP's default: the browser must be
    // set. Headless because there is no display; isolated so no profile outlives a job.
    let browser = args
        .iter()
        .position(|a| a == "--browser")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .or_else(|| args.iter().find_map(|a| a.strip_prefix("--browser=")));
    if browser != Some("chromium") {
        return Err(format!(
            "MCP server {MCP_SERVER_NAME:?} must run with --browser chromium (found {})",
            browser.map_or_else(|| "no --browser".to_string(), |b| format!("{b:?}"))
        ));
    }
    for flag in ["--headless", "--isolated"] {
        if !args.iter().any(|a| a == flag) {
            return Err(format!(
                "MCP server {MCP_SERVER_NAME:?} must run with {flag}"
            ));
        }
    }
    Ok(McpServer { command, args, env })
}

/// Starts the MCP server as configured and has it open a page in Chromium.
fn chromium_launches(cfg: &SmokeConfig, server: &McpServer) -> Result<String, String> {
    let mut child = Command::new(&server.command)
        .args(&server.args)
        .envs(&server.env)
        .current_dir(&cfg.scratch_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("can't start the MCP server {:?}: {e}", server.command))?;
    let result = match (child.stdin.take(), child.stdout.take()) {
        (Some(stdin), Some(stdout)) => {
            let mut session = McpSession {
                stdin,
                lines: lines_of(Some(stdout)),
                deadline: Instant::now() + cfg.browser_timeout,
                initialized: false,
            };
            let result = open_page(&mut session);
            // Playwright starts Chromium in a process group of its own, so killing the
            // server would leave the browser running: ask for a clean shutdown first.
            if session.initialized {
                session.deadline = Instant::now() + SHUTDOWN_WAIT;
                let _ = session.request(
                    3,
                    "tools/call",
                    json!({"name": "browser_close", "arguments": {}}),
                );
            }
            result
        }
        _ => Err("the MCP server has no stdio".to_string()),
    };
    // Its stdin is closed now (the session is gone): the server exits by itself.
    let deadline = Instant::now() + SHUTDOWN_WAIT;
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(20));
    }
    kill_group_of(&mut child);
    result
}

fn open_page(session: &mut McpSession) -> Result<String, String> {
    let init = session.request(
        1,
        "initialize",
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "agent-runner-smoke-test", "version": env!("CARGO_PKG_VERSION")},
        }),
    )?;
    let server_name = init
        .pointer("/serverInfo/name")
        .and_then(Value::as_str)
        .unwrap_or("MCP server");
    let server_version = init
        .pointer("/serverInfo/version")
        .and_then(Value::as_str)
        .unwrap_or("unknown version");
    session.initialized = true;
    session.notify("notifications/initialized")?;

    let url = format!("data:text/html,<title>{MARKER}</title><h1>ok</h1>");
    let result = session.request(
        2,
        "tools/call",
        json!({"name": "browser_navigate", "arguments": {"url": url}}),
    )?;
    let text: String = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(format!("browser_navigate failed: {}", brief(&text)));
    }
    if !text.contains(&format!("Page Title: {MARKER}")) {
        return Err(format!(
            "browser_navigate did not report the test page: {}",
            brief(&text)
        ));
    }
    Ok(format!(
        "{server_name} {server_version} opened a page in headless Chromium"
    ))
}

/// A minimal MCP client over stdio.
struct McpSession {
    stdin: ChildStdin,
    lines: Receiver<String>,
    deadline: Instant,
    /// Whether the server answered `initialize`.
    initialized: bool,
}

impl McpSession {
    fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("the MCP server closed its input: {e}"))
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method}))
    }

    /// Sends a request and waits for its response, skipping notifications and other traffic.
    fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value, String> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("no answer to {method} in time"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("the MCP server exited before answering {method}"));
                }
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| error.to_string(), str::to_string);
                return Err(format!("{method} failed: {}", brief(&text)));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| format!("{method} returned no result"));
        }
    }
}

/// Reads lines on a thread, so the caller can wait for them with a timeout.
fn lines_of(reader: Option<impl Read + Send + 'static>) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    if let Some(reader) = reader {
        thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    rx
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))
}

fn brief(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = line.chars().take(300).collect();
    if line.chars().count() > 300 {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(claude_json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".claude.json"), claude_json).expect("write");
        dir
    }

    fn server_json(args: &[&str]) -> String {
        json!({
            "userID": "placeholder",
            "mcpServers": {"playwright": {"type": "stdio", "command": "playwright-mcp", "args": args, "env": {}}}
        })
        .to_string()
    }

    #[test]
    fn mcp_config_as_written_by_claude_mcp_add_is_accepted() {
        // The shape `claude mcp add --scope user playwright -- playwright-mcp ...` wrote with
        // Claude Code 2.1.331.
        let home = home_with(&server_json(&[
            "--browser",
            "chromium",
            "--headless",
            "--isolated",
            "--no-sandbox",
        ]));
        let server = mcp_server(home.path()).expect("valid");
        assert_eq!(server.command, "playwright-mcp");
        assert_eq!(server.args.len(), 5);
        assert!(server.env.is_empty());
    }

    #[test]
    fn mcp_config_accepts_the_equals_form_of_browser() {
        let home = home_with(&server_json(&[
            "--browser=chromium",
            "--headless",
            "--isolated",
        ]));
        assert!(mcp_server(home.path()).is_ok());
    }

    #[test]
    fn mcp_config_must_name_chromium_headless_and_isolated() {
        for (args, expected) in [
            (&["--headless", "--isolated"][..], "--browser chromium"),
            (
                &["--browser", "chrome", "--headless", "--isolated"][..],
                "--browser chromium",
            ),
            (&["--browser", "chromium", "--isolated"][..], "--headless"),
            (&["--browser", "chromium", "--headless"][..], "--isolated"),
        ] {
            let home = home_with(&server_json(args));
            let err = mcp_server(home.path()).expect_err("must be rejected");
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn missing_or_broken_mcp_config_is_reported() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert!(
            mcp_server(empty.path())
                .expect_err("missing")
                .contains("can't read")
        );
        for (content, expected) in [
            ("{not json", "not valid JSON"),
            ("{}", "no user-scoped MCP server"),
            (
                r#"{"mcpServers":{"other":{"command":"x"}}}"#,
                "no user-scoped MCP server",
            ),
            (
                r#"{"mcpServers":{"playwright":{"type":"http","url":"https://example.invalid"}}}"#,
                "expected stdio",
            ),
            (
                r#"{"mcpServers":{"playwright":{"args":[]}}}"#,
                "has no command",
            ),
        ] {
            let home = home_with(content);
            let err = mcp_server(home.path()).expect_err(content);
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn settings_need_deny_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(settings(dir.path()).is_err());
        std::fs::write(dir.path().join("settings.json"), "{}").expect("write");
        assert!(
            settings(dir.path())
                .expect_err("no rules")
                .contains("permissions.deny")
        );
        std::fs::write(
            dir.path().join("settings.json"),
            include_str!("../../../assets/settings.json"),
        )
        .expect("write");
        let ok = settings(dir.path()).expect("the shipped settings pass");
        assert!(ok.contains("deny rules"), "{ok}");
    }

    #[test]
    fn agents_need_at_least_one_definition() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(agents(dir.path()).is_err());
        std::fs::create_dir(dir.path().join("agents")).expect("mkdir");
        assert!(agents(dir.path()).is_err());
        std::fs::write(dir.path().join("agents/notes.txt"), "x").expect("write");
        assert!(agents(dir.path()).is_err());
        std::fs::write(dir.path().join("agents/code-reviewer.md"), "x").expect("write");
        assert_eq!(agents(dir.path()).expect("found"), "code-reviewer");
    }

    #[test]
    fn report_lines_show_pass_and_fail() {
        let report = Report {
            ok: false,
            agent_runner_version: "0.0.0",
            claude_code_version: None,
            checks: vec![
                check("claude", Ok("2.1.331 (Claude Code)".into())),
                check("chromium", Err("boom".into())),
            ],
        };
        assert_eq!(
            report.lines(),
            ["PASS claude: 2.1.331 (Claude Code)", "FAIL chromium: boom"]
        );
    }
}

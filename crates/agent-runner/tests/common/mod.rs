//! Shared helpers for agent-runner's integration tests.
//!
//! The tests never start the real Claude Code: `fake-claude` (a binary of this package,
//! `src/bin/fake-claude.rs`) stands in for it and replays the stream-json fixtures.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code
#![allow(dead_code)] // each test binary uses a different part

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_runner::config::Config;
use agent_runner::process::Shutdown;
use agent_runner::run::{Outcome, run};
use singlelane_core::agent_out::{FailureReport, RunResult};
use singlelane_core::failure::{AgentReport, Classification, ErrorSource, FailureInput, classify};
use tempfile::TempDir;

/// The fake `claude` executable.
pub fn fake_claude() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-claude"))
}

/// The real `agent-runner` executable.
pub fn agent_runner() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-runner"))
}

/// A stream-json fixture.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub const PROMPT: &str = "You are an autonomous software engineer.\nTask: /task/task.md\n";

/// A scratch job: `work/`, `task/prompt.md`, `out/` and a private global git config.
pub struct Job {
    pub dir: TempDir,
    pub cfg: Config,
}

impl Job {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir(root.join("work")).expect("mkdir work");
        std::fs::create_dir(root.join("task")).expect("mkdir task");
        std::fs::write(root.join("task/prompt.md"), PROMPT).expect("write prompt");

        let vars = HashMap::from([
            ("AGENT_RUNNER_CLAUDE", path_str(&fake_claude())),
            ("AGENT_RUNNER_WORK_DIR", path_str(&root.join("work"))),
            (
                "AGENT_RUNNER_PROMPT",
                path_str(&root.join("task/prompt.md")),
            ),
            ("AGENT_RUNNER_OUT_DIR", path_str(&root.join("out"))),
            ("JOB_TIMEOUT", "60s".to_string()),
            ("AGENT_RUNNER_KILL_GRACE", "10s".to_string()),
        ]);
        let mut cfg =
            Config::from_lookup(|name| vars.get(name).cloned()).expect("valid test config");
        cfg.child_env = Self::base_env(root);
        Self { dir, cfg }
    }

    /// The environment every child gets: git must not touch the real global config, and the
    /// fake needs to know where the status file is.
    pub fn base_env(root: &Path) -> Vec<(OsString, OsString)> {
        vec![
            ("GIT_CONFIG_GLOBAL".into(), root.join("gitconfig").into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            (
                "FAKE_CLAUDE_STATUS_FILE".into(),
                root.join("out/status").into(),
            ),
        ]
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Sets an environment variable for the child processes.
    pub fn env(&mut self, name: &str, value: impl Into<OsString>) -> &mut Self {
        self.cfg.child_env.push((name.into(), value.into()));
        self
    }

    /// Has the fake print a fixture.
    pub fn replay(&mut self, name: &str) -> &mut Self {
        self.env("FAKE_CLAUDE_REPLAY", fixture(name))
    }

    /// Has the fake write the status file, as the agent would.
    pub fn status(&mut self, status: &str) -> &mut Self {
        self.env("FAKE_CLAUDE_STATUS", status)
    }

    /// Has the fake record how it was started; returns where.
    pub fn record(&mut self) -> PathBuf {
        let path = self.root().join("record.json");
        self.env("FAKE_CLAUDE_RECORD", &path);
        path
    }

    pub fn timeout_ms(&mut self, timeout: u64, grace: u64) -> &mut Self {
        self.cfg.job_timeout = Duration::from_millis(timeout);
        self.cfg.kill_grace = Duration::from_millis(grace);
        self
    }

    pub fn run(&self) -> Outcome {
        run(&self.cfg, &Shutdown::new())
    }

    pub fn out(&self, name: &str) -> PathBuf {
        self.root().join("out").join(name)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.out(name).exists()
    }

    pub fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.out(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
    }

    pub fn result(&self) -> RunResult {
        serde_json::from_str(&self.read("result.json")).expect("result.json parses")
    }

    pub fn failure(&self) -> FailureReport {
        serde_json::from_str(&self.read("failure.json")).expect("failure.json parses")
    }

    /// What the host's classifier makes of this job's `failure.json` (SPEC 6.8).
    pub fn classify(&self) -> Classification {
        classify_report(&self.failure())
    }
}

/// Classifies a `failure.json` the way the host will: as an untrusted agent report, with no
/// failing host-side network probe.
pub fn classify_report(report: &FailureReport) -> Classification {
    classify(&FailureInput {
        source: Some(ErrorSource::Agent),
        agent: Some(AgentReport::from_failure_report(report)),
        ..FailureInput::default()
    })
}

pub fn path_str(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_string()
}

pub fn read_json(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

/// Whether a process is gone (or only a zombie waiting to be reaped).
pub fn process_is_dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // "pid (comm) state ...": the state follows the last ')'.
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Waits until a process is gone.
pub fn wait_until_dead(pid: u32, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if process_is_dead(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

//! agent-runner's configuration, read from the container environment.
//!
//! The host passes plain, non-secret job settings (SPEC 6.5): `CLAUDE_MODEL`, `MAX_TURNS`,
//! `JOB_TIMEOUT`, `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`. The Claude credential comes from the
//! env file and is only passed on to Claude Code; here it is read for redaction alone.
//!
//! The `AGENT_RUNNER_*` variables override paths and timings. The image never sets them;
//! they exist for tests and for running the binary outside the container.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use singlelane_core::config::parse_duration;
use singlelane_core::policy::redact::Redactor;

/// Default model alias.
pub const DEFAULT_MODEL: &str = "opus";
/// Default `--max-turns`.
pub const DEFAULT_MAX_TURNS: u32 = 200;
/// Default `JOB_TIMEOUT`.
pub const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(90 * 60);
/// Time between SIGTERM and SIGKILL (SPEC 7).
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(60);
/// Git identity used when the host passes none.
pub const DEFAULT_GIT_NAME: &str = "singlelane-agent";
/// Git identity used when the host passes none.
pub const DEFAULT_GIT_EMAIL: &str = "singlelane-agent@users.noreply.github.com";

/// Env vars whose values are secrets: redacted wherever they show up in output.
pub const SECRET_ENV_VARS: [&str; 3] = [
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
];

/// A configuration problem.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{var}: {message}")]
pub struct ConfigError {
    /// The env var.
    pub var: &'static str,
    /// What is wrong with it.
    pub message: String,
}

/// Where agent-runner writes its results. Split from [`Config`] so that a failure to read
/// the rest of the configuration can still be reported in `/out`.
pub fn out_dir(lookup: impl Fn(&str) -> Option<String>) -> PathBuf {
    path(&lookup, "AGENT_RUNNER_OUT_DIR", "/out")
}

/// Everything a run needs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The Claude Code executable (`AGENT_RUNNER_CLAUDE`, default `claude` from `PATH`).
    pub claude: PathBuf,
    /// The git executable (`AGENT_RUNNER_GIT`, default `git` from `PATH`).
    pub git: PathBuf,
    /// The repository checkout Claude Code works in (`AGENT_RUNNER_WORK_DIR`, default `/work`).
    pub work_dir: PathBuf,
    /// The rendered prompt (`AGENT_RUNNER_PROMPT`, default `/task/prompt.md`).
    pub prompt_file: PathBuf,
    /// The output directory (`AGENT_RUNNER_OUT_DIR`, default `/out`).
    pub out_dir: PathBuf,
    /// `--model` (`CLAUDE_MODEL`).
    pub model: String,
    /// `--max-turns` (`MAX_TURNS`).
    pub max_turns: u32,
    /// How long Claude Code may run (`JOB_TIMEOUT`, for example `90m`).
    pub job_timeout: Duration,
    /// Time between SIGTERM and SIGKILL (`AGENT_RUNNER_KILL_GRACE`, default 60 s).
    pub kill_grace: Duration,
    /// Git `user.name` (`GIT_AUTHOR_NAME`).
    pub git_name: String,
    /// Git `user.email` (`GIT_AUTHOR_EMAIL`).
    pub git_email: String,
    /// Redacts the Claude credential and secret-shaped strings from everything written out.
    pub redactor: Redactor,
    /// Extra environment for the child processes. Empty in production; tests use it to steer
    /// the fake `claude` and to point git at a scratch config file.
    pub child_env: Vec<(OsString, OsString)>,
}

impl Config {
    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the configuration through `lookup` (env var name to value).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());

        let model =
            get("CLAUDE_MODEL").map_or_else(|| DEFAULT_MODEL.to_string(), |m| m.trim().to_string());
        if !valid_model(&model) {
            return Err(ConfigError {
                var: "CLAUDE_MODEL",
                message: format!("{model:?} is not a model alias or model id"),
            });
        }

        let max_turns = match get("MAX_TURNS") {
            None => DEFAULT_MAX_TURNS,
            Some(v) => v
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| ConfigError {
                    var: "MAX_TURNS",
                    message: format!("{v:?} is not a positive whole number"),
                })?,
        };

        let duration = |var: &'static str, default: Duration| match get(var) {
            None => Ok(default),
            Some(v) => parse_duration(&v).map_err(|message| ConfigError { var, message }),
        };
        let job_timeout = duration("JOB_TIMEOUT", DEFAULT_JOB_TIMEOUT)?;
        let kill_grace = duration("AGENT_RUNNER_KILL_GRACE", DEFAULT_KILL_GRACE)?;

        let identity = |var: &'static str, default: &str| {
            let value = get(var).map_or_else(|| default.to_string(), |v| v.trim().to_string());
            if value.chars().any(char::is_control) {
                Err(ConfigError {
                    var,
                    message: "must not contain control characters".to_string(),
                })
            } else {
                Ok(value)
            }
        };
        let git_name = identity("GIT_AUTHOR_NAME", DEFAULT_GIT_NAME)?;
        let git_email = identity("GIT_AUTHOR_EMAIL", DEFAULT_GIT_EMAIL)?;

        Ok(Self {
            claude: path(&lookup, "AGENT_RUNNER_CLAUDE", "claude"),
            git: path(&lookup, "AGENT_RUNNER_GIT", "git"),
            work_dir: path(&lookup, "AGENT_RUNNER_WORK_DIR", "/work"),
            prompt_file: path(&lookup, "AGENT_RUNNER_PROMPT", "/task/prompt.md"),
            out_dir: out_dir(&lookup),
            model,
            max_turns,
            job_timeout,
            kill_grace,
            git_name,
            git_email,
            redactor: redactor(&lookup),
            child_env: Vec::new(),
        })
    }
}

/// A redactor that knows the Claude credential of this container.
pub fn redactor(lookup: impl Fn(&str) -> Option<String>) -> Redactor {
    Redactor::with_secrets(SECRET_ENV_VARS.iter().filter_map(|name| lookup(name)))
}

fn path(lookup: impl Fn(&str) -> Option<String>, var: &str, default: &str) -> PathBuf {
    lookup(var)
        .filter(|v| !v.trim().is_empty())
        .map_or_else(|| PathBuf::from(default), PathBuf::from)
}

/// Model aliases and ids: `opus`, `claude-opus-5-5`, `opus[1m]`, a Bedrock id or ARN. The
/// value becomes one command-line argument, so it must not look like a flag.
fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 200
        && model.starts_with(|c: char| c.is_ascii_alphanumeric())
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:/@[]".contains(c))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|name| map.get(name).cloned())
    }

    #[test]
    fn defaults_match_the_container_layout() {
        let c = config(&[]).expect("defaults are valid");
        assert_eq!(c.claude, PathBuf::from("claude"));
        assert_eq!(c.git, PathBuf::from("git"));
        assert_eq!(c.work_dir, PathBuf::from("/work"));
        assert_eq!(c.prompt_file, PathBuf::from("/task/prompt.md"));
        assert_eq!(c.out_dir, PathBuf::from("/out"));
        assert_eq!(c.model, "opus");
        assert_eq!(c.max_turns, 200);
        assert_eq!(c.job_timeout, Duration::from_secs(5400));
        assert_eq!(c.kill_grace, Duration::from_secs(60));
        assert_eq!(c.git_name, DEFAULT_GIT_NAME);
        assert_eq!(c.git_email, DEFAULT_GIT_EMAIL);
        assert!(c.child_env.is_empty());
    }

    #[test]
    fn job_settings_come_from_the_environment() {
        let c = config(&[
            ("CLAUDE_MODEL", "claude-opus-5-5"),
            ("MAX_TURNS", "50"),
            ("JOB_TIMEOUT", "1h30m"),
            ("AGENT_RUNNER_KILL_GRACE", "5s"),
            ("GIT_AUTHOR_NAME", "example-app[bot]"),
            (
                "GIT_AUTHOR_EMAIL",
                "123456+example-app[bot]@users.noreply.github.com",
            ),
            ("AGENT_RUNNER_CLAUDE", "/opt/fake/claude"),
            ("AGENT_RUNNER_OUT_DIR", "/tmp/out"),
        ])
        .expect("valid");
        assert_eq!(c.model, "claude-opus-5-5");
        assert_eq!(c.max_turns, 50);
        assert_eq!(c.job_timeout, Duration::from_secs(5400));
        assert_eq!(c.kill_grace, Duration::from_secs(5));
        assert_eq!(c.git_name, "example-app[bot]");
        assert_eq!(
            c.git_email,
            "123456+example-app[bot]@users.noreply.github.com"
        );
        assert_eq!(c.claude, PathBuf::from("/opt/fake/claude"));
        assert_eq!(c.out_dir, PathBuf::from("/tmp/out"));
    }

    #[test]
    fn empty_values_fall_back_to_defaults() {
        let c = config(&[
            ("CLAUDE_MODEL", ""),
            ("MAX_TURNS", "  "),
            ("JOB_TIMEOUT", ""),
        ])
        .expect("valid");
        assert_eq!(c.model, "opus");
        assert_eq!(c.max_turns, 200);
        assert_eq!(c.job_timeout, DEFAULT_JOB_TIMEOUT);
    }

    #[test]
    fn bad_values_are_rejected_with_the_variable_name() {
        for (var, value) in [
            ("MAX_TURNS", "0"),
            ("MAX_TURNS", "-3"),
            ("MAX_TURNS", "many"),
            ("JOB_TIMEOUT", "soon"),
            ("JOB_TIMEOUT", "0m"),
            ("AGENT_RUNNER_KILL_GRACE", "x"),
            ("CLAUDE_MODEL", "--dangerously-skip-permissions"),
            ("CLAUDE_MODEL", "opus; rm -rf /"),
            ("GIT_AUTHOR_NAME", "a\nb"),
            ("GIT_AUTHOR_EMAIL", "a\u{0}b"),
        ] {
            let err = config(&[(var, value)]).expect_err(value);
            assert_eq!(err.var, var, "{value:?}");
            assert!(err.to_string().starts_with(var));
        }
    }

    #[test]
    fn model_ids_with_brackets_and_arns_are_accepted() {
        for model in [
            "opus",
            "sonnet",
            "opus[1m]",
            "claude-opus-5-5",
            "us.anthropic.claude-opus-5-5-v1:0",
            "arn:aws:bedrock:us-east-1:000000000000:inference-profile/example",
        ] {
            assert!(valid_model(model), "{model}");
        }
    }

    #[test]
    fn the_claude_credential_is_known_to_the_redactor_and_not_printed() {
        let key = format!("sk-ant-{}", "k".repeat(40));
        let odd = format!("odd-shaped-credential-{}", "z".repeat(12));
        let c = config(&[
            ("ANTHROPIC_API_KEY", &key),
            ("CLAUDE_CODE_OAUTH_TOKEN", &odd),
        ])
        .expect("valid");
        let text = format!("key {key} and token {odd}");
        let redacted = c.redactor.redact(&text);
        assert!(!redacted.contains(&key));
        assert!(!redacted.contains(&odd));
        let debug = format!("{c:?}");
        assert!(!debug.contains(&key));
        assert!(!debug.contains(&odd));
    }
}

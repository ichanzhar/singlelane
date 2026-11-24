//! Git setup inside the container: the commit identity and `safe.directory`.
//!
//! `/work` is a bind mount owned by the host's `singlelane` user. The uid matches the
//! container's `agent` user (1000) on a default install, but git refuses to work in a
//! repository owned by someone else, so the directory is marked safe either way.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};

use crate::config::Config;

/// Why the git setup failed.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// git would read a leading dash as an option.
    #[error("git identity {0:?} must not start with '-'")]
    Identity(String),
    /// The work dir cannot be written into the git config.
    #[error("work dir {0:?} is not valid UTF-8")]
    WorkDir(PathBuf),
    /// git could not be started.
    #[error("can't run {program}: {source}")]
    Spawn {
        /// The git executable.
        program: String,
        /// The spawn error.
        source: std::io::Error,
    },
    /// git ran and failed.
    #[error("{command} failed ({status}): {stderr}")]
    Failed {
        /// The command, without values.
        command: String,
        /// Its exit status.
        status: ExitStatus,
        /// What it printed to stderr.
        stderr: String,
    },
}

/// Sets `user.name`, `user.email` and `safe.directory` in the global git config.
pub fn configure(cfg: &Config) -> Result<(), GitError> {
    for value in [&cfg.git_name, &cfg.git_email] {
        if value.starts_with('-') {
            return Err(GitError::Identity(value.clone()));
        }
    }
    let work_dir = cfg
        .work_dir
        .to_str()
        .ok_or_else(|| GitError::WorkDir(cfg.work_dir.clone()))?;

    set(cfg, "user.name", &cfg.git_name)?;
    set(cfg, "user.email", &cfg.git_email)?;

    // `--get-all` exits with 1 when the key is not set: that is not an error here.
    let existing = git(cfg, ["config", "--global", "--get-all", "safe.directory"])?;
    let listed = String::from_utf8_lossy(&existing.stdout)
        .lines()
        .any(|line| line == work_dir);
    if !listed {
        check(
            "git config --global --add safe.directory",
            &git(
                cfg,
                ["config", "--global", "--add", "safe.directory", work_dir],
            )?,
        )?;
    }
    Ok(())
}

fn set(cfg: &Config, key: &str, value: &str) -> Result<(), GitError> {
    check(
        &format!("git config --global {key}"),
        &git(cfg, ["config", "--global", key, value])?,
    )
}

fn git<I, S>(cfg: &Config, args: I) -> Result<Output, GitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(&cfg.git)
        .args(args)
        .envs(cfg.child_env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .map_err(|source| GitError::Spawn {
            program: cfg.git.display().to_string(),
            source,
        })
}

fn check(command: &str, output: &Output) -> Result<(), GitError> {
    if output.status.success() {
        return Ok(());
    }
    Err(GitError::Failed {
        command: command.to_string(),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

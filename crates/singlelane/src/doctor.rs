//! `singlelane doctor`: the config, private key and GitHub checks (phase 2). Docker, `gh`,
//! the agent image, the claude env file, disk space and Telegram checks come with the phases
//! that use them.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use singlelane_core::doctor::{Check, CheckStatus, Summary};
use singlelane_core::policy::redact::Redactor;
use singlelane_github::AppKey;
use singlelane_github::doctor::github_checks;

use crate::github::{connect_with_key, load_key};
use crate::{load_config, print_lines};

/// Checks the private key file: exists, mode 600 (no group or other access), parses.
pub fn check_key_file(path: &Path) -> (Vec<Check>, Option<AppKey>) {
    let name = "private key file";
    let shown = path.display();
    let mode = match file_mode(path) {
        Ok(m) => m,
        Err(e) => {
            return (
                vec![Check::fail(
                    name,
                    format!("{shown}: {e}"),
                    "download a private key from the GitHub App settings and set \
                     github.private_key_path to it",
                )],
                None,
            );
        }
    };
    let mut checks = Vec::new();
    if mode & 0o077 != 0 {
        checks.push(Check::fail(
            name,
            format!("{shown} has mode {:o}", mode & 0o777),
            format!("chmod 600 {shown} && chown singlelane: {shown}"),
        ));
    } else {
        checks.push(Check::pass(
            name,
            format!("{shown}, mode {:o}", mode & 0o777),
        ));
    }
    match load_key(path) {
        Ok(key) => {
            checks.push(Check::pass("private key", "RSA key loaded"));
            (checks, Some(key))
        }
        Err(e) => {
            checks.push(Check::fail(
                "private key",
                format!("{e:#}"),
                "use the .pem file GitHub generated for the app (Private keys > Generate)",
            ));
            (checks, None)
        }
    }
}

#[cfg(unix)]
fn file_mode(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> std::io::Result<u32> {
    Err(std::io::Error::other(
        "file modes can only be checked on unix",
    ))
}

/// Runs the checks and prints them. Exit code 1 if anything failed.
pub async fn run(config_path: &Path, with_claude: bool) -> Result<ExitCode> {
    let redactor = Redactor::new();
    let mut checks = Vec::new();
    let config = match load_config(config_path) {
        Ok(c) => {
            checks.push(Check::pass(
                "config",
                format!("{} is valid", config_path.display()),
            ));
            Some(c)
        }
        Err(e) => {
            checks.push(Check::fail(
                "config",
                format!("{e:#}"),
                "fix the config file (format: SPEC.md section 5)",
            ));
            None
        }
    };
    if let Some(config) = &config {
        let (key_checks, key) = check_key_file(&config.github.private_key_path);
        checks.extend(key_checks);
        if let Some(key) = key {
            match connect_with_key(config, key) {
                Ok(client) => {
                    checks.extend(github_checks(&client.gh, client.auth.as_ref(), config).await);
                }
                Err(e) => checks.push(Check::fail(
                    "GitHub client",
                    format!("{e:#}"),
                    "check the host's TLS root certificates",
                )),
            }
        }
    }
    if with_claude {
        checks.push(Check::warn(
            "claude probe",
            "not available yet (needs the agent image, phase 3 and 4)",
            "run doctor without --with-claude for now",
        ));
    }
    let summary = Summary::of(&checks);
    let mut lines: Vec<String> = checks.iter().map(Check::render).collect();
    lines.push(format!("doctor: {summary}"));
    print_lines(&redactor, &lines)?;
    let failed = checks.iter().any(|c| c.status == CheckStatus::Fail);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn key_file_checks() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        let (checks, key) = check_key_file(&missing);
        assert_eq!(checks[0].status, CheckStatus::Fail);
        assert!(key.is_none());

        let bad = dir.path().join("bad.pem");
        std::fs::write(&bad, "not a key").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (checks, key) = check_key_file(&bad);
        assert_eq!(checks[0].status, CheckStatus::Fail, "mode 644");
        assert!(checks[0].hint.as_deref().unwrap().contains("chmod 600"));
        assert_eq!(checks[1].status, CheckStatus::Fail, "not a key");
        assert!(key.is_none());

        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (checks, _) = check_key_file(&bad);
        assert_eq!(checks[0].status, CheckStatus::Pass);
    }

    #[test]
    fn valid_key_with_mode_600_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.pem");
        let body = include_str!("../../singlelane-github/tests/fixtures/test-app-key.pkcs1.b64");
        let marker = |edge: &str| format!("-----{edge} RSA {} KEY-----", "PRIVATE");
        std::fs::write(
            &path,
            format!("{}\n{body}{}\n", marker("BEGIN"), marker("END")),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (checks, key) = check_key_file(&path);
        assert!(
            checks.iter().all(|c| c.status == CheckStatus::Pass),
            "{checks:?}"
        );
        assert!(key.is_some());
    }

    #[tokio::test]
    async fn invalid_config_fails_without_github_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "trusted_user = 1").unwrap();
        let code = run(&path, false).await.unwrap();
        assert_eq!(code, ExitCode::FAILURE);
    }
}

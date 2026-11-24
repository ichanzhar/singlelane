//! singlelane CLI and daemon (SPEC 9).
//!
//! Phase 2 provides `setup-labels` and the config and GitHub part of `doctor`. The other
//! commands arrive with the job executor (phase 4) and the daemon (phase 5).

mod doctor;
mod github;
mod logging;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use singlelane_core::config::{Config, DEFAULT_CONFIG_PATH};
use singlelane_core::policy::redact::Redactor;
use singlelane_github::labels::setup_labels;
use singlelane_github::types::LabelChange;

/// Turns GitHub issues into PRs with Claude Code, one task at a time.
#[derive(Debug, Parser)]
#[command(name = "singlelane", version)]
struct Cli {
    /// Config file.
    #[arg(long, global = true, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Preflight checks; prints PASS, WARN or FAIL with a fix hint for each.
    Doctor {
        /// Also prove Claude auth with a one-turn prompt (costs a few tokens).
        #[arg(long)]
        with_claude: bool,
    },
    /// Create or update the workflow labels in every configured repo.
    SetupLabels,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let redactor = Redactor::new();
    logging::init(redactor.clone());
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fatal(&redactor, &anyhow::Error::from(e)),
    };
    let result = runtime.block_on(async {
        match cli.command {
            Command::Doctor { with_claude } => doctor::run(&cli.config, with_claude).await,
            Command::SetupLabels => run_setup_labels(&cli.config).await,
        }
    });
    match result {
        Ok(code) => code,
        Err(e) => fatal(&redactor, &e),
    }
}

/// Prints an error (redacted, G8) and returns the failure exit code.
fn fatal(redactor: &Redactor, e: &anyhow::Error) -> ExitCode {
    let _ = writeln!(
        std::io::stderr().lock(),
        "error: {}",
        redactor.redact(&format!("{e:#}"))
    );
    ExitCode::FAILURE
}

/// Reads and parses the config file.
pub(crate) fn load_config(path: &std::path::Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("can't read config file {}", path.display()))?;
    Config::from_toml(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// Writes lines to stdout, redacted.
pub(crate) fn print_lines(redactor: &Redactor, lines: &[String]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for line in lines {
        writeln!(out, "{}", redactor.redact(line)).context("can't write to stdout")?;
    }
    Ok(())
}

async fn run_setup_labels(config_path: &std::path::Path) -> Result<ExitCode> {
    let config = load_config(config_path)?;
    let client = github::connect(&config)?;
    let outcomes = setup_labels(&client.gh, &config.repo_names()).await;
    let mut failed = false;
    let lines: Vec<String> = outcomes
        .iter()
        .map(|o| {
            let what = match &o.result {
                Ok(LabelChange::Created) => "created".to_string(),
                Ok(LabelChange::Updated) => "updated".to_string(),
                Ok(LabelChange::Unchanged) => "unchanged".to_string(),
                Err(e) => {
                    failed = true;
                    format!("FAILED: {e}")
                }
            };
            format!("{} {}: {what}", o.repo, o.label)
        })
        .collect();
    print_lines(&Redactor::new(), &lines)?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

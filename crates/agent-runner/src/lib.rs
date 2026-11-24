//! agent-runner: runs inside the agent container and drives Claude Code (SPEC 7).
//!
//! It replaces the old `entrypoint.sh`. No GitHub credential exists in the container (G2):
//! the agent edits and commits, the host pushes.
//!
//! - [`config`]: settings from the container environment.
//! - [`git`]: git identity and `safe.directory`.
//! - [`process`]: the child process group, the job timeout and the output pumps.
//! - [`stream`]: Claude Code's stream-json events.
//! - [`run`]: one run, and the files it leaves in `/out`.
//! - [`smoke`]: `--smoke-test`, the image check used by `singlelane image-update` (SPEC 7.1).

pub mod config;
pub mod git;
pub mod process;
pub mod run;
pub mod smoke;
pub mod stream;

/// `agent-runner <version>`, as printed by `--version`.
pub fn version_line() -> String {
    format!("agent-runner {}", env!("CARGO_PKG_VERSION"))
}

//! agent-runner: runs inside the agent container and drives Claude Code.
//!
//! Stub for phase 1; built in phase 3 (see `docs/build-phases/03-agent-runner-image.md`).

fn main() -> anyhow::Result<()> {
    anyhow::bail!(
        "agent-runner {} is not implemented yet (phase 3)",
        env!("CARGO_PKG_VERSION")
    )
}

//! singlelane CLI and daemon.
//!
//! Stub for phase 1; the CLI and daemon are built in phases 2, 4 and 5.

fn main() -> anyhow::Result<()> {
    anyhow::bail!(
        "singlelane {} is not functional yet: only singlelane-core exists (phase 1)",
        env!("CARGO_PKG_VERSION")
    )
}

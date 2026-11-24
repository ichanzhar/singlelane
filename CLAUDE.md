# CLAUDE.md - singlelane (Rust)

## What this is
singlelane is a Rust daemon that turns GitHub issues into PRs using Claude Code in per-job
containers on a Raspberry Pi 5, one task at a time. `SPEC.md` is the source of truth. Read it
before any change. Build phases and their prompts are in `docs/build-phases/`.

This is a public repository. Never commit secrets, private keys, real tokens, real chat ids,
personal repo names, or logs from real runs. Examples use obvious placeholders.

## Commands
- Format: `cargo fmt --all`
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Test: `cargo test --all`
- Cross build for the Pi: `cross build --release --target aarch64-unknown-linux-gnu`
  (or a native `cargo build --release` on the Pi)

## Rules
- Guarantees in SPEC.md section 2 are non-negotiable. Every guarantee has a test whose
  name starts with its id. Never weaken or delete these tests.
- All external I/O (GitHub, git, docker, gh, clock, filesystem roots) goes through traits so
  the flow is testable with fakes. Tests never call the real GitHub or start real containers.
- No `unsafe`. No `unwrap`/`expect` outside tests. Errors: `thiserror` in libs, `anyhow` in
  bins, with context on every external call.
- External commands: argument vectors only, never through a shell. Secrets go only in child
  process env vars, never in arguments (arguments show up in `ps`).
- Before writing an API call against a crate (octocrab, bollard, rusqlite, chrono-tz...),
  check its current signature on docs.rs. Do not guess.
- Keep `singlelane-core` pure: no tokio, no network, no filesystem.

## Process
- Work in the phase you were given. At the end of a phase: all checks green, commit with
  conventional commits, then summarize what was built, what is stubbed, and open questions.
- If you must deviate from SPEC.md, update SPEC.md and add a Decision log row in the same commit.
- `reference/` holds the old bash implementation for behavior reference only. Port the
  behavior, not the style. Delete `reference/` in the final phase.

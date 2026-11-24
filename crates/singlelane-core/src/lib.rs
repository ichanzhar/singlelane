//! Pure domain logic for singlelane.
//!
//! Everything here is deterministic and free of I/O: no tokio, no network, no filesystem.
//! Callers pass in parsed data (config text, env file text, file modes, GitHub snapshots,
//! the current time) and get decisions back.
//!
//! Modules:
//! - [`config`]: config types, TOML parsing and startup validation (SPEC 5).
//! - [`repo`]: repository names and item references.
//! - [`state`]: labels, item states and the transition table (SPEC 3).
//! - [`inflight`]: the in-flight rules as a pure decision function (SPEC 3.1, G10).
//! - [`policy`]: trusted-author filter (G1), protected paths (G4), redaction (G8).
//! - [`schedule`]: clock, working hours and tick alignment (G9).
//! - [`render`]: task and prompt rendering (SPEC 6.4).
//! - [`failure`]: failure classification (SPEC 6.8).
//! - [`notify`]: Telegram message formatting (SPEC 11).

pub mod config;
pub mod failure;
pub mod inflight;
pub mod notify;
pub mod policy;
pub mod render;
pub mod repo;
pub mod schedule;
pub mod state;

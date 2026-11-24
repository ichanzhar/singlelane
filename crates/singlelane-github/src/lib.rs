//! GitHub layer for singlelane (SPEC 4, 5.1, 6.1).
//!
//! - [`auth`]: GitHub App authentication: JWT signing, installation tokens with an in-memory
//!   cache and refresh, and the [`TokenProvider`] trait used by git and `gh` (G8: the key and
//!   tokens never appear in logs or errors).
//! - [`api`]: the [`GitHub`] trait, exactly the operations the factory needs. It has no merge
//!   or approve method (G3 by construction).
//! - [`octo`]: [`OctoGitHub`], the REST implementation on octocrab with rustls, over
//!   [`http::Http`] (retries with backoff, `Retry-After`, request-id logging, pagination).
//! - [`fake`]: [`FakeGitHub`], an in-memory implementation for tests of full flows.
//! - [`claim`]: the claim (label swap, re-read, verify; G5).
//! - [`task`]: reading the raw text of an item for `task.md` (filtered by G1 in core).
//! - [`labels`]: creates the workflow labels (`setup-labels`).
//! - [`doctor`]: the GitHub checks of `singlelane doctor`.

pub mod api;
pub mod auth;
pub mod claim;
pub mod doctor;
pub mod error;
pub mod fake;
pub mod http;
pub mod labels;
pub mod octo;
pub mod secret;
pub mod task;
pub mod types;

pub use api::GitHub;
pub use auth::{AppAuth, AppKey, TokenProvider};
pub use error::{GithubError, Result};
pub use fake::FakeGitHub;
pub use octo::OctoGitHub;
pub use secret::Secret;

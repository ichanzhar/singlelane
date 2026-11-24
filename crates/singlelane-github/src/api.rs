//! The [`GitHub`] trait: exactly the GitHub operations the factory needs.
//!
//! G3 by construction: there is no method to merge a PR, approve a PR, submit a review,
//! push, or change branch protection. Code that holds a `&dyn GitHub` cannot do any of it.

use async_trait::async_trait;
use singlelane_core::repo::RepoName;

use crate::error::Result;
use crate::types::{
    AppIdentity, AppInfo, BotUser, Installation, Issue, IssueComment, LabelChange, LabelInfo,
    NewPullRequest, PullRequest, Review, ReviewComment,
};

/// GitHub operations used by the factory. Implemented by [`crate::OctoGitHub`] (real API) and
/// [`crate::FakeGitHub`] (in memory, for tests).
#[async_trait]
pub trait GitHub: Send + Sync {
    // ---------------------------------------------------------------- app and installation

    /// `GET /app` (authenticated with the app JWT).
    async fn app(&self) -> Result<AppInfo>;

    /// `GET /users/<slug>[bot]`: the app's bot user.
    async fn bot_user(&self, app_slug: &str) -> Result<BotUser>;

    /// App and bot user together (the git identity for agent commits).
    async fn app_identity(&self) -> Result<AppIdentity> {
        let app = self.app().await?;
        let bot = self.bot_user(&app.slug).await?;
        Ok(AppIdentity { app, bot })
    }

    /// `GET /app/installations/<id>`: the installation's permissions and state.
    async fn installation(&self) -> Result<Installation>;

    /// `GET /installation/repositories`: `owner/name` of every repository the installation
    /// covers (all pages).
    async fn installation_repositories(&self) -> Result<Vec<String>>;

    // ---------------------------------------------------------------- reading items

    /// Open issues (not PRs) with `label`, optionally only those created by `creator`.
    /// Sorted by number, ascending. All pages.
    async fn list_issues(
        &self,
        repo: &RepoName,
        label: &str,
        creator: Option<&str>,
    ) -> Result<Vec<Issue>>;

    /// Open PRs, optionally only those carrying `label`. Sorted by number. All pages.
    async fn list_pull_requests(
        &self,
        repo: &RepoName,
        label: Option<&str>,
    ) -> Result<Vec<PullRequest>>;

    /// One issue with its labels. Errors with 404 for a PR number.
    async fn get_issue(&self, repo: &RepoName, number: u64) -> Result<Issue>;

    /// One PR with its labels.
    async fn get_pull_request(&self, repo: &RepoName, number: u64) -> Result<PullRequest>;

    /// Conversation comments of an issue or PR, oldest first. All pages.
    async fn list_issue_comments(&self, repo: &RepoName, number: u64) -> Result<Vec<IssueComment>>;

    /// Submitted reviews of a PR, oldest first. All pages.
    async fn list_reviews(&self, repo: &RepoName, pr: u64) -> Result<Vec<Review>>;

    /// Inline review comments of a PR, oldest first. All pages.
    async fn list_review_comments(&self, repo: &RepoName, pr: u64) -> Result<Vec<ReviewComment>>;

    /// The open PR whose head is `branch` in `repo` itself, if any.
    async fn find_open_pr_by_head(
        &self,
        repo: &RepoName,
        branch: &str,
    ) -> Result<Option<PullRequest>>;

    // ---------------------------------------------------------------- writing

    /// Adds labels to an issue or PR.
    async fn add_labels(&self, repo: &RepoName, number: u64, labels: &[String]) -> Result<()>;

    /// Removes a label from an issue or PR. `Ok(false)` if the label was not on the item
    /// (GitHub answers 404); the claim relies on that to detect a lost race (G5).
    async fn remove_label(&self, repo: &RepoName, number: u64, label: &str) -> Result<bool>;

    /// Posts a comment on an issue or PR; returns the comment id. Implementations redact the
    /// body before sending (G8).
    async fn create_comment(&self, repo: &RepoName, number: u64, body: &str) -> Result<u64>;

    /// Opens a PR, then adds its labels and requests reviews. The head must be an `agent/`
    /// branch (G3); anything else is refused without a request. Title and body are redacted
    /// (G8). A failed review request is logged, not returned.
    async fn create_pull_request(
        &self,
        repo: &RepoName,
        pr: &NewPullRequest,
    ) -> Result<PullRequest>;

    /// Closes a PR without merging (used when the owner closes the task's issue, SPEC 3).
    async fn close_pull_request(&self, repo: &RepoName, number: u64) -> Result<()>;

    // ---------------------------------------------------------------- repo setup

    /// The branch's `protected` flag (`GET /repos/../branches/<branch>`).
    async fn branch_protected(&self, repo: &RepoName, branch: &str) -> Result<bool>;

    /// All labels of a repository. All pages.
    async fn list_labels(&self, repo: &RepoName) -> Result<Vec<LabelInfo>>;

    /// Creates the label, or updates its color and description if they differ.
    async fn create_or_update_label(
        &self,
        repo: &RepoName,
        label: &LabelInfo,
    ) -> Result<LabelChange>;
}

#[cfg(test)]
mod tests {
    /// G3 by construction: the trait and the real implementation contain no merge or approve
    /// operation. A source scan, so adding one makes this test fail.
    #[test]
    fn g3_github_layer_has_no_merge_or_approve() {
        let sources = [
            ("api.rs", include_str!("api.rs")),
            ("octo.rs", include_str!("octo.rs")),
            ("fake.rs", include_str!("fake.rs")),
        ];
        // Assembled at runtime so the test itself doesn't match.
        let forbidden = [
            format!("fn {}", "merge"),
            format!("fn {}", "approve"),
            format!("/{}", "merge\""),
            format!("/{}", "merge\","),
            format!("\"{}\"", "APPROVE"),
            format!("{}_pull_request", "merge"),
            format!("submit_{}", "review"),
            format!("{}/", "/git/refs"),
            format!("/{}", "protection"),
        ];
        for (name, src) in sources {
            // Only the code, not this test module.
            let code = src.split("#[cfg(test)]").next().unwrap_or(src);
            for f in &forbidden {
                assert!(!code.contains(f.as_str()), "{name} contains {f:?}");
            }
        }
    }
}

//! Data types of the GitHub layer, independent of the HTTP client and of the JSON wire format.

use std::collections::BTreeMap;

use singlelane_core::inflight::{OpenIssue, OpenPr};
use singlelane_core::repo::{RepoName, same_login};

/// An issue (never a PR; the issues endpoints' PR entries are filtered out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Repository.
    pub repo: RepoName,
    /// Number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Body (`None` when empty).
    pub body: Option<String>,
    /// Author login (`None` for deleted users).
    pub author: Option<String>,
    /// Label names.
    pub labels: Vec<String>,
    /// Open or closed.
    pub open: bool,
}

impl Issue {
    /// Whether the issue carries `label` (case-insensitive, as on GitHub).
    pub fn has_label(&self, label: &str) -> bool {
        has_label(&self.labels, label)
    }

    /// The in-flight rules' view of this issue.
    pub fn to_open_issue(&self) -> OpenIssue {
        OpenIssue {
            repo: self.repo.clone(),
            number: self.number,
            title: self.title.clone(),
            author: self.author.clone().unwrap_or_default(),
            labels: self.labels.clone(),
        }
    }
}

/// A pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// Repository.
    pub repo: RepoName,
    /// Number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Body.
    pub body: Option<String>,
    /// Author login.
    pub author: Option<String>,
    /// Label names.
    pub labels: Vec<String>,
    /// Open or closed.
    pub open: bool,
    /// Merged (only known for single-PR reads; `false` in lists).
    pub merged: bool,
    /// Head branch name.
    pub head_ref: String,
    /// `owner/name` of the head repository (`None` if the fork was deleted).
    pub head_repo: Option<String>,
    /// Base branch name.
    pub base_ref: String,
    /// Web URL.
    pub html_url: String,
}

impl PullRequest {
    /// Whether the head branch lives in the PR's own repository (not a fork).
    pub fn head_in_repo(&self) -> bool {
        self.head_repo
            .as_deref()
            .and_then(|r| RepoName::parse(r).ok())
            .is_some_and(|r| r == self.repo)
    }

    /// Whether the PR carries `label` (case-insensitive).
    pub fn has_label(&self, label: &str) -> bool {
        has_label(&self.labels, label)
    }

    /// The in-flight rules' view of this PR. `linked_issue_open` is looked up by the caller.
    pub fn to_open_pr(&self, linked_issue_open: Option<bool>) -> OpenPr {
        OpenPr {
            repo: self.repo.clone(),
            number: self.number,
            head_branch: self.head_ref.clone(),
            head_in_repo: self.head_in_repo(),
            labels: self.labels.clone(),
            linked_issue_open,
        }
    }
}

fn has_label(labels: &[String], label: &str) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case(label))
}

/// An issue or PR conversation comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueComment {
    /// Comment id.
    pub id: u64,
    /// Author login.
    pub author: Option<String>,
    /// Markdown body.
    pub body: String,
}

/// A submitted PR review (its summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// Review id.
    pub id: u64,
    /// Author login.
    pub author: Option<String>,
    /// Summary body (empty when the reviewer wrote none).
    pub body: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, ...
    pub state: String,
}

/// An inline review comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    /// Comment id.
    pub id: u64,
    /// Author login.
    pub author: Option<String>,
    /// File path.
    pub path: String,
    /// `line`, else `original_line`.
    pub line: Option<u64>,
    /// Markdown body.
    pub body: String,
}

/// A PR to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPullRequest {
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
    /// Base branch.
    pub base: String,
    /// Head branch; must start with `agent/` (G3).
    pub head: String,
    /// Labels to add after creation.
    pub labels: Vec<String>,
    /// Users to request a review from.
    pub reviewers: Vec<String>,
}

/// The GitHub App itself (`GET /app`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    /// App id.
    pub id: u64,
    /// URL slug.
    pub slug: String,
    /// Display name.
    pub name: String,
}

impl AppInfo {
    /// The login of the app's bot user: `<slug>[bot]`.
    pub fn bot_login(&self) -> String {
        format!("{}[bot]", self.slug)
    }
}

/// The app's bot user (`GET /users/<slug>[bot]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotUser {
    /// `<slug>[bot]`.
    pub login: String,
    /// User id (not the app id).
    pub id: u64,
}

/// App and bot user together: the git identity of agent commits (SPEC 5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    /// The app.
    pub app: AppInfo,
    /// Its bot user.
    pub bot: BotUser,
}

impl AppIdentity {
    /// `user.name` for agent commits: `<slug>[bot]`.
    pub fn git_name(&self) -> String {
        self.bot.login.clone()
    }

    /// `user.email` for agent commits: `<bot-user-id>+<slug>[bot]@users.noreply.github.com`.
    pub fn git_email(&self) -> String {
        format!(
            "{}+{}@users.noreply.github.com",
            self.bot.id, self.bot.login
        )
    }

    /// Whether `login` is this app's bot user.
    pub fn is_bot(&self, login: &str) -> bool {
        same_login(login, &self.bot.login)
    }
}

/// The installation (`GET /app/installations/<id>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// Installation id.
    pub id: u64,
    /// Account the app is installed on.
    pub account: String,
    /// Granted permissions: name to `read` / `write` / `admin`.
    pub permissions: BTreeMap<String, String>,
    /// `selected` or `all`.
    pub repository_selection: String,
    /// Whether the installation is suspended.
    pub suspended: bool,
}

/// A repository label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelInfo {
    /// Name.
    pub name: String,
    /// Hex color without `#`.
    pub color: String,
    /// Description.
    pub description: String,
}

impl LabelInfo {
    /// Whether `other` has the same color and description (color case-insensitive).
    pub fn same_look(&self, other: &LabelInfo) -> bool {
        self.color.eq_ignore_ascii_case(&other.color) && self.description == other.description
    }
}

/// What `create_or_update_label` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelChange {
    /// The label did not exist.
    Created,
    /// Color or description changed.
    Updated,
    /// Already as wanted.
    Unchanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoName {
        RepoName::parse("Owner/Repo").unwrap()
    }

    #[test]
    fn git_identity_format() {
        let id = AppIdentity {
            app: AppInfo {
                id: 1,
                slug: "my-factory".into(),
                name: "My Factory".into(),
            },
            bot: BotUser {
                login: "my-factory[bot]".into(),
                id: 4242,
            },
        };
        assert_eq!(id.app.bot_login(), "my-factory[bot]");
        assert_eq!(id.git_name(), "my-factory[bot]");
        assert_eq!(
            id.git_email(),
            "4242+my-factory[bot]@users.noreply.github.com"
        );
        assert!(id.is_bot("My-Factory[bot]"));
        assert!(!id.is_bot("my-factory"));
    }

    #[test]
    fn head_in_repo_compares_case_insensitively() {
        let pr = PullRequest {
            repo: repo(),
            number: 1,
            title: String::new(),
            body: None,
            author: None,
            labels: vec!["Agent-PR".into()],
            open: true,
            merged: false,
            head_ref: "agent/issue-1".into(),
            head_repo: Some("owner/repo".into()),
            base_ref: "main".into(),
            html_url: String::new(),
        };
        assert!(pr.head_in_repo());
        assert!(pr.has_label("agent-pr"));
        let fork = PullRequest {
            head_repo: Some("mallory/repo".into()),
            ..pr.clone()
        };
        assert!(!fork.head_in_repo());
        let gone = PullRequest {
            head_repo: None,
            ..pr
        };
        assert!(!gone.head_in_repo());
    }
}

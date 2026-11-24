//! An in-memory [`GitHub`] for tests of full flows (this crate and later phases).
//!
//! Behaves like the real API where the factory depends on it: label names are
//! case-insensitive, removing a label that is not on the item reports `false` (GitHub's 404),
//! issues and PRs share one number sequence, issue lists exclude PRs, adding an unknown label
//! creates it. Every method yields to the scheduler first, so concurrent callers interleave
//! (the G5 test relies on that).
//!
//! Test hooks: [`FakeGitHub::fail_next`] makes the next call of an operation fail with a
//! status, and [`FakeGitHub::after_next`] runs a closure on the state right after an
//! operation, to simulate someone editing the item mid-flow.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use singlelane_core::policy::redact::Redactor;
use singlelane_core::repo::{RepoName, is_agent_branch};

use crate::api::GitHub;
use crate::error::{GithubError, Result};
use crate::types::{
    AppInfo, BotUser, Installation, Issue, IssueComment, LabelChange, LabelInfo, NewPullRequest,
    PullRequest, Review, ReviewComment,
};

/// A state hook for [`FakeGitHub::after_next`].
pub type Hook = Box<dyn FnOnce(&mut FakeState) + Send>;

/// The PR part of an item.
#[derive(Debug, Clone)]
pub struct FakePr {
    /// Head branch.
    pub head_ref: String,
    /// Head repository `owner/name`.
    pub head_repo: Option<String>,
    /// Base branch.
    pub base_ref: String,
    /// Merged.
    pub merged: bool,
}

/// One issue or PR.
#[derive(Debug, Clone)]
pub struct FakeItem {
    /// Number, title, body, author, labels, open.
    pub issue: Issue,
    /// Set for PRs.
    pub pr: Option<FakePr>,
    /// Conversation comments.
    pub comments: Vec<IssueComment>,
    /// Reviews (PRs).
    pub reviews: Vec<Review>,
    /// Inline review comments (PRs).
    pub review_comments: Vec<ReviewComment>,
}

impl FakeItem {
    fn to_pr(&self) -> Option<PullRequest> {
        let pr = self.pr.as_ref()?;
        let i = &self.issue;
        Some(PullRequest {
            repo: i.repo.clone(),
            number: i.number,
            title: i.title.clone(),
            body: i.body.clone(),
            author: i.author.clone(),
            labels: i.labels.clone(),
            open: i.open,
            merged: pr.merged,
            head_ref: pr.head_ref.clone(),
            head_repo: pr.head_repo.clone(),
            base_ref: pr.base_ref.clone(),
            html_url: i.repo.pr_url(i.number),
        })
    }
}

/// One repository.
#[derive(Debug, Clone, Default)]
pub struct FakeRepo {
    /// Items by number.
    pub items: BTreeMap<u64, FakeItem>,
    /// Labels by lowercase name.
    pub labels: BTreeMap<String, LabelInfo>,
    /// Branches and their `protected` flag.
    pub branches: BTreeMap<String, bool>,
    next_number: u64,
}

/// The whole fake state. Public so hooks can edit it.
#[derive(Debug)]
pub struct FakeState {
    /// Repositories.
    pub repos: HashMap<RepoName, FakeRepo>,
    /// `GET /app`.
    pub app: AppInfo,
    /// The bot user.
    pub bot: BotUser,
    /// `GET /app/installations/<id>`.
    pub installation: Installation,
    /// `GET /installation/repositories`.
    pub installation_repos: Vec<String>,
    /// Log of mutating calls, for assertions (`add_labels o/r#1 agent-working`).
    pub calls: Vec<String>,
    next_id: u64,
}

impl FakeState {
    fn repo(&mut self, repo: &RepoName) -> Result<&mut FakeRepo> {
        self.repos
            .get_mut(repo)
            .ok_or_else(|| not_found(&format!("/repos/{repo}")))
    }

    fn item(&mut self, repo: &RepoName, number: u64) -> Result<&mut FakeItem> {
        self.repo(repo)?
            .items
            .get_mut(&number)
            .ok_or_else(|| not_found(&format!("/repos/{repo}/issues/{number}")))
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// The fake. See the module docs.
pub struct FakeGitHub {
    state: Mutex<FakeState>,
    failures: Mutex<Vec<(String, u16)>>,
    hooks: Mutex<Vec<(String, Hook)>>,
    redactor: Redactor,
}

impl std::fmt::Debug for FakeGitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeGitHub").finish_non_exhaustive()
    }
}

fn not_found(path: &str) -> GithubError {
    status_error("GET", path, 404, "Not Found")
}

fn status_error(method: &str, path: &str, status: u16, message: &str) -> GithubError {
    GithubError::Status {
        method: method.into(),
        path: path.into(),
        status,
        message: message.into(),
        request_id: "fake".into(),
        rate_limited: status == 429,
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The default app slug of the fake.
pub const FAKE_APP_SLUG: &str = "singlelane-test";

impl Default for FakeGitHub {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeGitHub {
    /// An empty fake: app `singlelane-test` (id 1), bot user id 1001, an installation with
    /// exactly the permissions of SPEC 5.1, no repositories.
    pub fn new() -> Self {
        let permissions = crate::doctor::REQUIRED_PERMISSIONS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Self {
            state: Mutex::new(FakeState {
                repos: HashMap::new(),
                app: AppInfo {
                    id: 1,
                    slug: FAKE_APP_SLUG.into(),
                    name: "singlelane test".into(),
                },
                bot: BotUser {
                    login: format!("{FAKE_APP_SLUG}[bot]"),
                    id: 1001,
                },
                installation: Installation {
                    id: 2,
                    account: "owner".into(),
                    permissions,
                    repository_selection: "selected".into(),
                    suspended: false,
                },
                installation_repos: Vec::new(),
                calls: Vec::new(),
                next_id: 10_000,
            }),
            failures: Mutex::new(Vec::new()),
            hooks: Mutex::new(Vec::new()),
            redactor: Redactor::new(),
        }
    }

    /// Runs `f` on the state (setup and assertions).
    pub fn with_state<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        f(&mut lock(&self.state))
    }

    /// Adds a repository the installation covers, with a protected `main` branch.
    pub fn add_repo(&self, repo: &RepoName) {
        self.with_state(|s| {
            let r = s.repos.entry(repo.clone()).or_default();
            r.branches.entry("main".into()).or_insert(true);
            if !s
                .installation_repos
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&repo.to_string()))
            {
                s.installation_repos.push(repo.to_string());
            }
        });
    }

    fn new_item(
        &self,
        repo: &RepoName,
        author: &str,
        title: &str,
        body: &str,
        labels: &[&str],
        pr: Option<FakePr>,
    ) -> u64 {
        self.with_state(|s| {
            let r = s.repos.entry(repo.clone()).or_default();
            r.next_number += 1;
            let number = r.next_number;
            r.items.insert(
                number,
                FakeItem {
                    issue: Issue {
                        repo: repo.clone(),
                        number,
                        title: title.into(),
                        body: (!body.is_empty()).then(|| body.to_string()),
                        author: Some(author.into()),
                        labels: labels.iter().map(|l| (*l).to_string()).collect(),
                        open: true,
                    },
                    pr,
                    comments: Vec::new(),
                    reviews: Vec::new(),
                    review_comments: Vec::new(),
                },
            );
            number
        })
    }

    /// Adds an open issue; returns its number.
    pub fn add_issue(
        &self,
        repo: &RepoName,
        author: &str,
        title: &str,
        body: &str,
        labels: &[&str],
    ) -> u64 {
        self.new_item(repo, author, title, body, labels, None)
    }

    /// Adds an open PR from `head` in the same repository into `main`; returns its number.
    pub fn add_pr(
        &self,
        repo: &RepoName,
        author: &str,
        title: &str,
        body: &str,
        head: &str,
        labels: &[&str],
    ) -> u64 {
        let pr = FakePr {
            head_ref: head.into(),
            head_repo: Some(repo.to_string()),
            base_ref: "main".into(),
            merged: false,
        };
        self.new_item(repo, author, title, body, labels, Some(pr))
    }

    /// Adds a conversation comment.
    pub fn add_comment(&self, repo: &RepoName, number: u64, author: &str, body: &str) {
        self.with_state(|s| {
            let id = s.id();
            if let Ok(item) = s.item(repo, number) {
                item.comments.push(IssueComment {
                    id,
                    author: Some(author.into()),
                    body: body.into(),
                });
            }
        });
    }

    /// Adds a submitted review.
    pub fn add_review(&self, repo: &RepoName, pr: u64, author: &str, body: &str, state: &str) {
        self.with_state(|s| {
            let id = s.id();
            if let Ok(item) = s.item(repo, pr) {
                item.reviews.push(Review {
                    id,
                    author: Some(author.into()),
                    body: body.into(),
                    state: state.into(),
                });
            }
        });
    }

    /// Adds an inline review comment.
    pub fn add_review_comment(
        &self,
        repo: &RepoName,
        pr: u64,
        author: &str,
        path: &str,
        line: Option<u64>,
        body: &str,
    ) {
        self.with_state(|s| {
            let id = s.id();
            if let Ok(item) = s.item(repo, pr) {
                item.review_comments.push(ReviewComment {
                    id,
                    author: Some(author.into()),
                    path: path.into(),
                    line,
                    body: body.into(),
                });
            }
        });
    }

    /// Current labels of an item (empty if it doesn't exist).
    pub fn labels(&self, repo: &RepoName, number: u64) -> Vec<String> {
        self.with_state(|s| {
            s.item(repo, number)
                .map(|i| i.issue.labels.clone())
                .unwrap_or_default()
        })
    }

    /// Bodies of the comments on an item.
    pub fn comment_bodies(&self, repo: &RepoName, number: u64) -> Vec<String> {
        self.with_state(|s| {
            s.item(repo, number)
                .map(|i| i.comments.iter().map(|c| c.body.clone()).collect())
                .unwrap_or_default()
        })
    }

    /// The mutating-call log.
    pub fn calls(&self) -> Vec<String> {
        self.with_state(|s| s.calls.clone())
    }

    /// Opens or closes an item.
    pub fn set_open(&self, repo: &RepoName, number: u64, open: bool) {
        self.with_state(|s| {
            if let Ok(i) = s.item(repo, number) {
                i.issue.open = open;
            }
        });
    }

    /// Makes the next call of `op` (the trait method name) fail with `status`.
    pub fn fail_next(&self, op: &str, status: u16) {
        lock(&self.failures).push((op.to_string(), status));
    }

    /// Runs `hook` on the state right after the next successful call of `op`.
    pub fn after_next(&self, op: &str, hook: impl FnOnce(&mut FakeState) + Send + 'static) {
        lock(&self.hooks).push((op.to_string(), Box::new(hook)));
    }

    /// Yields, then applies a pending failure for `op`.
    async fn enter(&self, op: &str) -> Result<()> {
        tokio::task::yield_now().await;
        let mut failures = lock(&self.failures);
        if let Some(pos) = failures.iter().position(|(o, _)| o == op) {
            let (_, status) = failures.remove(pos);
            return Err(status_error("FAKE", op, status, "injected failure"));
        }
        Ok(())
    }

    /// Runs a pending hook for `op`.
    fn leave(&self, op: &str) {
        let hook = {
            let mut hooks = lock(&self.hooks);
            hooks
                .iter()
                .position(|(o, _)| o == op)
                .map(|pos| hooks.remove(pos).1)
        };
        if let Some(h) = hook {
            h(&mut lock(&self.state));
        }
    }

    fn read<R>(&self, f: impl FnOnce(&mut FakeState) -> Result<R>) -> Result<R> {
        f(&mut lock(&self.state))
    }
}

#[async_trait]
impl GitHub for FakeGitHub {
    async fn app(&self) -> Result<AppInfo> {
        self.enter("app").await?;
        self.read(|s| Ok(s.app.clone()))
    }

    async fn bot_user(&self, app_slug: &str) -> Result<BotUser> {
        self.enter("bot_user").await?;
        self.read(|s| {
            if s.bot
                .login
                .eq_ignore_ascii_case(&format!("{app_slug}[bot]"))
            {
                Ok(s.bot.clone())
            } else {
                Err(not_found(&format!("/users/{app_slug}[bot]")))
            }
        })
    }

    async fn installation(&self) -> Result<Installation> {
        self.enter("installation").await?;
        self.read(|s| Ok(s.installation.clone()))
    }

    async fn installation_repositories(&self) -> Result<Vec<String>> {
        self.enter("installation_repositories").await?;
        self.read(|s| Ok(s.installation_repos.clone()))
    }

    async fn list_issues(
        &self,
        repo: &RepoName,
        label: &str,
        creator: Option<&str>,
    ) -> Result<Vec<Issue>> {
        self.enter("list_issues").await?;
        self.read(|s| {
            Ok(s.repo(repo)?
                .items
                .values()
                .filter(|i| i.pr.is_none() && i.issue.open && i.issue.has_label(label))
                .filter(|i| {
                    creator.is_none_or(|c| {
                        i.issue
                            .author
                            .as_deref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(c))
                    })
                })
                .map(|i| i.issue.clone())
                .collect())
        })
    }

    async fn list_pull_requests(
        &self,
        repo: &RepoName,
        label: Option<&str>,
    ) -> Result<Vec<PullRequest>> {
        self.enter("list_pull_requests").await?;
        self.read(|s| {
            Ok(s.repo(repo)?
                .items
                .values()
                .filter(|i| i.issue.open && label.is_none_or(|l| i.issue.has_label(l)))
                .filter_map(FakeItem::to_pr)
                .collect())
        })
    }

    async fn get_issue(&self, repo: &RepoName, number: u64) -> Result<Issue> {
        self.enter("get_issue").await?;
        self.read(|s| {
            let item = s.item(repo, number)?;
            if item.pr.is_some() {
                return Err(not_found(&format!("/repos/{repo}/issues/{number}")));
            }
            Ok(item.issue.clone())
        })
    }

    async fn get_pull_request(&self, repo: &RepoName, number: u64) -> Result<PullRequest> {
        self.enter("get_pull_request").await?;
        self.read(|s| {
            s.item(repo, number)?
                .to_pr()
                .ok_or_else(|| not_found(&format!("/repos/{repo}/pulls/{number}")))
        })
    }

    async fn list_issue_comments(&self, repo: &RepoName, number: u64) -> Result<Vec<IssueComment>> {
        self.enter("list_issue_comments").await?;
        self.read(|s| Ok(s.item(repo, number)?.comments.clone()))
    }

    async fn list_reviews(&self, repo: &RepoName, pr: u64) -> Result<Vec<Review>> {
        self.enter("list_reviews").await?;
        self.read(|s| Ok(s.item(repo, pr)?.reviews.clone()))
    }

    async fn list_review_comments(&self, repo: &RepoName, pr: u64) -> Result<Vec<ReviewComment>> {
        self.enter("list_review_comments").await?;
        self.read(|s| Ok(s.item(repo, pr)?.review_comments.clone()))
    }

    async fn find_open_pr_by_head(
        &self,
        repo: &RepoName,
        branch: &str,
    ) -> Result<Option<PullRequest>> {
        self.enter("find_open_pr_by_head").await?;
        self.read(|s| {
            Ok(s.repo(repo)?
                .items
                .values()
                .filter(|i| i.issue.open)
                .filter_map(FakeItem::to_pr)
                .find(|p| p.head_ref == branch && p.head_in_repo()))
        })
    }

    async fn add_labels(&self, repo: &RepoName, number: u64, labels: &[String]) -> Result<()> {
        self.enter("add_labels").await?;
        self.read(|s| {
            s.calls
                .push(format!("add_labels {repo}#{number} {}", labels.join(",")));
            let r = s.repo(repo)?;
            for l in labels {
                r.labels
                    .entry(l.to_ascii_lowercase())
                    .or_insert_with(|| LabelInfo {
                        name: l.clone(),
                        color: "ededed".into(),
                        description: String::new(),
                    });
            }
            let item = s.item(repo, number)?;
            for l in labels {
                if !item.issue.has_label(l) {
                    item.issue.labels.push(l.clone());
                }
            }
            Ok(())
        })?;
        self.leave("add_labels");
        Ok(())
    }

    async fn remove_label(&self, repo: &RepoName, number: u64, label: &str) -> Result<bool> {
        self.enter("remove_label").await?;
        let removed = self.read(|s| {
            s.calls
                .push(format!("remove_label {repo}#{number} {label}"));
            let item = s.item(repo, number)?;
            let before = item.issue.labels.len();
            item.issue.labels.retain(|l| !l.eq_ignore_ascii_case(label));
            Ok(item.issue.labels.len() != before)
        })?;
        self.leave("remove_label");
        Ok(removed)
    }

    async fn create_comment(&self, repo: &RepoName, number: u64, body: &str) -> Result<u64> {
        self.enter("create_comment").await?;
        let body = self.redactor.redact(body);
        let id = self.read(|s| {
            s.calls.push(format!("create_comment {repo}#{number}"));
            let id = s.id();
            let author = Some(s.bot.login.clone());
            s.item(repo, number)?
                .comments
                .push(IssueComment { id, author, body });
            Ok(id)
        })?;
        self.leave("create_comment");
        Ok(id)
    }

    async fn create_pull_request(
        &self,
        repo: &RepoName,
        pr: &NewPullRequest,
    ) -> Result<PullRequest> {
        self.enter("create_pull_request").await?;
        if !is_agent_branch(&pr.head) {
            return Err(GithubError::Refused(format!(
                "PR head {:?} is not an agent/ branch",
                pr.head
            )));
        }
        let (title, body) = (
            self.redactor.redact(&pr.title),
            self.redactor.redact(&pr.body),
        );
        let exists = self.read(|s| {
            Ok(s.repo(repo)?
                .items
                .values()
                .any(|i| i.issue.open && i.pr.as_ref().is_some_and(|p| p.head_ref == pr.head)))
        })?;
        if exists {
            return Err(status_error(
                "POST",
                &format!("/repos/{repo}/pulls"),
                422,
                "A pull request already exists",
            ));
        }
        let bot = self.with_state(|s| s.bot.login.clone());
        let labels: Vec<&str> = pr.labels.iter().map(String::as_str).collect();
        let number = self.new_item(
            repo,
            &bot,
            &title,
            &body,
            &labels,
            Some(FakePr {
                head_ref: pr.head.clone(),
                head_repo: Some(repo.to_string()),
                base_ref: pr.base.clone(),
                merged: false,
            }),
        );
        let created = self.read(|s| {
            s.calls
                .push(format!("create_pull_request {repo}#{number} {}", pr.head));
            s.item(repo, number)?
                .to_pr()
                .ok_or_else(|| not_found("/pulls"))
        })?;
        self.leave("create_pull_request");
        Ok(created)
    }

    async fn close_pull_request(&self, repo: &RepoName, number: u64) -> Result<()> {
        self.enter("close_pull_request").await?;
        self.read(|s| {
            s.calls.push(format!("close_pull_request {repo}#{number}"));
            let item = s.item(repo, number)?;
            if item.pr.is_none() {
                return Err(not_found(&format!("/repos/{repo}/pulls/{number}")));
            }
            item.issue.open = false;
            Ok(())
        })?;
        self.leave("close_pull_request");
        Ok(())
    }

    async fn branch_protected(&self, repo: &RepoName, branch: &str) -> Result<bool> {
        self.enter("branch_protected").await?;
        self.read(|s| {
            s.repo(repo)?
                .branches
                .get(branch)
                .copied()
                .ok_or_else(|| not_found(&format!("/repos/{repo}/branches/{branch}")))
        })
    }

    async fn list_labels(&self, repo: &RepoName) -> Result<Vec<LabelInfo>> {
        self.enter("list_labels").await?;
        self.read(|s| Ok(s.repo(repo)?.labels.values().cloned().collect()))
    }

    async fn create_or_update_label(
        &self,
        repo: &RepoName,
        label: &LabelInfo,
    ) -> Result<LabelChange> {
        self.enter("create_or_update_label").await?;
        self.read(|s| {
            s.calls
                .push(format!("create_or_update_label {repo} {}", label.name));
            let r = s.repo(repo)?;
            let key = label.name.to_ascii_lowercase();
            let change = match r.labels.get(&key) {
                None => LabelChange::Created,
                Some(l) if l.same_look(label) && l.name == label.name => LabelChange::Unchanged,
                Some(_) => LabelChange::Updated,
            };
            r.labels.insert(key, label.clone());
            Ok(change)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoName {
        RepoName::parse("owner/repo").unwrap()
    }

    #[tokio::test]
    async fn issues_and_prs_share_numbers_and_lists_are_separate() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let i = gh.add_issue(&r, "owner", "t", "b", &["agent-ready"]);
        let p = gh.add_pr(
            &r,
            "x[bot]",
            "pr",
            "",
            "agent/issue-1",
            &["agent-ready", "agent-pr"],
        );
        assert_eq!((i, p), (1, 2));
        let issues = gh
            .list_issues(&r, "AGENT-READY", Some("Owner"))
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert!(gh.get_issue(&r, p).await.unwrap_err().is_not_found());
        let prs = gh.list_pull_requests(&r, Some("agent-pr")).await.unwrap();
        assert_eq!(prs.len(), 1);
        assert!(
            gh.find_open_pr_by_head(&r, "agent/issue-1")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn remove_label_reports_absence_and_hooks_and_failures_fire() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_issue(&r, "owner", "t", "b", &["agent-ready"]);
        assert!(gh.remove_label(&r, n, "Agent-Ready").await.unwrap());
        assert!(!gh.remove_label(&r, n, "agent-ready").await.unwrap());
        gh.fail_next("add_labels", 502);
        assert_eq!(
            gh.add_labels(&r, n, &["x".into()])
                .await
                .unwrap_err()
                .status(),
            Some(502)
        );
        gh.after_next("add_labels", move |s| {
            if let Some(i) = s.repos.get_mut(&repo()).and_then(|rr| rr.items.get_mut(&n)) {
                i.issue.labels.push("hooked".into());
            }
        });
        gh.add_labels(&r, n, &["x".into()]).await.unwrap();
        assert_eq!(gh.labels(&r, n), vec!["x", "hooked"]);
    }

    #[tokio::test]
    async fn g8_fake_redacts_comments_like_the_real_client() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_issue(&r, "owner", "t", "b", &[]);
        let token = format!("ghs_{}", "Ab1Cd2Ef3G".repeat(4));
        gh.create_comment(&r, n, &format!("oops {token}"))
            .await
            .unwrap();
        assert!(!gh.comment_bodies(&r, n)[0].contains(&token));
    }

    #[tokio::test]
    async fn g3_fake_refuses_prs_from_non_agent_branches() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let pr = NewPullRequest {
            title: "t".into(),
            body: "b".into(),
            base: "main".into(),
            head: "main".into(),
            labels: vec![],
            reviewers: vec![],
        };
        assert!(matches!(
            gh.create_pull_request(&r, &pr).await,
            Err(GithubError::Refused(_))
        ));
    }
}

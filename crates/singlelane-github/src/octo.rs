//! [`GitHub`] over the REST API, via [`Http`] (octocrab with rustls).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use http::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use singlelane_core::policy::redact::Redactor;
use singlelane_core::repo::{RepoName, is_agent_branch};

use crate::api::GitHub;
use crate::auth::{AppAuth, TokenProvider};
use crate::error::{GithubError, Result};
use crate::http::{Http, encode};
use crate::types::{
    AppInfo, BotUser, Installation, Issue, IssueComment, LabelChange, LabelInfo, NewPullRequest,
    PullRequest, Review, ReviewComment,
};

/// Items per page for list endpoints (GitHub's maximum).
const PER_PAGE: u32 = 100;
/// Upper bound on pages followed, so a broken `Link` header can't loop forever.
const MAX_PAGES: usize = 100;

/// Which credential a request uses.
#[derive(Debug, Clone, Copy)]
enum Cred {
    /// The app JWT (`/app` endpoints).
    App,
    /// The installation token (everything else).
    Installation,
}

/// The real GitHub implementation.
pub struct OctoGitHub {
    http: Http,
    auth: Arc<AppAuth>,
    redactor: Redactor,
}

impl std::fmt::Debug for OctoGitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OctoGitHub")
            .field("http", &self.http)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl OctoGitHub {
    /// A client using `http` for transport and `auth` for credentials. `redactor` is applied
    /// to every comment, PR title and PR body before it is sent (G8); pattern redaction is
    /// always on, known secrets come from the caller (the claude env file values).
    pub fn new(http: Http, auth: Arc<AppAuth>, redactor: Redactor) -> Self {
        Self {
            http,
            auth,
            redactor,
        }
    }

    /// The token provider (for git and `gh` in later phases).
    pub fn auth(&self) -> Arc<AppAuth> {
        Arc::clone(&self.auth)
    }

    async fn send(
        &self,
        method: Method,
        target: &str,
        cred: Cred,
        body: Option<&Value>,
    ) -> Result<crate::http::Response> {
        let secret = match cred {
            Cred::App => self.auth.jwt()?,
            Cred::Installation => self.auth.installation_token().await?,
        };
        self.http.request(method, target, &secret, body).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, cred: Cred) -> Result<T> {
        self.send(Method::GET, path, cred, None)
            .await?
            .json(&Method::GET, path)
    }

    /// Follows `Link: rel="next"` and collects every page. `extract` turns one page body into
    /// items (most endpoints return an array; some wrap it in an object).
    async fn get_all<T, R, F>(&self, first: &str, extract: F) -> Result<Vec<T>>
    where
        R: DeserializeOwned,
        F: Fn(R) -> Vec<T>,
    {
        let mut out = Vec::new();
        let mut next = Some(first.to_string());
        let mut pages = 0usize;
        while let Some(target) = next {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(GithubError::Decode {
                    method: "GET".into(),
                    path: first.to_string(),
                    message: format!("more than {MAX_PAGES} pages"),
                });
            }
            let resp = self
                .send(Method::GET, &target, Cred::Installation, None)
                .await?;
            let page: R = resp.json(&Method::GET, &target)?;
            out.extend(extract(page));
            next = resp.next_link();
        }
        Ok(out)
    }
}

fn repo_path(repo: &RepoName) -> String {
    format!("/repos/{}/{}", encode(repo.owner()), encode(repo.name()))
}

/// Encodes a branch name for a path, keeping `/` as GitHub expects in branch routes.
fn branch_path(branch: &str) -> String {
    branch.split('/').map(encode).collect::<Vec<_>>().join("/")
}

// ---------------------------------------------------------------------- wire types

#[derive(Debug, Deserialize)]
struct RawUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct RawLabel {
    name: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    state: String,
    #[serde(default)]
    pull_request: Option<Value>,
}

impl RawIssue {
    fn into_issue(self, repo: &RepoName) -> Issue {
        Issue {
            repo: repo.clone(),
            number: self.number,
            title: self.title,
            body: self.body.filter(|b| !b.is_empty()),
            author: self.user.map(|u| u.login),
            labels: self.labels.into_iter().map(|l| l.name).collect(),
            open: self.state == "open",
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawRepoRef {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct RawBranchRef {
    #[serde(rename = "ref")]
    name: String,
    #[serde(default)]
    repo: Option<RawRepoRef>,
}

#[derive(Debug, Deserialize)]
struct RawPull {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    state: String,
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default)]
    merged_at: Option<String>,
    head: RawBranchRef,
    base: RawBranchRef,
    #[serde(default)]
    html_url: String,
}

impl RawPull {
    fn into_pr(self, repo: &RepoName) -> PullRequest {
        PullRequest {
            repo: repo.clone(),
            number: self.number,
            title: self.title,
            body: self.body.filter(|b| !b.is_empty()),
            author: self.user.map(|u| u.login),
            labels: self.labels.into_iter().map(|l| l.name).collect(),
            open: self.state == "open",
            merged: self.merged.unwrap_or(false) || self.merged_at.is_some(),
            head_ref: self.head.name,
            head_repo: self.head.repo.map(|r| r.full_name),
            base_ref: self.base.name,
            html_url: self.html_url,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawComment {
    id: u64,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawReview {
    id: u64,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: String,
}

#[derive(Debug, Deserialize)]
struct RawReviewComment {
    id: u64,
    #[serde(default)]
    user: Option<RawUser>,
    path: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct RawApp {
    id: u64,
    slug: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawBotUser {
    login: String,
    id: u64,
}

#[derive(Debug, Deserialize)]
struct RawInstallation {
    id: u64,
    #[serde(default)]
    account: Option<RawUser>,
    #[serde(default)]
    permissions: BTreeMap<String, String>,
    #[serde(default)]
    repository_selection: String,
    #[serde(default)]
    suspended_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawInstallationRepos {
    #[serde(default)]
    repositories: Vec<RawRepoRef>,
}

#[derive(Debug, Deserialize)]
struct RawBranch {
    #[serde(default)]
    protected: bool,
}

#[derive(Debug, Deserialize)]
struct RawCreated {
    id: u64,
}

fn label_info(l: RawLabel) -> LabelInfo {
    LabelInfo {
        name: l.name,
        color: l.color,
        description: l.description.unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------- the trait

#[async_trait]
impl GitHub for OctoGitHub {
    async fn app(&self) -> Result<AppInfo> {
        let raw: RawApp = self.get("/app", Cred::App).await?;
        Ok(AppInfo {
            id: raw.id,
            slug: raw.slug,
            name: raw.name,
        })
    }

    async fn bot_user(&self, app_slug: &str) -> Result<BotUser> {
        let path = format!("/users/{}", encode(&format!("{app_slug}[bot]")));
        let raw: RawBotUser = self.get(&path, Cred::Installation).await?;
        Ok(BotUser {
            login: raw.login,
            id: raw.id,
        })
    }

    async fn installation(&self) -> Result<Installation> {
        let path = format!("/app/installations/{}", self.auth.installation_id());
        let raw: RawInstallation = self.get(&path, Cred::App).await?;
        Ok(Installation {
            id: raw.id,
            account: raw.account.map(|a| a.login).unwrap_or_default(),
            permissions: raw.permissions,
            repository_selection: raw.repository_selection,
            suspended: raw.suspended_at.is_some(),
        })
    }

    async fn installation_repositories(&self) -> Result<Vec<String>> {
        let first = format!("/installation/repositories?per_page={PER_PAGE}");
        self.get_all(&first, |page: RawInstallationRepos| {
            page.repositories.into_iter().map(|r| r.full_name).collect()
        })
        .await
    }

    async fn list_issues(
        &self,
        repo: &RepoName,
        label: &str,
        creator: Option<&str>,
    ) -> Result<Vec<Issue>> {
        let mut first = format!(
            "{}/issues?state=open&labels={}&per_page={PER_PAGE}",
            repo_path(repo),
            encode(label)
        );
        if let Some(c) = creator {
            first.push_str(&format!("&creator={}", encode(c)));
        }
        let mut issues = self
            .get_all(&first, |page: Vec<RawIssue>| {
                page.into_iter()
                    .filter(|i| i.pull_request.is_none())
                    .map(|i| i.into_issue(repo))
                    .collect()
            })
            .await?;
        issues.sort_by_key(|i| i.number);
        Ok(issues)
    }

    async fn list_pull_requests(
        &self,
        repo: &RepoName,
        label: Option<&str>,
    ) -> Result<Vec<PullRequest>> {
        let first = format!("{}/pulls?state=open&per_page={PER_PAGE}", repo_path(repo));
        let mut prs = self
            .get_all(&first, |page: Vec<RawPull>| {
                page.into_iter().map(|p| p.into_pr(repo)).collect()
            })
            .await?;
        if let Some(l) = label {
            prs.retain(|p: &PullRequest| p.has_label(l));
        }
        prs.sort_by_key(|p| p.number);
        Ok(prs)
    }

    async fn get_issue(&self, repo: &RepoName, number: u64) -> Result<Issue> {
        let path = format!("{}/issues/{number}", repo_path(repo));
        let raw: RawIssue = self.get(&path, Cred::Installation).await?;
        if raw.pull_request.is_some() {
            return Err(GithubError::Status {
                method: "GET".into(),
                path,
                status: 404,
                message: format!("#{number} is a pull request, not an issue"),
                request_id: "-".into(),
                rate_limited: false,
            });
        }
        Ok(raw.into_issue(repo))
    }

    async fn get_pull_request(&self, repo: &RepoName, number: u64) -> Result<PullRequest> {
        let path = format!("{}/pulls/{number}", repo_path(repo));
        let raw: RawPull = self.get(&path, Cred::Installation).await?;
        Ok(raw.into_pr(repo))
    }

    async fn list_issue_comments(&self, repo: &RepoName, number: u64) -> Result<Vec<IssueComment>> {
        let first = format!(
            "{}/issues/{number}/comments?per_page={PER_PAGE}",
            repo_path(repo)
        );
        self.get_all(&first, |page: Vec<RawComment>| {
            page.into_iter()
                .map(|c| IssueComment {
                    id: c.id,
                    author: c.user.map(|u| u.login),
                    body: c.body.unwrap_or_default(),
                })
                .collect()
        })
        .await
    }

    async fn list_reviews(&self, repo: &RepoName, pr: u64) -> Result<Vec<Review>> {
        let first = format!("{}/pulls/{pr}/reviews?per_page={PER_PAGE}", repo_path(repo));
        self.get_all(&first, |page: Vec<RawReview>| {
            page.into_iter()
                .map(|r| Review {
                    id: r.id,
                    author: r.user.map(|u| u.login),
                    body: r.body.unwrap_or_default(),
                    state: r.state,
                })
                .collect()
        })
        .await
    }

    async fn list_review_comments(&self, repo: &RepoName, pr: u64) -> Result<Vec<ReviewComment>> {
        let first = format!(
            "{}/pulls/{pr}/comments?per_page={PER_PAGE}",
            repo_path(repo)
        );
        self.get_all(&first, |page: Vec<RawReviewComment>| {
            page.into_iter()
                .map(|c| ReviewComment {
                    id: c.id,
                    author: c.user.map(|u| u.login),
                    path: c.path,
                    line: c.line.or(c.original_line),
                    body: c.body,
                })
                .collect()
        })
        .await
    }

    async fn find_open_pr_by_head(
        &self,
        repo: &RepoName,
        branch: &str,
    ) -> Result<Option<PullRequest>> {
        let first = format!(
            "{}/pulls?state=open&head={}&per_page={PER_PAGE}",
            repo_path(repo),
            encode(&format!("{}:{branch}", repo.owner()))
        );
        let prs = self
            .get_all(&first, |page: Vec<RawPull>| {
                page.into_iter().map(|p| p.into_pr(repo)).collect()
            })
            .await?;
        Ok(prs
            .into_iter()
            .find(|p| p.head_ref == branch && p.head_in_repo()))
    }

    async fn add_labels(&self, repo: &RepoName, number: u64, labels: &[String]) -> Result<()> {
        if labels.is_empty() {
            return Ok(());
        }
        let path = format!("{}/issues/{number}/labels", repo_path(repo));
        self.send(
            Method::POST,
            &path,
            Cred::Installation,
            Some(&json!({ "labels": labels })),
        )
        .await?;
        Ok(())
    }

    async fn remove_label(&self, repo: &RepoName, number: u64, label: &str) -> Result<bool> {
        let path = format!(
            "{}/issues/{number}/labels/{}",
            repo_path(repo),
            encode(label)
        );
        match self
            .send(Method::DELETE, &path, Cred::Installation, None)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn create_comment(&self, repo: &RepoName, number: u64, body: &str) -> Result<u64> {
        let path = format!("{}/issues/{number}/comments", repo_path(repo));
        let body = self.redactor.redact(body);
        let resp = self
            .send(
                Method::POST,
                &path,
                Cred::Installation,
                Some(&json!({ "body": body })),
            )
            .await?;
        let created: RawCreated = resp.json(&Method::POST, &path)?;
        Ok(created.id)
    }

    async fn create_pull_request(
        &self,
        repo: &RepoName,
        pr: &NewPullRequest,
    ) -> Result<PullRequest> {
        if !is_agent_branch(&pr.head) {
            return Err(GithubError::Refused(format!(
                "PR head {:?} is not an agent/ branch",
                pr.head
            )));
        }
        let path = format!("{}/pulls", repo_path(repo));
        let payload = json!({
            "title": self.redactor.redact(&pr.title),
            "body": self.redactor.redact(&pr.body),
            "head": pr.head,
            "base": pr.base,
        });
        let resp = self
            .send(Method::POST, &path, Cred::Installation, Some(&payload))
            .await?;
        let mut created = resp.json::<RawPull>(&Method::POST, &path)?.into_pr(repo);
        tracing::info!(repo = %repo, pr = created.number, head = %pr.head, "opened PR");

        self.add_labels(repo, created.number, &pr.labels).await?;
        for l in &pr.labels {
            if !created.has_label(l) {
                created.labels.push(l.clone());
            }
        }
        if !pr.reviewers.is_empty() {
            let path = format!(
                "{}/pulls/{}/requested_reviewers",
                repo_path(repo),
                created.number
            );
            if let Err(e) = self
                .send(
                    Method::POST,
                    &path,
                    Cred::Installation,
                    Some(&json!({ "reviewers": pr.reviewers })),
                )
                .await
            {
                tracing::warn!(repo = %repo, pr = created.number, error = %e, "review request failed");
            }
        }
        Ok(created)
    }

    async fn close_pull_request(&self, repo: &RepoName, number: u64) -> Result<()> {
        let path = format!("{}/pulls/{number}", repo_path(repo));
        self.send(
            Method::PATCH,
            &path,
            Cred::Installation,
            Some(&json!({ "state": "closed" })),
        )
        .await?;
        Ok(())
    }

    async fn branch_protected(&self, repo: &RepoName, branch: &str) -> Result<bool> {
        let path = format!("{}/branches/{}", repo_path(repo), branch_path(branch));
        let raw: RawBranch = self.get(&path, Cred::Installation).await?;
        Ok(raw.protected)
    }

    async fn list_labels(&self, repo: &RepoName) -> Result<Vec<LabelInfo>> {
        let first = format!("{}/labels?per_page={PER_PAGE}", repo_path(repo));
        self.get_all(&first, |page: Vec<RawLabel>| {
            page.into_iter().map(label_info).collect()
        })
        .await
    }

    async fn create_or_update_label(
        &self,
        repo: &RepoName,
        label: &LabelInfo,
    ) -> Result<LabelChange> {
        let path = format!("{}/labels/{}", repo_path(repo), encode(&label.name));
        let existing = match self.get::<RawLabel>(&path, Cred::Installation).await {
            Ok(l) => Some(label_info(l)),
            Err(e) if e.is_not_found() => None,
            Err(e) => return Err(e),
        };
        let payload = json!({
            "name": label.name,
            "color": label.color,
            "description": label.description,
        });
        match existing {
            None => {
                let create = format!("{}/labels", repo_path(repo));
                self.send(Method::POST, &create, Cred::Installation, Some(&payload))
                    .await?;
                Ok(LabelChange::Created)
            }
            Some(l) if l.same_look(label) && l.name == label.name => Ok(LabelChange::Unchanged),
            Some(_) => {
                let payload = json!({
                    "new_name": label.name,
                    "color": label.color,
                    "description": label.description,
                });
                self.send(Method::PATCH, &path, Cred::Installation, Some(&payload))
                    .await?;
                Ok(LabelChange::Updated)
            }
        }
    }
}

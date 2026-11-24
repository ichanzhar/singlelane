//! `OctoGitHub` and `AppAuth` against a wiremock server with recorded-style responses.

// Test-only file: helpers outside `#[test]` fns may panic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use serde_json::{Value, json};
use singlelane_core::policy::redact::Redactor;
use singlelane_core::repo::RepoName;
use singlelane_core::schedule::FixedClock;
use singlelane_github::auth::JwtClaims;
use singlelane_github::doctor::check_permissions;
use singlelane_github::http::{Http, RetryPolicy, Sleeper};
use singlelane_github::types::{LabelChange, LabelInfo, NewPullRequest};
use singlelane_github::{AppAuth, AppKey, GitHub, GithubError, OctoGitHub, TokenProvider};
use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const APP_ID: u64 = 123_456;
const INSTALLATION_ID: u64 = 12_345_678;
const NOW: &str = "2026-11-24T10:00:00Z";

/// A fake installation token, built at runtime so secret scanners don't flag this file.
fn token() -> String {
    format!("ghs_{}", "T0k3nV4lu3".repeat(4))
}

fn key_pem() -> String {
    let body = include_str!("fixtures/test-app-key.pkcs1.b64");
    let marker = |edge: &str| format!("-----{edge} RSA {} KEY-----", "PRIVATE");
    format!("{}\n{}{}\n", marker("BEGIN"), body, marker("END"))
}

fn fixture(name: &str, server: &MockServer) -> Value {
    let text = match name {
        "access_token" => include_str!("fixtures/api/access_token.json"),
        "app" => include_str!("fixtures/api/app.json"),
        "bot_user" => include_str!("fixtures/api/bot_user.json"),
        "installation" => include_str!("fixtures/api/installation.json"),
        "installation_repositories" => include_str!("fixtures/api/installation_repositories.json"),
        "issues_ready" => include_str!("fixtures/api/issues_ready.json"),
        "pull" => include_str!("fixtures/api/pull.json"),
        "issue_comments_page1" => include_str!("fixtures/api/issue_comments_page1.json"),
        "issue_comments_page2" => include_str!("fixtures/api/issue_comments_page2.json"),
        "reviews" => include_str!("fixtures/api/reviews.json"),
        "review_comments" => include_str!("fixtures/api/review_comments.json"),
        "branch_main" => include_str!("fixtures/api/branch_main.json"),
        "label" => include_str!("fixtures/api/label.json"),
        "comment_created" => include_str!("fixtures/api/comment_created.json"),
        other => panic!("no fixture {other}"),
    };
    let text = text
        .replace("{{BASE}}", &server.uri())
        .replace("{{TOKEN}}", &token())
        .replace("{{EXPIRES_AT}}", "2026-11-24T11:00:00Z");
    serde_json::from_str(&text).unwrap()
}

#[derive(Default)]
struct RecordingSleeper(Mutex<Vec<Duration>>);

#[async_trait]
impl Sleeper for RecordingSleeper {
    async fn sleep(&self, d: Duration) {
        self.0.lock().unwrap().push(d);
    }
}

impl RecordingSleeper {
    fn delays(&self) -> Vec<Duration> {
        self.0.lock().unwrap().clone()
    }
}

struct Env {
    server: MockServer,
    clock: Arc<FixedClock>,
    sleeper: Arc<RecordingSleeper>,
    auth: Arc<AppAuth>,
    gh: OctoGitHub,
}

/// Matches `Authorization: Bearer <JWT for APP_ID>`.
struct AppJwt;

impl Match for AppJwt {
    fn matches(&self, request: &Request) -> bool {
        let Some(value) = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let Some(jwt) = value.strip_prefix("Bearer ") else {
            return false;
        };
        let parts: Vec<&str> = jwt.split('.').collect();
        if parts.len() != 3 {
            return false;
        }
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(parts[1]) else {
            return false;
        };
        serde_json::from_slice::<JwtClaims>(&bytes).is_ok_and(|c| c.iss == APP_ID)
    }
}

fn bearer_token() -> (String, String) {
    ("authorization".into(), format!("Bearer {}", token()))
}

async fn env() -> Env {
    let server = MockServer::start().await;
    let clock = Arc::new(FixedClock::new(NOW.parse().unwrap()));
    let sleeper = Arc::new(RecordingSleeper::default());
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(90),
    };
    let http = Http::new(&server.uri(), policy, clock.clone())
        .unwrap()
        .with_sleeper(sleeper.clone());
    let key = AppKey::from_pem(key_pem().as_bytes()).unwrap();
    let auth = Arc::new(AppAuth::new(
        APP_ID,
        INSTALLATION_ID,
        key,
        http.clone(),
        clock.clone(),
    ));
    let gh = OctoGitHub::new(http, auth.clone(), Redactor::new());
    Env {
        server,
        clock,
        sleeper,
        auth,
        gh,
    }
}

async fn mount_token(env: &Env) {
    Mock::given(method("POST"))
        .and(path(format!(
            "/app/installations/{INSTALLATION_ID}/access_tokens"
        )))
        .and(AppJwt)
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(fixture("access_token", &env.server))
                .insert_header("x-github-request-id", "TOKEN:1"),
        )
        .mount(&env.server)
        .await;
}

fn repo() -> RepoName {
    RepoName::parse("owner/monorepo").unwrap()
}

async fn token_requests(env: &Env) -> usize {
    env.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/access_tokens"))
        .count()
}

// ------------------------------------------------------------------------- auth

#[tokio::test]
async fn app_identity_uses_jwt_then_installation_token() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/app"))
        .and(AppJwt)
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("app", &env.server)))
        .expect(1)
        .mount(&env.server)
        .await;
    let (k, v) = bearer_token();
    Mock::given(method("GET"))
        .and(path("/users/singlelane-test%5Bbot%5D"))
        .and(header(k.as_str(), v.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("bot_user", &env.server)))
        .expect(1)
        .mount(&env.server)
        .await;

    let id = env.gh.app_identity().await.unwrap();
    assert_eq!(id.app.id, APP_ID);
    assert_eq!(id.app.slug, "singlelane-test");
    assert_eq!(id.git_name(), "singlelane-test[bot]");
    assert_eq!(
        id.git_email(),
        "987654+singlelane-test[bot]@users.noreply.github.com"
    );
}

#[tokio::test]
async fn installation_token_is_cached_and_refreshed_before_expiry() {
    let env = env().await;
    mount_token(&env).await;

    // Minted at 10:00, expires 11:00.
    assert_eq!(
        env.auth.installation_token().await.unwrap().expose(),
        token()
    );
    env.auth.installation_token().await.unwrap();
    assert_eq!(token_requests(&env).await, 1, "second call uses the cache");

    // 10:49: 11 minutes left, still cached.
    env.clock.advance(Duration::from_secs(49 * 60));
    env.auth.installation_token().await.unwrap();
    assert_eq!(token_requests(&env).await, 1);

    // 10:51: 9 minutes left, below the 10 minute margin: refreshed.
    env.clock.advance(Duration::from_secs(2 * 60));
    env.auth.installation_token().await.unwrap();
    assert_eq!(token_requests(&env).await, 2);

    // A fresh token is always minted (before publish).
    env.auth.fresh_installation_token().await.unwrap();
    assert_eq!(token_requests(&env).await, 3);
}

#[tokio::test]
async fn g8_rejected_jwt_error_contains_no_secret() {
    let env = env().await;
    // GitHub echoing credentials back would be unusual; redact anyway.
    let echo = format!(
        "{{\"message\":\"A JSON web token could not be decoded: {} / {}\"}}",
        token(),
        env.auth.jwt().unwrap().expose()
    );
    Mock::given(method("POST"))
        .and(path(format!(
            "/app/installations/{INSTALLATION_ID}/access_tokens"
        )))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_raw(echo, "application/json")
                .insert_header("x-github-request-id", "AUTH:FAIL"),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    let err = env.auth.installation_token().await.unwrap_err();
    assert!(err.is_auth_failure());
    let text = format!("{err} {err:?}");
    assert!(!text.contains(&token()), "{text}");
    assert!(!text.contains("eyJ"), "{text}");
    assert!(text.contains("AUTH:FAIL"), "request id is kept: {text}");
    assert!(!format!("{:?} {:?}", env.auth, env.gh).contains(&token()));
}

// ------------------------------------------------------------------------- installation

#[tokio::test]
async fn installation_permissions_and_repositories() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path(format!("/app/installations/{INSTALLATION_ID}")))
        .and(AppJwt)
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("installation", &env.server)),
        )
        .mount(&env.server)
        .await;
    let (k, v) = bearer_token();
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param("per_page", "100"))
        .and(header(k.as_str(), v.as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fixture("installation_repositories", &env.server)),
        )
        .mount(&env.server)
        .await;

    let inst = env.gh.installation().await.unwrap();
    assert_eq!(inst.account, "owner");
    assert_eq!(inst.repository_selection, "selected");
    assert!(!inst.suspended);
    // The recorded installation has an extra `workflows` permission: doctor must fail.
    let problems = check_permissions(&inst.permissions).unwrap_err();
    assert_eq!(problems.extra, vec!["workflows (write)"]);

    let repos = env.gh.installation_repositories().await.unwrap();
    assert_eq!(repos, vec!["owner/monorepo", "owner/Other.Repo"]);
}

// ------------------------------------------------------------------------- reading

#[tokio::test]
async fn list_issues_filters_prs_and_sorts() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/issues"))
        .and(query_param("state", "open"))
        .and(query_param("labels", "agent-ready"))
        .and(query_param("creator", "owner"))
        .and(query_param("per_page", "100"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("issues_ready", &env.server)),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    let issues = env
        .gh
        .list_issues(&repo(), "agent-ready", Some("owner"))
        .await
        .unwrap();
    let numbers: Vec<u64> = issues.iter().map(|i| i.number).collect();
    assert_eq!(numbers, vec![7, 12], "PR #9 dropped, sorted ascending");
    assert_eq!(issues[0].body, None, "empty body is None");
    assert_eq!(issues[1].author.as_deref(), Some("owner"));
    assert!(issues[1].has_label("agent-ready") && issues[1].has_label("enhancement"));
    assert!(issues[1].open);
}

#[tokio::test]
async fn get_pull_request_and_find_by_head() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/pulls/15"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("pull", &env.server)))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("head", "owner:agent/issue-12"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([fixture("pull", &env.server)])),
        )
        .mount(&env.server)
        .await;

    let pr = env.gh.get_pull_request(&repo(), 15).await.unwrap();
    assert_eq!(pr.head_ref, "agent/issue-12");
    assert!(pr.head_in_repo());
    assert_eq!(pr.base_ref, "main");
    assert!(pr.open && !pr.merged);
    assert!(pr.has_label("agent-revise"));
    assert_eq!(pr.author.as_deref(), Some("singlelane-test[bot]"));

    let found = env
        .gh
        .find_open_pr_by_head(&repo(), "agent/issue-12")
        .await
        .unwrap();
    assert_eq!(found.map(|p| p.number), Some(15));
}

#[tokio::test]
async fn comments_are_paginated_via_link_header() {
    let env = env().await;
    mount_token(&env).await;
    let next = format!(
        "{}/repositories/2001/issues/12/comments?per_page=100&page=2",
        env.server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/repositories/2001/issues/12/comments"))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fixture("issue_comments_page2", &env.server))
                .insert_header(
                    "link",
                    format!(
                        "<{}>; rel=\"prev\", <{}>; rel=\"first\"",
                        env.server.uri(),
                        env.server.uri()
                    ),
                ),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/issues/12/comments"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fixture("issue_comments_page1", &env.server))
                .insert_header(
                    "link",
                    format!("<{next}>; rel=\"next\", <{next}>; rel=\"last\""),
                ),
        )
        .expect(1)
        .mount(&env.server)
        .await;

    let comments = env.gh.list_issue_comments(&repo(), 12).await.unwrap();
    let ids: Vec<u64> = comments.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![5001, 5002, 5003]);
    assert_eq!(comments[1].author.as_deref(), Some("mallory"));
    assert_eq!(comments[2].author, None, "deleted user");
}

#[tokio::test]
async fn g8_pagination_never_follows_links_to_other_hosts() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/labels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([fixture("label", &env.server)]))
                .insert_header(
                    "link",
                    "<https://attacker.example/steal?page=2>; rel=\"next\"",
                ),
        )
        .mount(&env.server)
        .await;
    let err = env.gh.list_labels(&repo()).await.unwrap_err();
    assert!(matches!(err, GithubError::Refused(_)), "{err}");
}

#[tokio::test]
async fn reviews_and_review_comments() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/pulls/15/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("reviews", &env.server)))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/pulls/15/comments"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("review_comments", &env.server)),
        )
        .mount(&env.server)
        .await;
    let reviews = env.gh.list_reviews(&repo(), 15).await.unwrap();
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[0].state, "CHANGES_REQUESTED");
    assert_eq!(reviews[1].body, "");
    let rc = env.gh.list_review_comments(&repo(), 15).await.unwrap();
    assert_eq!(rc[0].line, Some(12));
    assert_eq!(rc[1].line, Some(4), "falls back to original_line");
    assert_eq!(rc[1].path, "web/src/old.ts");
}

#[tokio::test]
async fn branch_protection_flag() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("branch_main", &env.server)))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/branches/nope"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"message": "Branch not found"})),
        )
        .mount(&env.server)
        .await;
    assert!(env.gh.branch_protected(&repo(), "main").await.unwrap());
    assert!(
        env.gh
            .branch_protected(&repo(), "nope")
            .await
            .unwrap_err()
            .is_not_found()
    );
}

// ------------------------------------------------------------------------- writing

#[tokio::test]
async fn labels_add_and_remove() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/issues/12/labels"))
        .and(body_json(json!({"labels": ["agent-working"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "agent-working"}])))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/owner/monorepo/issues/12/labels/agent-ready"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&env.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/owner/monorepo/issues/12/labels/agent-ready"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"message": "Label does not exist"})),
        )
        .mount(&env.server)
        .await;
    env.gh
        .add_labels(&repo(), 12, &["agent-working".into()])
        .await
        .unwrap();
    assert!(
        env.gh
            .remove_label(&repo(), 12, "agent-ready")
            .await
            .unwrap()
    );
    assert!(
        !env.gh
            .remove_label(&repo(), 12, "agent-ready")
            .await
            .unwrap(),
        "404 means the label was not there (lost claim race)"
    );
}

#[tokio::test]
async fn g8_comment_body_is_redacted_before_sending() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/issues/12/comments"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(fixture("comment_created", &env.server)),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    let id = env
        .gh
        .create_comment(&repo(), 12, &format!("Job failed: git said {}", token()))
        .await
        .unwrap();
    assert_eq!(id, 5100);
    let requests = env.server.received_requests().await.unwrap();
    let sent = requests
        .iter()
        .find(|r| r.url.path().ends_with("/comments"))
        .unwrap();
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    let text = body["body"].as_str().unwrap();
    assert!(!text.contains(&token()), "{text}");
    assert!(text.contains("[REDACTED]"));
}

#[tokio::test]
async fn create_pull_request_adds_labels_and_survives_a_failed_review_request() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/pulls"))
        .and(body_partial_json(json!({
            "head": "agent/issue-12",
            "base": "main",
            "title": "feat: dark mode toggle"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json({
            let mut pr = fixture("pull", &env.server);
            pr["labels"] = json!([]);
            pr
        }))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/issues/15/labels"))
        .and(body_json(json!({"labels": ["agent-pr"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "agent-pr"}])))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/pulls/15/requested_reviewers"))
        .and(body_json(json!({"reviewers": ["owner"]})))
        .respond_with(ResponseTemplate::new(422).set_body_json(
            json!({"message": "Review cannot be requested from pull request author."}),
        ))
        .expect(1)
        .mount(&env.server)
        .await;

    let new = NewPullRequest {
        title: "feat: dark mode toggle".into(),
        body: "Adds the toggle.\n\nCloses #12".into(),
        base: "main".into(),
        head: "agent/issue-12".into(),
        labels: vec!["agent-pr".into()],
        reviewers: vec!["owner".into()],
    };
    let pr = env.gh.create_pull_request(&repo(), &new).await.unwrap();
    assert_eq!(pr.number, 15);
    assert!(pr.has_label("agent-pr"));
}

#[tokio::test]
async fn g3_create_pull_request_refuses_non_agent_heads_without_a_request() {
    let env = env().await;
    let new = NewPullRequest {
        title: "t".into(),
        body: "b".into(),
        base: "main".into(),
        head: "main".into(),
        labels: vec![],
        reviewers: vec![],
    };
    let err = env.gh.create_pull_request(&repo(), &new).await.unwrap_err();
    assert!(matches!(err, GithubError::Refused(_)));
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn close_pull_request_patches_state() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("PATCH"))
        .and(path("/repos/owner/monorepo/pulls/15"))
        .and(body_json(json!({"state": "closed"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("pull", &env.server)))
        .expect(1)
        .mount(&env.server)
        .await;
    env.gh.close_pull_request(&repo(), 15).await.unwrap();
}

#[tokio::test]
async fn create_or_update_label() {
    let env = env().await;
    mount_token(&env).await;
    // agent-ready exists with another color: updated.
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/labels/agent-ready"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("label", &env.server)))
        .mount(&env.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/owner/monorepo/labels/agent-ready"))
        .and(body_partial_json(
            json!({"new_name": "agent-ready", "color": "0e8a16"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("label", &env.server)))
        .expect(1)
        .mount(&env.server)
        .await;
    // agent-pr does not exist: created.
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/labels/agent-pr"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&env.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/labels"))
        .and(body_partial_json(
            json!({"name": "agent-pr", "color": "c5def5"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture("label", &env.server)))
        .expect(1)
        .mount(&env.server)
        .await;

    let ready = LabelInfo {
        name: "agent-ready".into(),
        color: "0e8a16".into(),
        description: "Issue queued for the singlelane factory".into(),
    };
    let pr = LabelInfo {
        name: "agent-pr".into(),
        color: "c5def5".into(),
        description: "PR opened by the singlelane factory".into(),
    };
    assert_eq!(
        env.gh
            .create_or_update_label(&repo(), &ready)
            .await
            .unwrap(),
        LabelChange::Updated
    );
    assert_eq!(
        env.gh.create_or_update_label(&repo(), &pr).await.unwrap(),
        LabelChange::Created
    );
}

// ------------------------------------------------------------------------- retries

fn secondary_limit(retry_after: Option<&str>) -> ResponseTemplate {
    let t = ResponseTemplate::new(403)
        .set_body_json(json!({
            "message": "You have exceeded a secondary rate limit. Please wait a few minutes before you try again.",
            "documentation_url": "https://docs.github.com/rest/overview/rate-limits-for-the-rest-api"
        }))
        .insert_header("x-github-request-id", "RL:403");
    match retry_after {
        Some(v) => t.insert_header("retry-after", v),
        None => t,
    }
}

#[tokio::test]
async fn secondary_rate_limit_403_respects_retry_after() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/issues/12/comments"))
        .respond_with(secondary_limit(Some("7")))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&env.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/issues/12/comments"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(fixture("comment_created", &env.server)),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    // POST is retried after a rate limit: GitHub refused it, so nothing was created.
    let id = env.gh.create_comment(&repo(), 12, "hi").await.unwrap();
    assert_eq!(id, 5100);
    assert_eq!(env.sleeper.delays(), vec![Duration::from_secs(7)]);
}

#[tokio::test]
async fn secondary_rate_limit_that_persists_is_a_rate_limit_error() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/issues/12"))
        .respond_with(secondary_limit(None))
        .expect(3)
        .mount(&env.server)
        .await;
    let err = env.gh.get_issue(&repo(), 12).await.unwrap_err();
    assert!(err.is_rate_limited(), "{err}");
    assert!(!err.is_auth_failure());
    assert!(
        err.to_string().contains("RL:403"),
        "request id in the error: {err}"
    );
    // No Retry-After: GitHub's guidance is to wait at least a minute.
    assert_eq!(
        env.sleeper.delays(),
        vec![Duration::from_secs(60), Duration::from_secs(60)]
    );
}

#[tokio::test]
async fn retry_after_longer_than_the_cap_is_not_waited_for() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/issues/12"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(json!({"message": "Too many requests"}))
                .insert_header("retry-after", "3600"),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    let err = env.gh.get_issue(&repo(), 12).await.unwrap_err();
    assert!(err.is_rate_limited());
    assert!(env.sleeper.delays().is_empty());
}

#[tokio::test]
async fn server_errors_are_retried_with_backoff_for_get_only() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/pulls/15"))
        .respond_with(ResponseTemplate::new(502).set_body_string("Bad Gateway"))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/pulls/15"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("pull", &env.server)))
        .mount(&env.server)
        .await;
    env.gh.get_pull_request(&repo(), 15).await.unwrap();
    assert_eq!(
        env.sleeper.delays(),
        vec![Duration::from_secs(1), Duration::from_secs(2)]
    );

    // A POST that failed with 5xx may have been applied: not retried.
    Mock::given(method("POST"))
        .and(path("/repos/owner/monorepo/issues/12/comments"))
        .respond_with(ResponseTemplate::new(502).set_body_string("Bad Gateway"))
        .expect(1)
        .mount(&env.server)
        .await;
    let err = env.gh.create_comment(&repo(), 12, "x").await.unwrap_err();
    assert_eq!(err.status(), Some(502));
    assert_eq!(env.sleeper.delays().len(), 2);
}

#[tokio::test]
async fn bad_credentials_are_auth_failures_and_not_retried() {
    let env = env().await;
    mount_token(&env).await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/monorepo/issues/12"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"message": "Bad credentials", "status": "401"})),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    let err = env.gh.get_issue(&repo(), 12).await.unwrap_err();
    assert!(err.is_auth_failure());
    assert!(env.sleeper.delays().is_empty());
}

#[tokio::test]
async fn transport_errors_are_reported_without_secrets() {
    // Nothing listens on this port.
    let clock = Arc::new(FixedClock::new(Timestamp::UNIX_EPOCH));
    let sleeper = Arc::new(RecordingSleeper::default());
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_secs(1),
    };
    let http = Http::new("http://127.0.0.1:9", policy, clock.clone())
        .unwrap()
        .with_sleeper(sleeper.clone());
    let secret = singlelane_github::Secret::new(token());
    let err = http
        .request(http::Method::GET, "/app", &secret, None)
        .await
        .unwrap_err();
    assert!(err.is_transport(), "{err}");
    assert!(!format!("{err} {err:?}").contains(&token()));
    assert_eq!(sleeper.delays().len(), 1, "GET retried once");
}

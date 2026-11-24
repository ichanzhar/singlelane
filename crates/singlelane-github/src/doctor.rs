//! The GitHub part of `singlelane doctor` (SPEC 9): JWT accepted, installation token minted,
//! installation covers the repos, permissions exactly as in SPEC 5.1, `trusted_user` is not
//! the bot, labels exist, base branch protected.

use std::collections::BTreeMap;

use singlelane_core::config::{Config, check_trusted_user_is_not_bot};
use singlelane_core::doctor::Check;

use crate::api::GitHub;
use crate::auth::TokenProvider;
use crate::labels::missing_labels;

/// The installation permissions of SPEC 5.1, exactly. Anything else is a failure.
pub const REQUIRED_PERMISSIONS: [(&str, &str); 4] = [
    ("contents", "write"),
    ("issues", "write"),
    ("metadata", "read"),
    ("pull_requests", "write"),
];

/// Differences between granted and required permissions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionProblems {
    /// Granted but not allowed (for example `workflows`).
    pub extra: Vec<String>,
    /// Required but not granted.
    pub missing: Vec<String>,
    /// Granted at a different level: `name: actual (want wanted)`.
    pub wrong_level: Vec<String>,
}

impl PermissionProblems {
    /// Whether the permissions are exactly right.
    pub fn is_empty(&self) -> bool {
        self.extra.is_empty() && self.missing.is_empty() && self.wrong_level.is_empty()
    }
}

impl std::fmt::Display for PermissionProblems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if !self.extra.is_empty() {
            parts.push(format!("not allowed: {}", self.extra.join(", ")));
        }
        if !self.missing.is_empty() {
            parts.push(format!("missing: {}", self.missing.join(", ")));
        }
        if !self.wrong_level.is_empty() {
            parts.push(format!("wrong level: {}", self.wrong_level.join(", ")));
        }
        f.write_str(&parts.join("; "))
    }
}

/// Compares granted permissions with [`REQUIRED_PERMISSIONS`].
pub fn check_permissions(granted: &BTreeMap<String, String>) -> Result<(), PermissionProblems> {
    let mut p = PermissionProblems::default();
    for (name, level) in granted {
        match REQUIRED_PERMISSIONS.iter().find(|(n, _)| n == name) {
            None => p.extra.push(format!("{name} ({level})")),
            Some((_, want)) if !level.eq_ignore_ascii_case(want) => {
                p.wrong_level.push(format!("{name}: {level} (want {want})"));
            }
            Some(_) => {}
        }
    }
    for (name, want) in REQUIRED_PERMISSIONS {
        if !granted.contains_key(name) {
            p.missing.push(format!("{name} ({want})"));
        }
    }
    if p.is_empty() { Ok(()) } else { Err(p) }
}

const PERMISSIONS_HINT: &str = "in the GitHub App settings set repository permissions to exactly: \
     Metadata read, Contents read and write, Issues read and write, Pull requests read and write; \
     nothing else. Then accept the new permissions on the installation";

/// Runs the GitHub checks in order. A failed JWT or token check skips the checks that need it.
pub async fn github_checks(
    gh: &dyn GitHub,
    tokens: &dyn TokenProvider,
    config: &Config,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // JWT accepted: GET /app.
    let app = match gh.app().await {
        Ok(app) => {
            checks.push(Check::pass(
                "GitHub App JWT",
                format!("accepted by GET /app: app {} (id {})", app.slug, app.id),
            ));
            if app.id != config.github.app_id {
                checks.push(Check::fail(
                    "GitHub App id",
                    format!(
                        "config has app_id {}, the key belongs to app {}",
                        config.github.app_id, app.id
                    ),
                    "set github.app_id to the App ID shown on the app's settings page",
                ));
            }
            Some(app)
        }
        Err(e) => {
            checks.push(Check::fail(
                "GitHub App JWT",
                format!("rejected: {e}"),
                "check github.app_id, and that github.private_key_path is a current private key \
                 of the app (Settings > Developer settings > GitHub Apps > Private keys); \
                 also check the host clock",
            ));
            None
        }
    };

    // Installation token.
    let token_ok = match tokens.installation_token().await {
        Ok(_) => {
            checks.push(Check::pass("installation token", "minted"));
            true
        }
        Err(e) => {
            checks.push(Check::fail(
                "installation token",
                format!("can't be minted: {e}"),
                "check github.installation_id (the number at the end of the installation's \
                 settings URL) and that the app is still installed and not suspended",
            ));
            false
        }
    };

    // Installation state and permissions (JWT).
    if app.is_some() {
        match gh.installation().await {
            Ok(inst) => {
                if inst.suspended {
                    checks.push(Check::fail(
                        "installation",
                        "suspended",
                        "unsuspend the installation in the account's Settings > Applications",
                    ));
                }
                match check_permissions(&inst.permissions) {
                    Ok(()) => checks.push(Check::pass(
                        "installation permissions",
                        "exactly metadata:read, contents:write, issues:write, pull_requests:write",
                    )),
                    Err(p) => checks.push(Check::fail(
                        "installation permissions",
                        p.to_string(),
                        PERMISSIONS_HINT,
                    )),
                }
                if inst.repository_selection == "all" {
                    checks.push(Check::warn(
                        "installation repositories",
                        "the app is installed on all repositories",
                        "reinstall with \"Only select repositories\" and pick the target repos",
                    ));
                }
            }
            Err(e) => checks.push(Check::fail(
                "installation permissions",
                format!("can't read the installation: {e}"),
                "check github.installation_id",
            )),
        }
    }

    if !token_ok {
        checks.push(Check::fail(
            "repository checks",
            "skipped: no installation token",
            "fix the installation token check first",
        ));
        return checks;
    }

    // Installation covers every configured repo.
    match gh.installation_repositories().await {
        Ok(covered) => {
            let missing: Vec<String> = config
                .repos
                .iter()
                .map(|r| r.name.to_string())
                .filter(|name| !covered.iter().any(|c| c.eq_ignore_ascii_case(name)))
                .collect();
            if missing.is_empty() {
                checks.push(Check::pass(
                    "installation repositories",
                    format!("covers all {} configured repo(s)", config.repos.len()),
                ));
            } else {
                checks.push(Check::fail(
                    "installation repositories",
                    format!("not covered: {}", missing.join(", ")),
                    "add the repos to the installation (Configure > Repository access > \
                     Only select repositories)",
                ));
            }
        }
        Err(e) => checks.push(Check::fail(
            "installation repositories",
            format!("can't list: {e}"),
            "check that the app is installed",
        )),
    }

    // trusted_user is not the bot.
    if let Some(app) = &app {
        let bot = match gh.bot_user(&app.slug).await {
            Ok(b) => b.login,
            Err(_) => app.bot_login(),
        };
        match check_trusted_user_is_not_bot(&config.trusted_user, &bot) {
            Ok(()) => checks.push(Check::pass(
                "trusted_user",
                format!("{} is not the app bot {bot}", config.trusted_user),
            )),
            Err(e) => checks.push(Check::fail(
                "trusted_user",
                e,
                "set trusted_user to your own GitHub login",
            )),
        }
    }

    for repo in &config.repos {
        let name = &repo.name;
        match gh.list_labels(name).await {
            Ok(labels) => {
                let missing = missing_labels(&labels);
                if missing.is_empty() {
                    checks.push(Check::pass(format!("labels {name}"), "all present"));
                } else {
                    let names: Vec<&str> = missing.iter().map(|l| l.as_str()).collect();
                    checks.push(Check::fail(
                        format!("labels {name}"),
                        format!("missing: {}", names.join(", ")),
                        "run `singlelane setup-labels`",
                    ));
                }
            }
            Err(e) => checks.push(Check::fail(
                format!("labels {name}"),
                format!("can't list: {e}"),
                "check the Issues permission and the installation's repository access",
            )),
        }
        let branch = &repo.base_branch;
        match gh.branch_protected(name, branch).await {
            Ok(true) => checks.push(Check::pass(
                format!("base branch {name}@{branch}"),
                "protected",
            )),
            Ok(false) => checks.push(Check::warn(
                format!("base branch {name}@{branch}"),
                "not protected",
                "add a branch rule or ruleset: require a PR with 1 approval, dismiss stale \
                 approvals, block force pushes and deletion, empty bypass list (rulesets on \
                 private repos need a paid GitHub plan)",
            )),
            Err(e) if e.is_not_found() => checks.push(Check::fail(
                format!("base branch {name}@{branch}"),
                "branch not found",
                "set base_branch to the repo's default branch",
            )),
            Err(e) => checks.push(Check::fail(
                format!("base branch {name}@{branch}"),
                format!("can't read: {e}"),
                "check the Contents permission",
            )),
        }
    }
    checks
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::error::{GithubError, Result};
    use crate::fake::{FAKE_APP_SLUG, FakeGitHub};
    use crate::labels::setup_labels;
    use crate::secret::Secret;
    use singlelane_core::doctor::CheckStatus;
    use singlelane_core::repo::RepoName;

    fn granted(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn exact_permissions_pass() {
        assert!(check_permissions(&granted(&REQUIRED_PERMISSIONS)).is_ok());
    }

    #[test]
    fn extra_workflows_permission_fails() {
        let mut g = granted(&REQUIRED_PERMISSIONS);
        g.insert("workflows".into(), "write".into());
        let p = check_permissions(&g).unwrap_err();
        assert_eq!(p.extra, vec!["workflows (write)"]);
        assert!(p.to_string().contains("not allowed: workflows"));
    }

    #[test]
    fn missing_or_wrong_level_fails() {
        let g = granted(&[
            ("metadata", "read"),
            ("contents", "read"),
            ("issues", "write"),
        ]);
        let p = check_permissions(&g).unwrap_err();
        assert_eq!(p.missing, vec!["pull_requests (write)"]);
        assert_eq!(p.wrong_level, vec!["contents: read (want write)"]);
        let g = granted(&[
            ("metadata", "read"),
            ("contents", "write"),
            ("issues", "write"),
            ("pull_requests", "write"),
            ("administration", "read"),
        ]);
        assert_eq!(
            check_permissions(&g).unwrap_err().extra,
            vec!["administration (read)"]
        );
    }

    struct Tokens(bool);

    #[async_trait]
    impl TokenProvider for Tokens {
        async fn installation_token(&self) -> Result<Secret> {
            if self.0 {
                Ok(Secret::new("t"))
            } else {
                Err(GithubError::Status {
                    method: "POST".into(),
                    path: "/app/installations/2/access_tokens".into(),
                    status: 404,
                    message: "Not Found".into(),
                    request_id: "-".into(),
                    rate_limited: false,
                })
            }
        }
        async fn fresh_installation_token(&self) -> Result<Secret> {
            self.installation_token().await
        }
    }

    fn config(trusted: &str) -> Config {
        let text = format!(
            r#"
trusted_user = "{trusted}"
timezone = "Europe/Kyiv"
work_hours = {{ start = "07:00", end = "23:00" }}
data_dir = "/var/lib/singlelane"
claude_env_file = "/etc/singlelane/claude.env"
[github]
app_id = 1
installation_id = 2
private_key_path = "/etc/singlelane/github-app.pem"
[[repo]]
name = "owner/repo"
base_branch = "main"
[[repo]]
name = "owner/other"
base_branch = "develop"
"#
        );
        Config::from_toml(&text).unwrap()
    }

    fn status_of<'a>(checks: &'a [Check], name: &str) -> Vec<&'a Check> {
        checks.iter().filter(|c| c.name.starts_with(name)).collect()
    }

    #[tokio::test]
    async fn all_green_with_a_correct_setup() {
        let gh = FakeGitHub::new();
        let r = RepoName::parse("owner/repo").unwrap();
        let o = RepoName::parse("owner/other").unwrap();
        gh.add_repo(&r);
        gh.add_repo(&o);
        gh.with_state(|s| {
            if let Some(repo) = s.repos.get_mut(&o) {
                repo.branches.insert("develop".into(), true);
            }
        });
        setup_labels(&gh, &[r, o]).await;
        let checks = github_checks(&gh, &Tokens(true), &config("owner")).await;
        for c in &checks {
            assert_eq!(c.status, CheckStatus::Pass, "{}", c.render());
        }
        assert!(checks.len() >= 9);
    }

    #[tokio::test]
    async fn problems_are_reported_with_hints() {
        let gh = FakeGitHub::new();
        let r = RepoName::parse("owner/repo").unwrap();
        gh.add_repo(&r);
        gh.with_state(|s| {
            s.installation
                .permissions
                .insert("workflows".into(), "write".into());
            if let Some(repo) = s.repos.get_mut(&r) {
                repo.branches.insert("main".into(), false);
            }
        });
        let checks = github_checks(&gh, &Tokens(true), &config(FAKE_APP_SLUG)).await;
        let perm = status_of(&checks, "installation permissions");
        assert_eq!(perm[0].status, CheckStatus::Fail);
        assert!(perm[0].detail.contains("workflows"));
        assert!(perm[0].hint.is_some());
        let repos = status_of(&checks, "installation repositories");
        assert_eq!(repos[0].status, CheckStatus::Fail);
        assert!(repos[0].detail.contains("owner/other"));
        assert_eq!(
            status_of(&checks, "trusted_user")[0].status,
            CheckStatus::Fail
        );
        assert_eq!(
            status_of(&checks, "labels owner/repo")[0].status,
            CheckStatus::Fail
        );
        assert_eq!(
            status_of(&checks, "base branch owner/repo@main")[0].status,
            CheckStatus::Warn
        );
        assert_eq!(
            status_of(&checks, "base branch owner/other")[0].status,
            CheckStatus::Fail
        );
    }

    #[tokio::test]
    async fn failed_jwt_and_token_skip_dependent_checks() {
        let gh = FakeGitHub::new();
        gh.fail_next("app", 401);
        let checks = github_checks(&gh, &Tokens(false), &config("owner")).await;
        assert_eq!(checks[0].status, CheckStatus::Fail);
        assert_eq!(checks[1].status, CheckStatus::Fail);
        assert!(checks.last().unwrap().detail.contains("skipped"));
        assert!(checks.iter().all(|c| c.status == CheckStatus::Fail));
    }
}

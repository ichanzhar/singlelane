//! Repository names, item references and agent branch names.

use std::fmt;

use thiserror::Error;

/// Prefix of every branch the factory creates or pushes to (G3).
pub const AGENT_BRANCH_PREFIX: &str = "agent/";

/// A validated `owner/name` repository name.
///
/// GitHub owner and repo names are case-insensitive; equality here is case-insensitive too,
/// while the original spelling is kept for display and API calls.
#[derive(Debug, Clone, Eq)]
pub struct RepoName {
    owner: String,
    name: String,
}

/// Error for a malformed repository name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid repo name {0:?}: expected `owner/name`")]
pub struct RepoNameError(pub String);

impl RepoName {
    /// Parses `owner/name`. Owner: letters, digits and single hyphens (GitHub login rules).
    /// Name: letters, digits, `-`, `_`, `.`, not `.` or `..`.
    pub fn parse(s: &str) -> Result<Self, RepoNameError> {
        let err = || RepoNameError(s.to_string());
        let (owner, name) = s.split_once('/').ok_or_else(err)?;
        if !is_valid_login(owner) {
            return Err(err());
        }
        let name_ok = !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !name_ok {
            return Err(err());
        }
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    /// The owner part.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The name part.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner__name`, used in job ids and file names.
    pub fn slug(&self) -> String {
        format!("{}__{}", self.owner, self.name)
    }

    /// Web URL of an issue.
    pub fn issue_url(&self, number: u64) -> String {
        format!("https://github.com/{self}/issues/{number}")
    }

    /// Web URL of a pull request.
    pub fn pr_url(&self, number: u64) -> String {
        format!("https://github.com/{self}/pull/{number}")
    }
}

impl PartialEq for RepoName {
    fn eq(&self, other: &Self) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl std::hash::Hash for RepoName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.owner.to_ascii_lowercase().hash(state);
        self.name.to_ascii_lowercase().hash(state);
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// GitHub login rules: 1-39 chars, alphanumeric or single hyphens, no leading or trailing
/// hyphen. Bot logins (`name[bot]`) are not valid here on purpose.
pub fn is_valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Case-insensitive GitHub login comparison.
pub fn same_login(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Issue or pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    /// A GitHub issue.
    Issue,
    /// A GitHub pull request.
    Pr,
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Issue => "issue",
            Self::Pr => "PR",
        })
    }
}

/// A reference to one issue or PR.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemRef {
    /// Repository.
    pub repo: RepoName,
    /// Issue or PR.
    pub kind: ItemKind,
    /// Issue or PR number.
    pub number: u64,
}

impl ItemRef {
    /// An issue reference.
    pub fn issue(repo: RepoName, number: u64) -> Self {
        Self {
            repo,
            kind: ItemKind::Issue,
            number,
        }
    }

    /// A PR reference.
    pub fn pr(repo: RepoName, number: u64) -> Self {
        Self {
            repo,
            kind: ItemKind::Pr,
            number,
        }
    }

    /// Web URL of the item.
    pub fn url(&self) -> String {
        match self.kind {
            ItemKind::Issue => self.repo.issue_url(self.number),
            ItemKind::Pr => self.repo.pr_url(self.number),
        }
    }
}

impl fmt::Display for ItemRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

/// The branch an issue job works on: `agent/issue-<n>`.
pub fn issue_branch(number: u64) -> String {
    format!("{AGENT_BRANCH_PREFIX}issue-{number}")
}

/// Parses `agent/issue-<n>` back to `n`. Anything else, including leading zeros or extra
/// suffixes, is `None`.
pub fn parse_issue_branch(branch: &str) -> Option<u64> {
    let digits = branch
        .strip_prefix(AGENT_BRANCH_PREFIX)?
        .strip_prefix("issue-")?;
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    digits.parse().ok().filter(|n| *n > 0)
}

/// Whether a branch is one the factory may push to (G3).
pub fn is_agent_branch(branch: &str) -> bool {
    branch.len() > AGENT_BRANCH_PREFIX.len() && branch.starts_with(AGENT_BRANCH_PREFIX)
}

/// Job id: `<owner>__<repo>-<n>-<timestamp>` (SPEC 6.2). `timestamp` is formatted by the
/// caller, for example `20261124-081500`.
pub fn job_id(item: &ItemRef, timestamp: &str) -> String {
    format!("{}-{}-{}", item.repo.slug(), item.number, timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_parses_and_compares_case_insensitively() {
        let a = RepoName::parse("Owner/Repo.rs").unwrap();
        let b = RepoName::parse("owner/repo.rs").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "Owner/Repo.rs");
        assert_eq!(a.slug(), "Owner__Repo.rs");
    }

    #[test]
    fn repo_name_rejects_bad_input() {
        for bad in [
            "", "owner", "/repo", "owner/", "a/b/c", "-o/r", "o-/r", "o--o/r", "o/..", "o/.",
            "o/r r", "o r/r",
        ] {
            assert!(RepoName::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn issue_branch_round_trips() {
        assert_eq!(issue_branch(42), "agent/issue-42");
        assert_eq!(parse_issue_branch("agent/issue-42"), Some(42));
        for bad in [
            "agent/issue-",
            "agent/issue-0",
            "agent/issue-042",
            "agent/issue-4a",
            "issue-4",
            "agent/issue-4/x",
            "main",
            "agent/fix-4",
        ] {
            assert_eq!(parse_issue_branch(bad), None, "{bad}");
        }
    }

    #[test]
    fn agent_branch_check() {
        assert!(is_agent_branch("agent/issue-1"));
        assert!(!is_agent_branch("agent/"));
        assert!(!is_agent_branch("main"));
        assert!(!is_agent_branch("feature/agent/x"));
    }

    #[test]
    fn job_id_format() {
        let item = ItemRef::issue(RepoName::parse("o/r").unwrap(), 7);
        assert_eq!(job_id(&item, "20261124-081500"), "o__r-7-20261124-081500");
    }
}

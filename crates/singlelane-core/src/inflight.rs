//! The in-flight rules (SPEC 3.1, G10) as a pure function.
//!
//! The caller reads all open labeled issues and PRs of every configured repo from GitHub and
//! passes them in as a [`Snapshot`]. [`plan`] returns what this tick may do. GitHub is the
//! only source of truth for whether something is in flight; the history store only supplies
//! [`Snapshot::last_in_flight`], which decides queue priority.

use std::collections::BTreeMap;

use crate::policy::trust::TrustPolicy;
use crate::repo::ItemKind;
use crate::repo::{RepoName, is_agent_branch, parse_issue_branch};
use crate::state::{ItemState, Label, StateError, agent_labels};

/// An open issue as read from GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenIssue {
    /// Repository.
    pub repo: RepoName,
    /// Issue number.
    pub number: u64,
    /// Issue title (for notifications).
    pub title: String,
    /// Author login.
    pub author: String,
    /// All label names on the issue.
    pub labels: Vec<String>,
}

/// An open PR as read from GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    /// Repository.
    pub repo: RepoName,
    /// PR number.
    pub number: u64,
    /// Head branch name.
    pub head_branch: String,
    /// Whether the head branch lives in the same repository (not a fork).
    pub head_in_repo: bool,
    /// All label names on the PR.
    pub labels: Vec<String>,
    /// For a PR on `agent/issue-<n>`: whether issue `n` is still open. `None` if the branch
    /// does not name an issue or the caller did not look it up.
    pub linked_issue_open: Option<bool>,
}

/// Everything the in-flight rules need for one tick.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Configured repos, in config order.
    pub repos: Vec<RepoName>,
    /// Open issues with at least one agent label, across all repos.
    pub issues: Vec<OpenIssue>,
    /// Open PRs with at least one agent label, across all repos.
    pub prs: Vec<OpenPr>,
    /// The issue that was in flight most recently, from the history store.
    pub last_in_flight: Option<(RepoName, u64)>,
}

/// Identity of an in-flight task.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskId {
    /// The task of issue `n` (the normal case).
    Issue(String, u64),
    /// An `agent-pr` PR whose branch does not name an issue. Counted conservatively.
    OrphanPr(String, u64),
}

/// A task that is in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightTask {
    /// Repository.
    pub repo: RepoName,
    /// The issue number, if known.
    pub issue: Option<u64>,
    /// State of the issue (`None` if the issue is not open or not labeled).
    pub issue_state: Option<ItemState>,
    /// The open agent PR of the task, if any.
    pub pr: Option<InFlightPr>,
}

/// The open agent PR of an in-flight task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightPr {
    /// PR number.
    pub number: u64,
    /// PR state from its labels.
    pub state: ItemState,
    /// Head branch.
    pub head_branch: String,
}

impl InFlightTask {
    /// A short human description of where the task stands.
    pub fn describe(&self) -> String {
        let issue = match self.issue {
            Some(n) => format!("{}#{n}", self.repo),
            None => format!("{} (unknown issue)", self.repo),
        };
        match (&self.issue_state, &self.pr) {
            (_, Some(pr)) if pr.state == ItemState::Working => {
                format!("{issue}: revise running on PR #{}", pr.number)
            }
            (_, Some(pr)) if pr.state == ItemState::Revise => {
                format!("{issue}: revise requested on PR #{}", pr.number)
            }
            (_, Some(pr)) if pr.state == ItemState::Blocked => {
                format!("{issue}: blocked on PR #{}", pr.number)
            }
            (Some(state), Some(pr)) => format!("{issue}: {state}, PR #{}", pr.number),
            (Some(state), None) => format!("{issue}: {state}"),
            (None, Some(pr)) => format!("{issue}: PR #{} {}", pr.number, pr.state),
            (None, None) => issue,
        }
    }
}

/// Why a task is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseReason {
    /// The issue still carries agent labels but has no open agent PR any more after being
    /// in review: the PR was closed without merge.
    PrClosedUnmerged,
    /// The issue was closed by the owner while its agent PR is still open.
    IssueClosed,
}

/// Housekeeping the caller must apply before acting on the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Repository.
    pub repo: RepoName,
    /// The task's issue.
    pub issue: u64,
    /// The open agent PR to close (for [`ReleaseReason::IssueClosed`]).
    pub pr: Option<u64>,
    /// Why.
    pub reason: ReleaseReason,
}

/// What this tick may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing in flight, queue empty.
    Idle,
    /// Nothing in flight: claim this issue.
    ClaimIssue {
        /// Repository.
        repo: RepoName,
        /// Issue number.
        issue: u64,
    },
    /// Run revise on the in-flight task's PR.
    Revise {
        /// Repository.
        repo: RepoName,
        /// PR number.
        pr: u64,
        /// The task's issue, if known.
        issue: Option<u64>,
    },
    /// A task is in flight and there is nothing to run for it (working, blocked, in review).
    Wait(InFlightTask),
    /// More than one task in flight: claim nothing, pause and notify.
    Conflict(Vec<InFlightTask>),
}

/// A label problem on one item, reported but not fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelProblem {
    /// Repository.
    pub repo: RepoName,
    /// Item kind.
    pub kind: ItemKind,
    /// Number.
    pub number: u64,
    /// The error.
    pub error: StateError,
}

/// The output of [`plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Releases to apply first. If any of them fails, the caller must not act on
    /// [`Plan::decision`] in this tick.
    pub releases: Vec<Release>,
    /// What to do.
    pub decision: Decision,
    /// `agent-revise` labels on PRs that are not the in-flight task's PR (or not on an
    /// `agent/` branch). Ignored; reported in the daily summary.
    pub ignored_revise: Vec<(RepoName, u64)>,
    /// `agent-ready` issues ignored because their author is not `trusted_user` (G1).
    pub untrusted_ready: Vec<(RepoName, u64)>,
    /// Items with conflicting workflow labels. They count as in flight.
    pub label_problems: Vec<LabelProblem>,
}

#[derive(Debug, Default)]
struct TaskBuilder {
    repo: Option<RepoName>,
    issue: Option<u64>,
    issue_state: Option<ItemState>,
    issue_conflict: bool,
    prs: Vec<InFlightPr>,
    linked_issue_open: Option<bool>,
}

/// Applies the in-flight rules of SPEC 3.1.
pub fn plan(snapshot: &Snapshot, trust: &TrustPolicy) -> Plan {
    let mut label_problems = Vec::new();
    let mut tasks: BTreeMap<TaskId, TaskBuilder> = BTreeMap::new();
    let repo_index = |r: &RepoName| snapshot.repos.iter().position(|x| x == r);
    let key = |r: &RepoName| r.to_string().to_ascii_lowercase();

    // Issues that count as in flight, and the agent-ready queue.
    let mut ready: Vec<&OpenIssue> = Vec::new();
    let mut untrusted_ready = Vec::new();
    for issue in &snapshot.issues {
        if repo_index(&issue.repo).is_none() {
            continue;
        }
        let labels = agent_labels(&issue.labels);
        let (state, conflict) = match ItemState::from_labels(ItemKind::Issue, true, &labels) {
            Ok(s) => (s, false),
            Err(error) => {
                label_problems.push(LabelProblem {
                    repo: issue.repo.clone(),
                    kind: ItemKind::Issue,
                    number: issue.number,
                    error,
                });
                // Conflicting labels always involve an in-flight label: count it.
                (ItemState::Blocked, true)
            }
        };
        if state.issue_in_flight() {
            let t = tasks
                .entry(TaskId::Issue(key(&issue.repo), issue.number))
                .or_default();
            t.repo = Some(issue.repo.clone());
            t.issue = Some(issue.number);
            t.issue_state = Some(state);
            t.issue_conflict = conflict;
        } else if state == ItemState::Ready {
            if trust.is_trusted(&issue.author) {
                ready.push(issue);
            } else {
                untrusted_ready.push((issue.repo.clone(), issue.number));
            }
        }
    }

    // Agent PRs make their issue's task in flight.
    let mut revise_requests: Vec<&OpenPr> = Vec::new();
    for pr in &snapshot.prs {
        if repo_index(&pr.repo).is_none() {
            continue;
        }
        let labels = agent_labels(&pr.labels);
        let state = match ItemState::from_labels(ItemKind::Pr, true, &labels) {
            Ok(s) => s,
            Err(error) => {
                label_problems.push(LabelProblem {
                    repo: pr.repo.clone(),
                    kind: ItemKind::Pr,
                    number: pr.number,
                    error,
                });
                ItemState::Blocked
            }
        };
        if !labels.contains(&Label::Pr) {
            if state == ItemState::Revise {
                revise_requests.push(pr);
            }
            continue;
        }
        let id = match parse_issue_branch(&pr.head_branch).filter(|_| pr.head_in_repo) {
            Some(n) => TaskId::Issue(key(&pr.repo), n),
            None => TaskId::OrphanPr(key(&pr.repo), pr.number),
        };
        let t = tasks.entry(id.clone()).or_default();
        t.repo = Some(pr.repo.clone());
        if let TaskId::Issue(_, n) = id {
            t.issue = Some(n);
            t.linked_issue_open = pr.linked_issue_open;
        }
        t.prs.push(InFlightPr {
            number: pr.number,
            state,
            head_branch: pr.head_branch.clone(),
        });
        if state == ItemState::Revise {
            revise_requests.push(pr);
        }
    }

    // Releases: tasks GitHub says are over but whose labels still say otherwise.
    let mut releases = Vec::new();
    let mut in_flight: Vec<InFlightTask> = Vec::new();
    for (_, t) in tasks {
        let Some(repo) = t.repo else { continue };
        if let Some(issue) = t.issue {
            // PR closed without merge: the issue was in review, and its PR is gone.
            if t.issue_state == Some(ItemState::InReview) && t.prs.is_empty() && !t.issue_conflict {
                releases.push(Release {
                    repo,
                    issue,
                    pr: None,
                    reason: ReleaseReason::PrClosedUnmerged,
                });
                continue;
            }
            // Issue closed by the owner while its PR is open.
            if t.issue_state.is_none() && t.linked_issue_open == Some(false) && t.prs.len() == 1 {
                releases.push(Release {
                    repo,
                    issue,
                    pr: t.prs.first().map(|p| p.number),
                    reason: ReleaseReason::IssueClosed,
                });
                continue;
            }
        }
        // More than one agent PR for one issue is itself a conflict: keep them apart.
        if t.prs.len() > 1 {
            for pr in t.prs {
                in_flight.push(InFlightTask {
                    repo: repo.clone(),
                    issue: t.issue,
                    issue_state: t.issue_state,
                    pr: Some(pr),
                });
            }
            continue;
        }
        in_flight.push(InFlightTask {
            repo,
            issue: t.issue,
            issue_state: t.issue_state,
            pr: t.prs.into_iter().next(),
        });
    }

    let in_flight_pr = match in_flight.as_slice() {
        [only] => only.pr.as_ref().map(|p| (only.repo.clone(), p.number)),
        _ => None,
    };
    let ignored_revise: Vec<(RepoName, u64)> = revise_requests
        .iter()
        .filter(|pr| {
            in_flight_pr.as_ref() != Some(&(pr.repo.clone(), pr.number))
                || !is_agent_branch(&pr.head_branch)
                || !pr.head_in_repo
        })
        .map(|pr| (pr.repo.clone(), pr.number))
        .collect();

    let decision = match in_flight.len() {
        0 => next_issue(snapshot, &ready, repo_index),
        1 => {
            let task = in_flight.remove(0);
            match &task.pr {
                Some(pr)
                    if pr.state == ItemState::Revise
                        && is_agent_branch(&pr.head_branch)
                        && task.issue_state != Some(ItemState::Working)
                        && !ignored_revise.contains(&(task.repo.clone(), pr.number)) =>
                {
                    Decision::Revise {
                        repo: task.repo.clone(),
                        pr: pr.number,
                        issue: task.issue,
                    }
                }
                _ => Decision::Wait(task),
            }
        }
        _ => Decision::Conflict(in_flight),
    };

    Plan {
        releases,
        decision,
        ignored_revise,
        untrusted_ready,
        label_problems,
    }
}

fn next_issue(
    snapshot: &Snapshot,
    ready: &[&OpenIssue],
    repo_index: impl Fn(&RepoName) -> Option<usize>,
) -> Decision {
    if let Some((repo, n)) = &snapshot.last_in_flight
        && ready.iter().any(|i| &i.repo == repo && i.number == *n)
    {
        return Decision::ClaimIssue {
            repo: repo.clone(),
            issue: *n,
        };
    }
    ready
        .iter()
        .filter_map(|i| repo_index(&i.repo).map(|idx| (idx, i.number, i)))
        .min_by_key(|(idx, n, _)| (*idx, *n))
        .map_or(Decision::Idle, |(_, _, i)| Decision::ClaimIssue {
            repo: i.repo.clone(),
            issue: i.number,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "owner";

    fn repo(s: &str) -> RepoName {
        RepoName::parse(s).unwrap()
    }

    fn issue(r: &str, n: u64, labels: &[&str]) -> OpenIssue {
        OpenIssue {
            repo: repo(r),
            number: n,
            title: format!("issue {n}"),
            author: OWNER.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn pr(r: &str, n: u64, branch: &str, labels: &[&str]) -> OpenPr {
        OpenPr {
            repo: repo(r),
            number: n,
            head_branch: branch.into(),
            head_in_repo: true,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            linked_issue_open: Some(true),
        }
    }

    fn snap(issues: Vec<OpenIssue>, prs: Vec<OpenPr>) -> Snapshot {
        Snapshot {
            repos: vec![repo("o/a"), repo("o/b")],
            issues,
            prs,
            last_in_flight: None,
        }
    }

    fn run(s: &Snapshot) -> Plan {
        plan(s, &TrustPolicy::new(OWNER))
    }

    fn claim(r: &str, n: u64) -> Decision {
        Decision::ClaimIssue {
            repo: repo(r),
            issue: n,
        }
    }

    #[test]
    fn g10_none_in_flight_claims_lowest_number_in_config_order() {
        let s = snap(
            vec![
                issue("o/b", 1, &["agent-ready"]),
                issue("o/a", 9, &["agent-ready"]),
                issue("o/a", 5, &["agent-ready"]),
            ],
            vec![],
        );
        assert_eq!(run(&s).decision, claim("o/a", 5));
    }

    #[test]
    fn g10_empty_queue_is_idle() {
        assert_eq!(run(&snap(vec![], vec![])).decision, Decision::Idle);
    }

    #[test]
    fn g10_working_issue_blocks_new_claims() {
        let s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/b", 7, &["agent-working"]),
            ],
            vec![],
        );
        let p = run(&s);
        assert!(matches!(p.decision, Decision::Wait(ref t) if t.issue == Some(7)));
    }

    #[test]
    fn g10_blocked_issue_blocks_new_claims() {
        let s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/a", 3, &["agent-blocked"]),
            ],
            vec![],
        );
        assert!(matches!(run(&s).decision, Decision::Wait(ref t) if t.issue == Some(3)));
    }

    #[test]
    fn g10_in_review_blocks_new_claims() {
        let s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/a", 3, &["agent-in-review"]),
            ],
            vec![pr("o/a", 10, "agent/issue-3", &["agent-pr"])],
        );
        let p = run(&s);
        match p.decision {
            Decision::Wait(t) => {
                assert_eq!(t.issue, Some(3));
                assert_eq!(t.pr.map(|p| p.number), Some(10));
            }
            other => panic!("expected wait, got {other:?}"),
        }
        assert!(p.releases.is_empty());
    }

    #[test]
    fn g10_agent_pr_alone_counts_as_in_flight() {
        // Owner removed the issue labels by hand, but the agent PR is still open.
        let s = snap(
            vec![issue("o/a", 1, &["agent-ready"])],
            vec![pr("o/b", 10, "agent/issue-3", &["agent-pr"])],
        );
        assert!(matches!(run(&s).decision, Decision::Wait(_)));
    }

    #[test]
    fn g10_in_review_with_revise_runs_revise() {
        let s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/a", 3, &["agent-in-review"]),
            ],
            vec![pr(
                "o/a",
                10,
                "agent/issue-3",
                &["agent-pr", "agent-revise"],
            )],
        );
        let p = run(&s);
        assert_eq!(
            p.decision,
            Decision::Revise {
                repo: repo("o/a"),
                pr: 10,
                issue: Some(3)
            }
        );
        assert!(p.ignored_revise.is_empty());
    }

    #[test]
    fn g10_revise_already_running_waits() {
        let s = snap(
            vec![issue("o/a", 3, &["agent-in-review"])],
            vec![pr(
                "o/a",
                10,
                "agent/issue-3",
                &["agent-pr", "agent-working"],
            )],
        );
        assert!(matches!(run(&s).decision, Decision::Wait(_)));
    }

    #[test]
    fn g10_revise_on_foreign_pr_is_ignored_and_reported() {
        let s = snap(
            vec![issue("o/a", 3, &["agent-in-review"])],
            vec![
                pr("o/a", 10, "agent/issue-3", &["agent-pr"]),
                pr("o/a", 11, "feature/x", &["agent-revise"]),
                pr("o/b", 12, "agent/issue-9", &["agent-revise"]),
            ],
        );
        let p = run(&s);
        assert!(matches!(p.decision, Decision::Wait(ref t) if t.issue == Some(3)));
        assert_eq!(p.ignored_revise, vec![(repo("o/a"), 11), (repo("o/b"), 12)]);
    }

    #[test]
    fn g10_revise_on_foreign_pr_with_nothing_in_flight_claims_issue_not_revise() {
        let s = snap(
            vec![issue("o/a", 4, &["agent-ready"])],
            vec![pr("o/a", 11, "agent/issue-9", &["agent-revise"])],
        );
        let p = run(&s);
        assert_eq!(p.decision, claim("o/a", 4));
        assert_eq!(p.ignored_revise, vec![(repo("o/a"), 11)]);
    }

    #[test]
    fn g10_revise_on_fork_branch_is_ignored() {
        let mut fork = pr("o/a", 10, "agent/issue-3", &["agent-pr", "agent-revise"]);
        fork.head_in_repo = false;
        let s = snap(vec![issue("o/a", 3, &["agent-in-review"])], vec![fork]);
        let p = run(&s);
        assert!(matches!(p.decision, Decision::Wait(_)));
        assert_eq!(p.ignored_revise, vec![(repo("o/a"), 10)]);
    }

    #[test]
    fn g10_two_in_flight_is_a_conflict() {
        let s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/a", 3, &["agent-blocked"]),
                issue("o/b", 4, &["agent-in-review"]),
            ],
            vec![pr(
                "o/b",
                10,
                "agent/issue-4",
                &["agent-pr", "agent-revise"],
            )],
        );
        let p = run(&s);
        match p.decision {
            Decision::Conflict(tasks) => assert_eq!(tasks.len(), 2),
            other => panic!("expected conflict, got {other:?}"),
        }
        // No revise runs during a conflict either.
        assert_eq!(p.ignored_revise, vec![(repo("o/b"), 10)]);
    }

    #[test]
    fn g10_two_agent_prs_for_one_issue_is_a_conflict() {
        let s = snap(
            vec![issue("o/a", 3, &["agent-in-review"])],
            vec![
                pr("o/a", 10, "agent/issue-3", &["agent-pr"]),
                pr("o/a", 11, "agent/issue-3", &["agent-pr"]),
            ],
        );
        assert!(matches!(run(&s).decision, Decision::Conflict(ref t) if t.len() == 2));
    }

    #[test]
    fn g10_pr_closed_unmerged_releases_task_and_allows_next_claim() {
        // Issue still says in-review, but its PR is gone (closed without merge).
        let s = snap(
            vec![
                issue("o/a", 3, &["agent-in-review"]),
                issue("o/a", 5, &["agent-ready"]),
            ],
            vec![],
        );
        let p = run(&s);
        assert_eq!(
            p.releases,
            vec![Release {
                repo: repo("o/a"),
                issue: 3,
                pr: None,
                reason: ReleaseReason::PrClosedUnmerged
            }]
        );
        assert_eq!(p.decision, claim("o/a", 5));
    }

    #[test]
    fn g10_issue_closed_releases_task_and_closes_pr() {
        let mut open_pr = pr("o/a", 10, "agent/issue-3", &["agent-pr"]);
        open_pr.linked_issue_open = Some(false);
        let s = snap(vec![issue("o/a", 5, &["agent-ready"])], vec![open_pr]);
        let p = run(&s);
        assert_eq!(
            p.releases,
            vec![Release {
                repo: repo("o/a"),
                issue: 3,
                pr: Some(10),
                reason: ReleaseReason::IssueClosed
            }]
        );
        assert_eq!(p.decision, claim("o/a", 5));
    }

    #[test]
    fn g10_merged_pr_and_closed_issue_leave_nothing_in_flight() {
        // After a merge neither the issue nor the PR is open, so they are not in the snapshot.
        let s = snap(vec![issue("o/b", 2, &["agent-ready"])], vec![]);
        assert_eq!(run(&s).decision, claim("o/b", 2));
    }

    #[test]
    fn g10_unknown_linked_issue_state_keeps_task_in_flight() {
        let mut open_pr = pr("o/a", 10, "agent/issue-3", &["agent-pr"]);
        open_pr.linked_issue_open = None;
        let s = snap(vec![issue("o/a", 5, &["agent-ready"])], vec![open_pr]);
        let p = run(&s);
        assert!(p.releases.is_empty());
        assert!(matches!(p.decision, Decision::Wait(_)));
    }

    #[test]
    fn g10_answered_blocked_issue_goes_ahead_of_older_queued_issues() {
        let mut s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/a", 2, &["agent-ready"]),
                issue("o/b", 8, &["agent-ready"]),
            ],
            vec![],
        );
        s.last_in_flight = Some((repo("o/b"), 8));
        assert_eq!(run(&s).decision, claim("o/b", 8));
    }

    #[test]
    fn g10_answered_issue_with_blocked_label_left_on_is_claimable() {
        let mut s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/a", 8, &["agent-ready", "agent-blocked"]),
            ],
            vec![],
        );
        s.last_in_flight = Some((repo("o/a"), 8));
        assert_eq!(run(&s).decision, claim("o/a", 8));
    }

    #[test]
    fn g10_last_in_flight_not_ready_falls_back_to_lowest() {
        let mut s = snap(vec![issue("o/a", 4, &["agent-ready"])], vec![]);
        s.last_in_flight = Some((repo("o/a"), 2));
        assert_eq!(run(&s).decision, claim("o/a", 4));
    }

    #[test]
    fn g10_conflicting_labels_count_as_in_flight() {
        let s = snap(
            vec![
                issue("o/a", 1, &["agent-ready"]),
                issue("o/a", 3, &["agent-blocked", "agent-in-review"]),
            ],
            vec![],
        );
        let p = run(&s);
        assert!(matches!(p.decision, Decision::Wait(_)));
        assert_eq!(p.label_problems.len(), 1);
    }

    #[test]
    fn g10_orphan_agent_pr_counts_as_in_flight() {
        let s = snap(
            vec![issue("o/a", 1, &["agent-ready"])],
            vec![pr("o/a", 10, "agent/something", &["agent-pr"])],
        );
        assert!(matches!(run(&s).decision, Decision::Wait(_)));
    }

    #[test]
    fn g1_g10_untrusted_ready_issue_is_never_claimed() {
        let mut stranger = issue("o/a", 1, &["agent-ready"]);
        stranger.author = "mallory".into();
        let s = snap(vec![stranger, issue("o/a", 2, &["agent-ready"])], vec![]);
        let p = run(&s);
        assert_eq!(p.decision, claim("o/a", 2));
        assert_eq!(p.untrusted_ready, vec![(repo("o/a"), 1)]);
    }

    #[test]
    fn g10_items_from_unconfigured_repos_are_ignored() {
        let s = snap(
            vec![
                issue("x/y", 1, &["agent-working"]),
                issue("o/b", 3, &["agent-ready"]),
            ],
            vec![],
        );
        assert_eq!(run(&s).decision, claim("o/b", 3));
    }

    #[test]
    fn g10_repo_names_match_case_insensitively() {
        let s = snap(
            vec![issue("O/A", 3, &["agent-in-review"])],
            vec![pr("o/a", 10, "agent/issue-3", &["agent-pr"])],
        );
        let p = run(&s);
        assert!(p.releases.is_empty());
        assert!(matches!(p.decision, Decision::Wait(_)));
    }
}

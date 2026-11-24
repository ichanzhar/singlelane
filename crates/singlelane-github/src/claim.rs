//! Claiming an item (SPEC 6.1, G5): swap the queue label for `agent-working`, re-read, verify.
//!
//! The swap removes the queue label first. GitHub answers 404 when the label is not on the
//! item, so of two claimers racing for the same item only one sees the removal succeed; the
//! other stops before it adds anything. The re-read then confirms the item is open and in the
//! `working` state with no queue label left, which also catches a concurrent manual edit.
//!
//! The attempt count (`max_attempts`) lives in the history store and is checked by the caller
//! before claiming.

use singlelane_core::policy::trust::TrustPolicy;
use singlelane_core::repo::{ItemKind, ItemRef, is_agent_branch};
use singlelane_core::state::{Event, ItemState, Label, agent_labels, transition};
use thiserror::Error;

use crate::api::GitHub;
use crate::error::GithubError;

/// A successful claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    /// The claimed item.
    pub item: ItemRef,
    /// The label that was swapped out (`agent-ready` or `agent-revise`).
    pub queue_label: Label,
    /// For a PR: its head branch. For an issue: `None`.
    pub head_branch: Option<String>,
    /// The item's labels after the claim, as re-read.
    pub labels: Vec<String>,
}

/// Why a claim did not happen.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClaimError {
    /// The item is not in its queue state (or not eligible at all). Nothing was changed.
    #[error("{item} can't be claimed: {reason}")]
    NotClaimable {
        /// The item.
        item: ItemRef,
        /// Why.
        reason: String,
    },
    /// Someone else removed the queue label first. Nothing was added.
    #[error("{item}: lost the claim, the queue label was already gone")]
    Lost {
        /// The item.
        item: ItemRef,
    },
    /// The re-read after the swap does not show the expected state. The job must not run.
    #[error("{item}: claim not confirmed: {reason} (labels now: {labels:?})")]
    NotConfirmed {
        /// The item.
        item: ItemRef,
        /// What was wrong.
        reason: String,
        /// Labels seen on the re-read.
        labels: Vec<String>,
    },
    /// A GitHub call failed.
    #[error("{item}: {source}")]
    GitHub {
        /// The item.
        item: ItemRef,
        /// The error.
        #[source]
        source: Box<GithubError>,
    },
}

/// What the claim reads about an item.
struct Snapshot {
    open: bool,
    author: Option<String>,
    labels: Vec<String>,
    head: Option<(String, bool)>,
}

async fn read(gh: &dyn GitHub, item: &ItemRef) -> Result<Snapshot, GithubError> {
    Ok(match item.kind {
        ItemKind::Issue => {
            let i = gh.get_issue(&item.repo, item.number).await?;
            Snapshot {
                open: i.open,
                author: i.author,
                labels: i.labels,
                head: None,
            }
        }
        ItemKind::Pr => {
            let p = gh.get_pull_request(&item.repo, item.number).await?;
            let in_repo = p.head_in_repo();
            Snapshot {
                open: p.open,
                author: p.author,
                labels: p.labels,
                head: Some((p.head_ref, in_repo)),
            }
        }
    })
}

/// Claims `item`: an issue labeled `agent-ready` by the trusted user, or an agent PR labeled
/// `agent-revise`.
pub async fn claim(
    gh: &dyn GitHub,
    trust: &TrustPolicy,
    item: &ItemRef,
) -> Result<Claimed, ClaimError> {
    let gh_err = |source| ClaimError::GitHub {
        item: item.clone(),
        source: Box::new(source),
    };
    let not_claimable = |reason: String| ClaimError::NotClaimable {
        item: item.clone(),
        reason,
    };

    // 1. Read and check eligibility.
    let before = read(gh, item).await.map_err(gh_err)?;
    let state = ItemState::from_labels(item.kind, before.open, &agent_labels(&before.labels))
        .map_err(|e| not_claimable(e.to_string()))?;
    let queue_state = match item.kind {
        ItemKind::Issue => ItemState::Ready,
        ItemKind::Pr => ItemState::Revise,
    };
    if state != queue_state {
        return Err(not_claimable(format!("it is {state}, not {queue_state}")));
    }
    if item.kind == ItemKind::Issue
        && !before
            .author
            .as_deref()
            .is_some_and(|a| trust.is_trusted(a))
    {
        // G1: never work on an issue from anyone but the trusted user.
        return Err(not_claimable(format!(
            "the author {:?} is not the trusted user",
            before.author
        )));
    }
    if let Some((head, in_repo)) = &before.head
        && (!is_agent_branch(head) || !in_repo)
    {
        return Err(not_claimable(format!(
            "the PR head {head:?} is not an agent/ branch of this repository"
        )));
    }
    let t = transition(item.kind, state, Event::Claim).map_err(|e| not_claimable(e.to_string()))?;
    let Some(queue_label) = state.label() else {
        return Err(not_claimable("no queue label".into()));
    };

    // 2. Swap: remove the queue label (the race is decided here), add agent-working, then
    //    drop agent-blocked if the owner left it on when requeueing.
    if !gh
        .remove_label(&item.repo, item.number, queue_label.as_str())
        .await
        .map_err(gh_err)?
    {
        return Err(ClaimError::Lost { item: item.clone() });
    }
    let add: Vec<String> = t.add.iter().map(|l| l.as_str().to_string()).collect();
    gh.add_labels(&item.repo, item.number, &add)
        .await
        .map_err(gh_err)?;
    for l in t.remove.iter().filter(|l| **l != queue_label) {
        if agent_labels(&before.labels).contains(l) {
            gh.remove_label(&item.repo, item.number, l.as_str())
                .await
                .map_err(gh_err)?;
        }
    }

    // 3. Re-read and verify.
    let after = read(gh, item).await.map_err(gh_err)?;
    let labels = agent_labels(&after.labels);
    let not_confirmed = |reason: &str| ClaimError::NotConfirmed {
        item: item.clone(),
        reason: reason.to_string(),
        labels: after.labels.clone(),
    };
    if !after.open {
        return Err(not_confirmed("the item was closed"));
    }
    if !labels.contains(&Label::Working) {
        return Err(not_confirmed("agent-working is missing"));
    }
    let extra: Vec<Label> = labels
        .iter()
        .copied()
        .filter(|l| !matches!(l, Label::Working | Label::Pr))
        .collect();
    if !extra.is_empty() {
        return Err(not_confirmed("other workflow labels are present"));
    }
    if item.kind == ItemKind::Pr && after.head != before.head {
        return Err(not_confirmed("the PR head changed"));
    }

    tracing::info!(item = %item, "claimed");
    Ok(Claimed {
        item: item.clone(),
        queue_label,
        head_branch: after.head.map(|(h, _)| h),
        labels: after.labels,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::fake::FakeGitHub;
    use singlelane_core::repo::RepoName;

    fn repo() -> RepoName {
        RepoName::parse("owner/repo").unwrap()
    }

    fn trust() -> TrustPolicy {
        TrustPolicy::new("owner")
    }

    #[tokio::test]
    async fn claim_issue_swaps_labels_and_drops_blocked() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_issue(
            &r,
            "owner",
            "t",
            "b",
            &["agent-ready", "agent-blocked", "bug"],
        );
        let c = claim(&gh, &trust(), &ItemRef::issue(r.clone(), n))
            .await
            .unwrap();
        assert_eq!(c.queue_label, Label::Ready);
        let mut labels = gh.labels(&r, n);
        labels.sort();
        assert_eq!(labels, vec!["agent-working", "bug"]);
    }

    #[tokio::test]
    async fn claim_revise_pr() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_pr(
            &r,
            "bot[bot]",
            "t",
            "",
            "agent/issue-4",
            &["agent-pr", "agent-revise"],
        );
        let c = claim(&gh, &trust(), &ItemRef::pr(r.clone(), n))
            .await
            .unwrap();
        assert_eq!(c.head_branch.as_deref(), Some("agent/issue-4"));
        let mut labels = gh.labels(&r, n);
        labels.sort();
        assert_eq!(labels, vec!["agent-pr", "agent-working"]);
    }

    #[tokio::test]
    async fn refuses_items_not_in_queue_state() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let working = gh.add_issue(&r, "owner", "t", "b", &["agent-working"]);
        let none = gh.add_issue(&r, "owner", "t", "b", &[]);
        let closed = gh.add_issue(&r, "owner", "t", "b", &["agent-ready"]);
        gh.set_open(&r, closed, false);
        let conflict = gh.add_issue(&r, "owner", "t", "b", &["agent-ready", "agent-in-review"]);
        for n in [working, none, closed, conflict] {
            let e = claim(&gh, &trust(), &ItemRef::issue(r.clone(), n))
                .await
                .unwrap_err();
            assert!(matches!(e, ClaimError::NotClaimable { .. }), "#{n}: {e}");
        }
        assert!(
            gh.calls().is_empty(),
            "nothing may be changed: {:?}",
            gh.calls()
        );
    }

    #[tokio::test]
    async fn g1_refuses_issues_from_other_authors() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_issue(&r, "mallory", "t", "b", &["agent-ready"]);
        let e = claim(&gh, &trust(), &ItemRef::issue(r.clone(), n))
            .await
            .unwrap_err();
        assert!(matches!(e, ClaimError::NotClaimable { .. }));
        assert_eq!(gh.labels(&r, n), vec!["agent-ready"]);
    }

    #[tokio::test]
    async fn g3_refuses_revise_on_non_agent_or_fork_branches() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_pr(&r, "owner", "t", "", "feature/x", &["agent-revise"]);
        let e = claim(&gh, &trust(), &ItemRef::pr(r.clone(), n))
            .await
            .unwrap_err();
        assert!(matches!(e, ClaimError::NotClaimable { .. }));
        let f = gh.add_pr(&r, "mallory", "t", "", "agent/issue-1", &["agent-revise"]);
        gh.with_state(|s| {
            if let Some(pr) = s
                .repos
                .get_mut(&repo())
                .and_then(|rr| rr.items.get_mut(&f))
                .and_then(|i| i.pr.as_mut())
            {
                pr.head_repo = Some("mallory/repo".into());
            }
        });
        let e = claim(&gh, &trust(), &ItemRef::pr(r.clone(), f))
            .await
            .unwrap_err();
        assert!(matches!(e, ClaimError::NotClaimable { .. }));
    }

    #[tokio::test]
    async fn g5_concurrent_edit_after_swap_is_not_confirmed() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_issue(&r, "owner", "t", "b", &["agent-ready"]);
        // The owner re-adds agent-ready right after our add: the re-read must catch it.
        gh.after_next("add_labels", move |s| {
            if let Some(i) = s.repos.get_mut(&repo()).and_then(|rr| rr.items.get_mut(&n)) {
                i.issue.labels.push("agent-ready".into());
            }
        });
        let e = claim(&gh, &trust(), &ItemRef::issue(r.clone(), n))
            .await
            .unwrap_err();
        assert!(matches!(e, ClaimError::NotConfirmed { .. }), "{e}");

        let m = gh.add_issue(&r, "owner", "t", "b", &["agent-ready"]);
        gh.after_next("add_labels", move |s| {
            if let Some(i) = s.repos.get_mut(&repo()).and_then(|rr| rr.items.get_mut(&m)) {
                i.issue.open = false;
            }
        });
        let e = claim(&gh, &trust(), &ItemRef::issue(r.clone(), m))
            .await
            .unwrap_err();
        assert!(matches!(e, ClaimError::NotConfirmed { .. }), "{e}");
    }

    #[tokio::test]
    async fn github_errors_are_reported() {
        let gh = FakeGitHub::new();
        let r = repo();
        gh.add_repo(&r);
        let n = gh.add_issue(&r, "owner", "t", "b", &["agent-ready"]);
        gh.fail_next("remove_label", 502);
        let e = claim(&gh, &trust(), &ItemRef::issue(r.clone(), n))
            .await
            .unwrap_err();
        assert!(matches!(e, ClaimError::GitHub { .. }));
        assert_eq!(gh.labels(&r, n), vec!["agent-ready"]);
    }

    /// G5: two claimers race for the same issue; exactly one wins, every time, and the item
    /// ends with exactly one `agent-working` and no queue label.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn g5_two_concurrent_claimers_only_one_wins() {
        for round in 0..200 {
            let gh = Arc::new(FakeGitHub::new());
            let r = repo();
            gh.add_repo(&r);
            let n = gh.add_issue(&r, "owner", "t", "b", &["agent-ready", "agent-blocked"]);
            let item = ItemRef::issue(r.clone(), n);
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let spawn = |gh: Arc<FakeGitHub>, item: ItemRef, b: Arc<tokio::sync::Barrier>| {
                tokio::spawn(async move {
                    b.wait().await;
                    claim(gh.as_ref(), &trust(), &item).await
                })
            };
            let a = spawn(Arc::clone(&gh), item.clone(), Arc::clone(&barrier));
            let b = spawn(Arc::clone(&gh), item.clone(), Arc::clone(&barrier));
            let (a, b) = (a.await.unwrap(), b.await.unwrap());
            let wins = [&a, &b].iter().filter(|r| r.is_ok()).count();
            assert_eq!(wins, 1, "round {round}: {a:?} / {b:?}");
            for res in [&a, &b] {
                if let Err(e) = res {
                    assert!(
                        matches!(e, ClaimError::Lost { .. } | ClaimError::NotClaimable { .. }),
                        "round {round}: unexpected loser error {e}"
                    );
                }
            }
            let labels = gh.labels(&r, n);
            assert_eq!(labels, vec!["agent-working"], "round {round}");
        }
    }
}

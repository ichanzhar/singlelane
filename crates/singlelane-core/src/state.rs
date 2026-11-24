//! Labels, item states and the transition table (SPEC 3).
//!
//! GitHub labels are the source of truth for item state. [`ItemState::from_labels`] derives
//! the state of one item from its labels, and [`transition`] maps a state and an event to the
//! next state plus the exact label changes to apply. Invalid transitions are errors.

use std::fmt;

use thiserror::Error;

use crate::repo::ItemKind;

/// The labels the factory manages (SPEC 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Label {
    /// Issue queued for the factory.
    Ready,
    /// A job is running on this issue or PR.
    Working,
    /// Needs a human: a question from the agent, or a failure.
    Blocked,
    /// Issue has an open agent PR waiting for human review.
    InReview,
    /// On a PR: apply the owner's review comments.
    Revise,
    /// Marks PRs opened by the factory.
    Pr,
}

impl Label {
    /// All labels, in the order `setup-labels` creates them.
    pub const ALL: [Label; 6] = [
        Label::Ready,
        Label::Working,
        Label::Blocked,
        Label::InReview,
        Label::Revise,
        Label::Pr,
    ];

    /// The GitHub label name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "agent-ready",
            Self::Working => "agent-working",
            Self::Blocked => "agent-blocked",
            Self::InReview => "agent-in-review",
            Self::Revise => "agent-revise",
            Self::Pr => "agent-pr",
        }
    }

    /// Parses a GitHub label name. Label names on GitHub are case-insensitive.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|l| l.as_str().eq_ignore_ascii_case(name.trim()))
    }

    /// Label description for `setup-labels`.
    pub fn description(self) -> &'static str {
        match self {
            Self::Ready => "Issue queued for the singlelane factory",
            Self::Working => "A singlelane job is running on this item",
            Self::Blocked => "Needs a human: a question from the agent, or a failure",
            Self::InReview => "Issue has an open agent PR waiting for human review",
            Self::Revise => "On an agent PR: apply the owner's review comments",
            Self::Pr => "PR opened by the singlelane factory",
        }
    }

    /// Label color (hex, no `#`) for `setup-labels`.
    pub fn color(self) -> &'static str {
        match self {
            Self::Ready => "0e8a16",
            Self::Working => "fbca04",
            Self::Blocked => "d93f0b",
            Self::InReview => "1d76db",
            Self::Revise => "5319e7",
            Self::Pr => "c5def5",
        }
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the agent labels out of a list of GitHub label names, ignoring everything else.
pub fn agent_labels<S: AsRef<str>>(names: &[S]) -> Vec<Label> {
    let mut out: Vec<Label> = names
        .iter()
        .filter_map(|n| Label::parse(n.as_ref()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// State of one issue or PR, derived from its labels and open/closed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemState {
    /// Open, no workflow label. For a PR: only `agent-pr` (waiting for review).
    Idle,
    /// Issue labeled `agent-ready`.
    Ready,
    /// PR labeled `agent-revise`.
    Revise,
    /// `agent-working`.
    Working,
    /// `agent-blocked`.
    Blocked,
    /// Issue labeled `agent-in-review`.
    InReview,
    /// Closed (merged or not). Terminal.
    Closed,
}

impl ItemState {
    /// Every state, for exhaustive tests.
    pub const ALL: [ItemState; 7] = [
        ItemState::Idle,
        ItemState::Ready,
        ItemState::Revise,
        ItemState::Working,
        ItemState::Blocked,
        ItemState::InReview,
        ItemState::Closed,
    ];

    /// The workflow label that represents this state, if any.
    pub fn label(self) -> Option<Label> {
        match self {
            Self::Idle | Self::Closed => None,
            Self::Ready => Some(Label::Ready),
            Self::Revise => Some(Label::Revise),
            Self::Working => Some(Label::Working),
            Self::Blocked => Some(Label::Blocked),
            Self::InReview => Some(Label::InReview),
        }
    }

    /// Whether this state exists for the given item kind.
    pub fn valid_for(self, kind: ItemKind) -> bool {
        match self {
            Self::Ready | Self::InReview => kind == ItemKind::Issue,
            Self::Revise => kind == ItemKind::Pr,
            Self::Idle | Self::Working | Self::Blocked | Self::Closed => true,
        }
    }

    /// Whether an issue in this state counts as in flight (SPEC 3.1).
    pub fn issue_in_flight(self) -> bool {
        matches!(self, Self::Working | Self::Blocked | Self::InReview)
    }

    /// Derives the state from the labels of an item.
    ///
    /// Rules:
    /// - closed items are [`ItemState::Closed`] whatever their labels;
    /// - `agent-working` wins over everything (a job owns the item);
    /// - `agent-ready` together with `agent-blocked` on an issue means the owner answered the
    ///   question and requeued it without removing `agent-blocked`: [`ItemState::Ready`];
    /// - `agent-revise` together with `agent-blocked` on a PR likewise is [`ItemState::Revise`];
    /// - any other combination of two workflow labels is a [`StateError::ConflictingLabels`];
    /// - labels that don't exist for the kind (`agent-revise` on an issue) are ignored.
    pub fn from_labels(kind: ItemKind, open: bool, labels: &[Label]) -> Result<Self, StateError> {
        if !open {
            return Ok(Self::Closed);
        }
        let has = |l: Label| labels.contains(&l);
        let workflow: Vec<Label> = [
            Label::Working,
            Label::Ready,
            Label::Revise,
            Label::Blocked,
            Label::InReview,
        ]
        .into_iter()
        .filter(|l| has(*l))
        .filter(|l| match l {
            Label::Ready | Label::InReview => kind == ItemKind::Issue,
            Label::Revise => kind == ItemKind::Pr,
            _ => true,
        })
        .collect();

        let state = match workflow.as_slice() {
            [] => Self::Idle,
            [Label::Working, ..] => Self::Working,
            [Label::Ready] => Self::Ready,
            [Label::Revise] => Self::Revise,
            [Label::Blocked] => Self::Blocked,
            [Label::InReview] => Self::InReview,
            [Label::Ready, Label::Blocked] => Self::Ready,
            [Label::Revise, Label::Blocked] => Self::Revise,
            other => {
                return Err(StateError::ConflictingLabels {
                    kind,
                    labels: other.to_vec(),
                });
            }
        };
        Ok(state)
    }
}

impl fmt::Display for ItemState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Idle => "idle",
            Self::Ready => "ready",
            Self::Revise => "revise requested",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::InReview => "in review",
            Self::Closed => "closed",
        })
    }
}

/// Something that happens to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    /// The factory claims the item for a job (6.1).
    Claim,
    /// The claim found `max_attempts` exceeded: block instead of running (6.1).
    AttemptsExceeded,
    /// Issue job: PR opened or updated (6.6).
    PrPublished,
    /// Revise job: fixes pushed and commented (6.6).
    RevisePublished,
    /// The agent asked a question (`out/status` = `blocked`), or G4 refused the push.
    AgentBlocked,
    /// A counted failure (`agent_failed`, `factory_bug`) blocks the item (6.8).
    FailedBlocking,
    /// A non-counted failure (`network`, `claude_auth`, usage limit, `github_auth`, `infra`)
    /// or startup recovery (6.7) puts the item back in its queue.
    Requeue,
    /// The owner (re)queues an issue: sets `agent-ready`.
    OwnerReady,
    /// The owner requests changes on a PR: sets `agent-revise`.
    OwnerRevise,
    /// The agent PR was closed without merge.
    PrClosedUnmerged,
    /// The agent PR was merged (this also closes the issue via `Closes #n`).
    PrMerged,
    /// The issue was closed by the owner.
    IssueClosed,
}

impl Event {
    /// Every event, for exhaustive tests.
    pub const ALL: [Event; 12] = [
        Event::Claim,
        Event::AttemptsExceeded,
        Event::PrPublished,
        Event::RevisePublished,
        Event::AgentBlocked,
        Event::FailedBlocking,
        Event::Requeue,
        Event::OwnerReady,
        Event::OwnerRevise,
        Event::PrClosedUnmerged,
        Event::PrMerged,
        Event::IssueClosed,
    ];
}

/// The result of a valid transition: the next state and the label changes to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// State before.
    pub from: ItemState,
    /// State after.
    pub to: ItemState,
    /// Labels to remove (only labels the factory manages).
    pub remove: Vec<Label>,
    /// Labels to add.
    pub add: Vec<Label>,
}

/// State machine errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateError {
    /// The event is not allowed in this state.
    #[error("invalid transition: {event:?} on {kind} in state {from}")]
    InvalidTransition {
        /// Item kind.
        kind: ItemKind,
        /// Current state.
        from: ItemState,
        /// Rejected event.
        event: Event,
    },
    /// The state does not exist for this item kind.
    #[error("state {state} does not exist for a {kind}")]
    InvalidState {
        /// Item kind.
        kind: ItemKind,
        /// State.
        state: ItemState,
    },
    /// The item carries workflow labels that can't be combined.
    #[error("conflicting labels on {kind}: {}", fmt_labels(labels))]
    ConflictingLabels {
        /// Item kind.
        kind: ItemKind,
        /// The workflow labels found.
        labels: Vec<Label>,
    },
}

fn fmt_labels(labels: &[Label]) -> String {
    labels
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Applies `event` to an item of `kind` in state `from`.
///
/// Label changes: the old state's label is removed and the new state's label added. For a
/// release of an issue (PR closed without merge) every workflow label is removed so stray
/// labels from manual edits are cleaned up too. `agent-pr` is never touched: it stays on the
/// PR for its lifetime.
pub fn transition(kind: ItemKind, from: ItemState, event: Event) -> Result<Transition, StateError> {
    use Event as E;
    use ItemKind as K;
    use ItemState as S;

    if !from.valid_for(kind) {
        return Err(StateError::InvalidState { kind, state: from });
    }
    let invalid = || StateError::InvalidTransition { kind, from, event };

    let to = match (kind, from, event) {
        (_, S::Closed, _) => return Err(invalid()),

        // Claim and the max-attempts guard (6.1).
        (K::Issue, S::Ready, E::Claim) | (K::Pr, S::Revise, E::Claim) => S::Working,
        (K::Issue, S::Ready, E::AttemptsExceeded) | (K::Pr, S::Revise, E::AttemptsExceeded) => {
            S::Blocked
        }

        // Job outcomes (6.6, 6.8).
        (K::Issue, S::Working, E::PrPublished) => S::InReview,
        (K::Pr, S::Working, E::RevisePublished) => S::Idle,
        (_, S::Working, E::AgentBlocked | E::FailedBlocking) => S::Blocked,
        (K::Issue, S::Working, E::Requeue) => S::Ready,
        (K::Pr, S::Working, E::Requeue) => S::Revise,

        // Owner actions.
        (K::Issue, S::Idle | S::Blocked, E::OwnerReady) => S::Ready,
        (K::Pr, S::Idle | S::Blocked, E::OwnerRevise) => S::Revise,

        // Release (SPEC 3). The issue loses its agent labels but stays open.
        (K::Issue, S::Working | S::Blocked | S::InReview, E::PrClosedUnmerged) => S::Idle,
        (K::Pr, _, E::PrClosedUnmerged | E::PrMerged) => S::Closed,
        (K::Issue, _, E::PrMerged | E::IssueClosed) => S::Closed,
        // Closing the issue closes the open agent PR too.
        (K::Pr, _, E::IssueClosed) => S::Closed,

        _ => return Err(invalid()),
    };

    let (remove, add) = if to == S::Closed {
        (Vec::new(), Vec::new())
    } else if kind == K::Issue && event == E::PrClosedUnmerged {
        (
            vec![
                Label::Ready,
                Label::Working,
                Label::Blocked,
                Label::InReview,
            ],
            Vec::new(),
        )
    } else {
        let mut remove: Vec<Label> = from.label().into_iter().collect();
        // Requeueing an answered blocked item: the owner may have left `agent-blocked` on.
        if matches!(to, S::Working) {
            remove.push(Label::Blocked);
        }
        remove.sort();
        remove.dedup();
        let add: Vec<Label> = to.label().into_iter().collect();
        remove.retain(|l| !add.contains(l));
        (remove, add)
    };

    Ok(Transition {
        from,
        to,
        remove,
        add,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ItemKind as K;
    use ItemState as S;

    /// The complete expected transition table. Anything not listed must be rejected.
    fn expected(kind: ItemKind, from: ItemState, event: Event) -> Option<ItemState> {
        use Event as E;
        let table: &[(ItemKind, ItemState, Event, ItemState)] = &[
            (K::Issue, S::Ready, E::Claim, S::Working),
            (K::Issue, S::Ready, E::AttemptsExceeded, S::Blocked),
            (K::Issue, S::Working, E::PrPublished, S::InReview),
            (K::Issue, S::Working, E::AgentBlocked, S::Blocked),
            (K::Issue, S::Working, E::FailedBlocking, S::Blocked),
            (K::Issue, S::Working, E::Requeue, S::Ready),
            (K::Issue, S::Idle, E::OwnerReady, S::Ready),
            (K::Issue, S::Blocked, E::OwnerReady, S::Ready),
            (K::Issue, S::Working, E::PrClosedUnmerged, S::Idle),
            (K::Issue, S::Blocked, E::PrClosedUnmerged, S::Idle),
            (K::Issue, S::InReview, E::PrClosedUnmerged, S::Idle),
            (K::Pr, S::Revise, E::Claim, S::Working),
            (K::Pr, S::Revise, E::AttemptsExceeded, S::Blocked),
            (K::Pr, S::Working, E::RevisePublished, S::Idle),
            (K::Pr, S::Working, E::AgentBlocked, S::Blocked),
            (K::Pr, S::Working, E::FailedBlocking, S::Blocked),
            (K::Pr, S::Working, E::Requeue, S::Revise),
            (K::Pr, S::Idle, E::OwnerRevise, S::Revise),
            (K::Pr, S::Blocked, E::OwnerRevise, S::Revise),
        ];
        if let Some((_, _, _, to)) = table
            .iter()
            .find(|(k, f, e, _)| *k == kind && *f == from && *e == event)
        {
            return Some(*to);
        }
        let closing = match kind {
            K::Issue => matches!(event, E::PrMerged | E::IssueClosed),
            K::Pr => matches!(event, E::PrMerged | E::PrClosedUnmerged | E::IssueClosed),
        };
        (closing && from != S::Closed).then_some(S::Closed)
    }

    #[test]
    fn state_machine_is_exhaustively_as_specified() {
        let mut valid = 0;
        for kind in [K::Issue, K::Pr] {
            for from in ItemState::ALL {
                for event in Event::ALL {
                    let got = transition(kind, from, event);
                    if !from.valid_for(kind) {
                        assert_eq!(
                            got,
                            Err(StateError::InvalidState { kind, state: from }),
                            "{kind:?} {from:?} {event:?}"
                        );
                        continue;
                    }
                    match expected(kind, from, event) {
                        Some(to) => {
                            let t = got.unwrap_or_else(|e| {
                                panic!("{kind:?} {from:?} {event:?} rejected: {e}")
                            });
                            assert_eq!(t.to, to, "{kind:?} {from:?} {event:?}");
                            assert!(t.to.valid_for(kind));
                            valid += 1;
                        }
                        None => assert_eq!(
                            got,
                            Err(StateError::InvalidTransition { kind, from, event }),
                            "{kind:?} {from:?} {event:?} should be rejected"
                        ),
                    }
                }
            }
        }
        // 19 table rows + issue (5 open states x 2 closing events) + PR (4 open states x 3).
        assert_eq!(valid, 19 + 10 + 12);
    }

    #[test]
    fn label_changes_follow_states() {
        for kind in [K::Issue, K::Pr] {
            for from in ItemState::ALL {
                for event in Event::ALL {
                    let Ok(t) = transition(kind, from, event) else {
                        continue;
                    };
                    assert!(!t.add.contains(&Label::Pr) && !t.remove.contains(&Label::Pr));
                    for l in &t.add {
                        assert!(!t.remove.contains(l), "{kind:?} {from:?} {event:?}");
                    }
                    if t.to == S::Closed {
                        assert!(t.add.is_empty() && t.remove.is_empty());
                        continue;
                    }
                    // Applying the label changes to the from-state's label yields the
                    // to-state when re-derived from labels.
                    let mut labels: Vec<Label> = from.label().into_iter().collect();
                    labels.retain(|l| !t.remove.contains(l));
                    labels.extend(t.add.iter().copied());
                    assert_eq!(
                        ItemState::from_labels(kind, true, &labels),
                        Ok(t.to),
                        "{kind:?} {from:?} {event:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn claim_swaps_ready_for_working() {
        let t = transition(K::Issue, S::Ready, Event::Claim).unwrap();
        assert_eq!(t.remove, vec![Label::Ready, Label::Blocked]);
        assert_eq!(t.add, vec![Label::Working]);
        let t = transition(K::Pr, S::Revise, Event::Claim).unwrap();
        assert_eq!(t.remove, vec![Label::Blocked, Label::Revise]);
        assert_eq!(t.add, vec![Label::Working]);
    }

    #[test]
    fn pr_closed_unmerged_strips_all_issue_labels() {
        let t = transition(K::Issue, S::InReview, Event::PrClosedUnmerged).unwrap();
        assert_eq!(t.to, S::Idle);
        assert!(t.add.is_empty());
        for l in [
            Label::Ready,
            Label::Working,
            Label::Blocked,
            Label::InReview,
        ] {
            assert!(t.remove.contains(&l));
        }
    }

    #[test]
    fn closed_is_terminal() {
        for kind in [K::Issue, K::Pr] {
            for event in Event::ALL {
                assert!(transition(kind, S::Closed, event).is_err());
            }
        }
    }

    #[test]
    fn from_labels_rules() {
        use Label as L;
        let f = ItemState::from_labels;
        assert_eq!(f(K::Issue, true, &[]), Ok(S::Idle));
        assert_eq!(f(K::Issue, false, &[L::Working]), Ok(S::Closed));
        assert_eq!(f(K::Issue, true, &[L::Ready]), Ok(S::Ready));
        assert_eq!(f(K::Issue, true, &[L::Ready, L::Blocked]), Ok(S::Ready));
        assert_eq!(f(K::Issue, true, &[L::Working, L::Ready]), Ok(S::Working));
        assert_eq!(f(K::Issue, true, &[L::InReview]), Ok(S::InReview));
        assert_eq!(f(K::Issue, true, &[L::Revise]), Ok(S::Idle));
        assert_eq!(f(K::Pr, true, &[L::Pr]), Ok(S::Idle));
        assert_eq!(f(K::Pr, true, &[L::Pr, L::Revise]), Ok(S::Revise));
        assert_eq!(
            f(K::Pr, true, &[L::Pr, L::Revise, L::Blocked]),
            Ok(S::Revise)
        );
        assert_eq!(f(K::Pr, true, &[L::Pr, L::Ready]), Ok(S::Idle));
        assert!(matches!(
            f(K::Issue, true, &[L::Ready, L::InReview]),
            Err(StateError::ConflictingLabels { .. })
        ));
        assert!(matches!(
            f(K::Issue, true, &[L::Blocked, L::InReview]),
            Err(StateError::ConflictingLabels { .. })
        ));
    }

    #[test]
    fn label_names_round_trip() {
        for l in Label::ALL {
            assert_eq!(Label::parse(l.as_str()), Some(l));
            assert_eq!(Label::parse(&l.as_str().to_uppercase()), Some(l));
        }
        assert_eq!(Label::parse("bug"), None);
        assert_eq!(
            agent_labels(&["bug", "agent-ready", "AGENT-READY", "agent-pr"]),
            vec![Label::Ready, Label::Pr]
        );
    }
}

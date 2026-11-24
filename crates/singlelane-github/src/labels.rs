//! `setup-labels`: create or update the workflow labels (SPEC 3) in every configured repo.

use singlelane_core::repo::RepoName;
use singlelane_core::state::Label;

use crate::api::GitHub;
use crate::error::GithubError;
use crate::types::{LabelChange, LabelInfo};

/// The label definition `setup-labels` applies.
pub fn label_spec(label: Label) -> LabelInfo {
    LabelInfo {
        name: label.as_str().to_string(),
        color: label.color().to_string(),
        description: label.description().to_string(),
    }
}

/// What happened to one label in one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelOutcome {
    /// Repository.
    pub repo: RepoName,
    /// Label.
    pub label: Label,
    /// The change, or the error.
    pub result: Result<LabelChange, GithubError>,
}

/// Creates or updates every workflow label in every repo. Continues after errors and reports
/// each label separately.
pub async fn setup_labels(gh: &dyn GitHub, repos: &[RepoName]) -> Vec<LabelOutcome> {
    let mut out = Vec::new();
    for repo in repos {
        for label in Label::ALL {
            let result = gh.create_or_update_label(repo, &label_spec(label)).await;
            out.push(LabelOutcome {
                repo: repo.clone(),
                label,
                result,
            });
        }
    }
    out
}

/// Workflow labels missing from `existing` (names compared case-insensitively).
pub fn missing_labels(existing: &[LabelInfo]) -> Vec<Label> {
    Label::ALL
        .into_iter()
        .filter(|l| {
            !existing
                .iter()
                .any(|e| e.name.eq_ignore_ascii_case(l.as_str()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeGitHub;

    #[tokio::test]
    async fn setup_labels_creates_then_is_idempotent() {
        let gh = FakeGitHub::new();
        let r = RepoName::parse("owner/repo").unwrap();
        gh.add_repo(&r);
        let first = setup_labels(&gh, std::slice::from_ref(&r)).await;
        assert_eq!(first.len(), 6);
        assert!(first.iter().all(|o| o.result == Ok(LabelChange::Created)));
        let second = setup_labels(&gh, std::slice::from_ref(&r)).await;
        assert!(
            second
                .iter()
                .all(|o| o.result == Ok(LabelChange::Unchanged))
        );
        assert!(missing_labels(&gh.list_labels(&r).await.unwrap()).is_empty());
    }

    #[tokio::test]
    async fn changed_color_is_updated_and_errors_are_reported_per_label() {
        let gh = FakeGitHub::new();
        let r = RepoName::parse("owner/repo").unwrap();
        gh.add_repo(&r);
        let mut wrong = label_spec(Label::Ready);
        wrong.color = "000000".into();
        gh.create_or_update_label(&r, &wrong).await.unwrap();
        gh.fail_next("create_or_update_label", 403);
        let out = setup_labels(&gh, std::slice::from_ref(&r)).await;
        assert!(out[0].result.is_err());
        // The failed one was agent-ready; run again to see the update.
        let out = setup_labels(&gh, std::slice::from_ref(&r)).await;
        assert_eq!(out[0].result, Ok(LabelChange::Updated));
        let missing = missing_labels(&[label_spec(Label::Ready), label_spec(Label::Pr)]);
        assert_eq!(missing.len(), 4);
    }
}

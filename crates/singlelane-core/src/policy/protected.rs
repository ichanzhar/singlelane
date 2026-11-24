//! Protected-path guard (G4).
//!
//! If the diff since the job's base commit touches any path matching the repo's
//! `protected_paths` globs, nothing is pushed. Matching rules:
//! - globs are matched against the full repo-relative path, `/` is a separator that `*` does
//!   not cross (`**` does), so `CODEOWNERS` only matches the file in the repo root;
//! - matching is case-insensitive: overblocking is safer than letting `.GitHub/...` through;
//! - a pattern also protects everything below it, so `.github/workflows` covers its files.
//!
//! Callers must list both sides of renames (`git diff --name-only --no-renames`), so moving
//! a protected file away is caught too.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use thiserror::Error;

/// A compiled set of protected path globs.
#[derive(Debug, Clone)]
pub struct ProtectedPaths {
    patterns: Vec<String>,
    set: GlobSet,
}

/// A glob that does not compile.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid protected path glob {pattern:?}: {reason}")]
pub struct ProtectedPathsError {
    /// The offending pattern.
    pub pattern: String,
    /// The globset error.
    pub reason: String,
}

fn build(pattern: &str) -> Result<Glob, ProtectedPathsError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(true)
        .backslash_escape(true)
        .empty_alternates(true)
        .build()
        .map_err(|e| ProtectedPathsError {
            pattern: pattern.to_string(),
            reason: e.kind().to_string(),
        })
}

fn normalize(path: &str) -> &str {
    let mut p = path.trim();
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    p.trim_start_matches('/')
}

impl ProtectedPaths {
    /// Compiles the globs. Empty patterns are rejected.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self, ProtectedPathsError> {
        let mut builder = GlobSetBuilder::new();
        let mut kept = Vec::new();
        for raw in patterns {
            let pattern = normalize(raw.as_ref()).trim_end_matches('/');
            if pattern.is_empty() {
                return Err(ProtectedPathsError {
                    pattern: raw.as_ref().to_string(),
                    reason: "empty pattern".into(),
                });
            }
            builder.add(build(pattern)?);
            // Protect everything below a directory pattern too.
            if !pattern.ends_with("/**") {
                builder.add(build(&format!("{pattern}/**"))?);
            }
            kept.push(raw.as_ref().to_string());
        }
        let set = builder.build().map_err(|e| ProtectedPathsError {
            pattern: kept.join(", "),
            reason: e.to_string(),
        })?;
        Ok(Self {
            patterns: kept,
            set,
        })
    }

    /// The patterns as configured.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Whether one path is protected.
    pub fn is_protected(&self, path: &str) -> bool {
        let p = normalize(path);
        !p.is_empty() && self.set.is_match(p)
    }

    /// The protected paths among `changed`, in input order.
    pub fn violations<S: AsRef<str>>(&self, changed: &[S]) -> Vec<String> {
        changed
            .iter()
            .map(AsRef::as_ref)
            .filter(|p| self.is_protected(p))
            .map(|p| normalize(p).to_string())
            .collect()
    }

    /// The blocked-item explanation for a set of violations (posted as a comment).
    pub fn explain(violations: &[String]) -> String {
        let list: Vec<String> = violations.iter().map(|p| format!("- `{p}`")).collect();
        format!(
            "The agent's changes touch protected paths, so nothing was pushed:\n\n{}\n\n\
             Protected paths are set in the singlelane config (`protected_paths`). Make these \
             changes yourself, or adjust the issue and add `agent-ready` again.",
            list.join("\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_guard() -> ProtectedPaths {
        ProtectedPaths::new(&[
            ".github/workflows/**",
            ".github/actions/**",
            "CODEOWNERS",
            ".github/CODEOWNERS",
        ])
        .unwrap()
    }

    #[test]
    fn g4_workflow_and_action_files_are_protected() {
        let g = default_guard();
        assert!(g.is_protected(".github/workflows/ci.yml"));
        assert!(g.is_protected(".github/workflows/sub/dir/x.yaml"));
        assert!(g.is_protected(".github/actions/setup/action.yml"));
        assert!(g.is_protected("./.github/workflows/ci.yml"));
        assert!(g.is_protected(".GitHub/Workflows/ci.yml"));
    }

    #[test]
    fn g4_codeowners_in_root_and_dot_github() {
        let g = default_guard();
        assert!(g.is_protected("CODEOWNERS"));
        assert!(g.is_protected(".github/CODEOWNERS"));
        assert!(g.is_protected("codeowners"));
        // Not listed in the default config: docs/CODEOWNERS and nested copies.
        assert!(!g.is_protected("docs/CODEOWNERS"));
        assert!(!g.is_protected("src/CODEOWNERS"));
        assert!(!g.is_protected("CODEOWNERS.md"));
    }

    #[test]
    fn g4_codeowners_anywhere_with_double_star() {
        let g = ProtectedPaths::new(&["**/CODEOWNERS"]).unwrap();
        assert!(g.is_protected("CODEOWNERS"));
        assert!(g.is_protected(".github/CODEOWNERS"));
        assert!(g.is_protected("docs/CODEOWNERS"));
    }

    #[test]
    fn g4_ordinary_files_are_not_protected() {
        let g = default_guard();
        for p in [
            "src/main.rs",
            ".github/dependabot.yml",
            ".github/ISSUE_TEMPLATE/bug.md",
            "workflows/ci.yml",
            "docs/.github/workflows/ci.yml",
            "",
        ] {
            assert!(!g.is_protected(p), "{p}");
        }
    }

    #[test]
    fn g4_violations_lists_matches_in_order() {
        let g = default_guard();
        let v = g.violations(&["src/a.rs", "CODEOWNERS", ".github/workflows/ci.yml"]);
        assert_eq!(v, vec!["CODEOWNERS", ".github/workflows/ci.yml"]);
        let text = ProtectedPaths::explain(&v);
        assert!(text.contains("`CODEOWNERS`"));
    }

    #[test]
    fn g4_directory_pattern_protects_its_contents() {
        let g = ProtectedPaths::new(&["deploy", "infra/"]).unwrap();
        assert!(g.is_protected("deploy"));
        assert!(g.is_protected("deploy/k8s/app.yaml"));
        assert!(g.is_protected("infra/main.tf"));
        assert!(!g.is_protected("deployment.md"));
    }

    #[test]
    fn g4_bad_globs_are_rejected() {
        assert!(ProtectedPaths::new(&["a[b"]).is_err());
        assert!(ProtectedPaths::new(&[""]).is_err());
    }
}

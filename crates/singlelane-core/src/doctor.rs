//! `doctor` check results and their output format (SPEC 9).
//!
//! Every check prints one line, `PASS`, `WARN` or `FAIL`, followed by a fix hint for
//! anything that is not a pass. The checks themselves live next to the I/O they need; this
//! module only defines the result type and the formatting, so all crates print the same way.

use std::fmt;

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckStatus {
    /// Fine.
    Pass,
    /// Works, but needs attention.
    Warn,
    /// Broken: singlelane must not run like this.
    Fail,
}

impl CheckStatus {
    /// `PASS`, `WARN` or `FAIL`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Short check name, for example `installation permissions`.
    pub name: String,
    /// Outcome.
    pub status: CheckStatus,
    /// What was found.
    pub detail: String,
    /// How to fix it (for `WARN` and `FAIL`).
    pub hint: Option<String>,
}

impl Check {
    /// A passing check.
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
            hint: None,
        }
    }

    /// A warning with a fix hint.
    pub fn warn(
        name: impl Into<String>,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    /// A failure with a fix hint.
    pub fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    /// The printed form: `STATUS name: detail`, plus an indented `fix:` line if there is a
    /// hint and the check did not pass.
    pub fn render(&self) -> String {
        let mut out = format!("{} {}: {}", self.status, self.name, self.detail);
        if self.status != CheckStatus::Pass
            && let Some(hint) = &self.hint
        {
            out.push_str("\n     fix: ");
            out.push_str(hint);
        }
        out
    }
}

/// Counts of each status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Passing checks.
    pub pass: usize,
    /// Warnings.
    pub warn: usize,
    /// Failures.
    pub fail: usize,
}

impl Summary {
    /// Counts the statuses of `checks`.
    pub fn of(checks: &[Check]) -> Self {
        let mut s = Self::default();
        for c in checks {
            match c.status {
                CheckStatus::Pass => s.pass += 1,
                CheckStatus::Warn => s.warn += 1,
                CheckStatus::Fail => s.fail += 1,
            }
        }
        s
    }

    /// `doctor` succeeds when nothing failed. Warnings don't fail it.
    pub fn ok(&self) -> bool {
        self.fail == 0
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} passed, {} warning(s), {} failed",
            self.pass, self.warn, self.fail
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_pass_has_no_hint() {
        let c = Check {
            hint: Some("ignored".into()),
            ..Check::pass("config", "valid")
        };
        assert_eq!(c.render(), "PASS config: valid");
    }

    #[test]
    fn render_fail_and_warn_show_hint() {
        let c = Check::fail(
            "key file",
            "mode 644",
            "chmod 600 /etc/singlelane/github-app.pem",
        );
        assert_eq!(
            c.render(),
            "FAIL key file: mode 644\n     fix: chmod 600 /etc/singlelane/github-app.pem"
        );
        let w = Check::warn("branch", "not protected", "add a branch rule");
        assert!(
            w.render()
                .starts_with("WARN branch: not protected\n     fix: ")
        );
    }

    #[test]
    fn summary_counts_and_ok() {
        let checks = vec![
            Check::pass("a", "x"),
            Check::warn("b", "x", "y"),
            Check::warn("c", "x", "y"),
        ];
        let s = Summary::of(&checks);
        assert_eq!((s.pass, s.warn, s.fail), (1, 2, 0));
        assert!(s.ok());
        let mut checks = checks;
        checks.push(Check::fail("d", "x", "y"));
        assert!(!Summary::of(&checks).ok());
        assert_eq!(
            Summary::of(&checks).to_string(),
            "1 passed, 2 warning(s), 1 failed"
        );
    }
}

//! Telegram message formatting (SPEC 11).
//!
//! Messages use Telegram's HTML parse mode. Every dynamic value is redacted (G8), cut to a
//! per-field limit, then HTML-escaped; links point at the issue or PR. The final text is
//! redacted once more and kept within Telegram's 4096-character limit without cutting
//! through a tag or an entity.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::failure::FailureClass;
use crate::inflight::ReleaseReason;
use crate::policy::Redactor;
use crate::repo::{ItemKind, ItemRef};

/// Telegram's maximum message length, in characters.
pub const TELEGRAM_MAX_CHARS: usize = 4096;
/// Characters of the agent's question shown in `blocked` (SPEC 11).
pub const QUESTION_PREVIEW_CHARS: usize = 300;
const TITLE_CHARS: usize = 200;
const REASON_CHARS: usize = 500;
const LIST_ITEMS: usize = 10;

/// Notification event kinds, as named in `telegram.notify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    /// PR created.
    PrOpened,
    /// The agent asked a question.
    Blocked,
    /// `agent_failed` or `factory_bug`.
    Failed,
    /// The factory paused.
    Paused,
    /// The factory resumed.
    Resumed,
    /// Claude token is about to turn one year old.
    TokenExpiry,
    /// Connectivity back after a long outage.
    NetworkRestored,
    /// The in-flight PR waits for review.
    ReviewReminder,
    /// The in-flight task was released.
    TaskReleased,
    /// Nightly update changed the Claude Code version.
    ImageUpdated,
    /// Nightly update failed.
    ImageUpdateFailed,
    /// Daily summary.
    DailySummary,
}

impl EventKind {
    /// All event kinds.
    pub const ALL: [EventKind; 12] = [
        EventKind::PrOpened,
        EventKind::Blocked,
        EventKind::Failed,
        EventKind::Paused,
        EventKind::Resumed,
        EventKind::TokenExpiry,
        EventKind::NetworkRestored,
        EventKind::ReviewReminder,
        EventKind::TaskReleased,
        EventKind::ImageUpdated,
        EventKind::ImageUpdateFailed,
        EventKind::DailySummary,
    ];

    /// Config name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrOpened => "pr_opened",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Paused => "paused",
            Self::Resumed => "resumed",
            Self::TokenExpiry => "token_expiry",
            Self::NetworkRestored => "network_restored",
            Self::ReviewReminder => "review_reminder",
            Self::TaskReleased => "task_released",
            Self::ImageUpdated => "image_updated",
            Self::ImageUpdateFailed => "image_update_failed",
            Self::DailySummary => "daily_summary",
        }
    }

    /// Parses a config name.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s.trim())
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Run statistics from `result.json`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunStats {
    /// Turns.
    pub turns: Option<u32>,
    /// Cost in USD.
    pub cost_usd: Option<f64>,
    /// Wall time.
    pub duration: Option<Duration>,
}

/// One recent failure, for `paused`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFailure {
    /// The item.
    pub item: ItemRef,
    /// Its class.
    pub class: FailureClass,
}

/// Why the factory paused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseCause {
    /// The breaker tripped after counted failures.
    Breaker,
    /// A fatal or usage-limit class.
    Class(FailureClass),
    /// More than one task in flight.
    Conflict,
    /// Paused by the owner.
    Manual,
}

/// A notification with its data.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// PR created.
    PrOpened {
        /// The issue.
        issue: ItemRef,
        /// Issue title.
        issue_title: String,
        /// The new PR number.
        pr: u64,
        /// Run stats.
        stats: RunStats,
    },
    /// The agent asked a question.
    Blocked {
        /// Issue or PR.
        item: ItemRef,
        /// The question (first 300 chars are shown).
        question: String,
    },
    /// `agent_failed` or `factory_bug`.
    Failed {
        /// Issue or PR.
        item: ItemRef,
        /// Class.
        class: FailureClass,
        /// Short reason.
        reason: String,
        /// Local log path.
        log_path: String,
    },
    /// The factory paused.
    Paused {
        /// Cause.
        cause: PauseCause,
        /// Reason text.
        reason: String,
        /// The last failures with class.
        recent: Vec<RecentFailure>,
        /// For a usage-limit pause: when it ends.
        until: Option<DateTime<Utc>>,
        /// For `infra`: the failing check.
        failing_check: Option<String>,
    },
    /// The factory resumed.
    Resumed {
        /// Who resumed it (`cli`, `telegram`, `usage limit reset`).
        by: String,
        /// Queue size.
        queue_size: usize,
    },
    /// Claude token expiry reminder.
    TokenExpiry {
        /// Days left.
        days_left: i64,
    },
    /// Connectivity is back.
    NetworkRestored {
        /// Outage start.
        start: DateTime<Utc>,
        /// Outage end.
        end: DateTime<Utc>,
    },
    /// The in-flight PR waits for review.
    ReviewReminder {
        /// The PR.
        pr: ItemRef,
        /// How long it has waited.
        waited: Duration,
        /// Issues queued behind it.
        queue_size: usize,
    },
    /// The in-flight task was released.
    TaskReleased {
        /// The task's issue.
        issue: ItemRef,
        /// What happened.
        how: Released,
        /// Next issue in the queue, with title.
        next: Option<(ItemRef, String)>,
    },
    /// Image updated.
    ImageUpdated {
        /// Old Claude Code version.
        old_version: String,
        /// New Claude Code version.
        new_version: String,
        /// Channel.
        channel: String,
    },
    /// Image update failed.
    ImageUpdateFailed {
        /// Failing step.
        step: String,
        /// Short reason.
        reason: String,
    },
    /// Daily summary.
    DailySummary(DailySummary),
}

/// How a task was released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// Its PR was merged.
    Merged,
    /// Its PR was closed without merge, or its issue was closed.
    Other(ReleaseReason),
}

/// Data for the daily summary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DailySummary {
    /// The in-flight task and its state, as text.
    pub in_flight: Option<(ItemRef, String)>,
    /// PRs opened today.
    pub prs_opened: u32,
    /// Items blocked today.
    pub blocked: u32,
    /// Jobs failed today.
    pub failed: u32,
    /// Queue size.
    pub queue_size: usize,
    /// Total cost today in USD.
    pub cost_usd: f64,
    /// Ignored `agent-revise` labels.
    pub ignored_revise: Vec<ItemRef>,
}

impl Event {
    /// The event's kind, for filtering with `telegram.notify`.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::PrOpened { .. } => EventKind::PrOpened,
            Self::Blocked { .. } => EventKind::Blocked,
            Self::Failed { .. } => EventKind::Failed,
            Self::Paused { .. } => EventKind::Paused,
            Self::Resumed { .. } => EventKind::Resumed,
            Self::TokenExpiry { .. } => EventKind::TokenExpiry,
            Self::NetworkRestored { .. } => EventKind::NetworkRestored,
            Self::ReviewReminder { .. } => EventKind::ReviewReminder,
            Self::TaskReleased { .. } => EventKind::TaskReleased,
            Self::ImageUpdated { .. } => EventKind::ImageUpdated,
            Self::ImageUpdateFailed { .. } => EventKind::ImageUpdateFailed,
            Self::DailySummary(_) => EventKind::DailySummary,
        }
    }
}

/// Escapes text for Telegram HTML (`&`, `<`, `>`, `"`).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Cuts `s` to at most `max` characters, adding `…` when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Formats a duration like `1h 05m`, `2d 3h`, `45s`.
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (days, hours, mins, secs) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60, s % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else if mins > 0 {
        format!("{mins}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

/// Builds Telegram messages.
#[derive(Debug, Clone)]
pub struct Formatter {
    redactor: Redactor,
    tz: Tz,
}

impl Formatter {
    /// A formatter that redacts with `redactor` and shows times in `tz`.
    pub fn new(redactor: Redactor, tz: Tz) -> Self {
        Self { redactor, tz }
    }

    /// Redact, cut, escape: for every dynamic value.
    fn text(&self, s: &str, max: usize) -> String {
        escape_html(&truncate_chars(&self.redactor.redact(s), max))
    }

    fn link(&self, url: &str, label: &str) -> String {
        format!(
            "<a href=\"{}\">{}</a>",
            escape_html(&self.redactor.redact(url)),
            self.text(label, TITLE_CHARS)
        )
    }

    fn item_link(&self, item: &ItemRef) -> String {
        let label = match item.kind {
            ItemKind::Issue => format!("{}#{}", item.repo, item.number),
            ItemKind::Pr => format!("{} PR #{}", item.repo, item.number),
        };
        self.link(&item.url(), &label)
    }

    fn time(&self, t: DateTime<Utc>) -> String {
        t.with_timezone(&self.tz)
            .format("%Y-%m-%d %H:%M %Z")
            .to_string()
    }

    fn list(&self, items: &[String]) -> String {
        let mut lines: Vec<String> = items
            .iter()
            .take(LIST_ITEMS)
            .map(|i| format!("• {i}"))
            .collect();
        if items.len() > LIST_ITEMS {
            lines.push(format!("• … and {} more", items.len() - LIST_ITEMS));
        }
        lines.join("\n")
    }

    /// Formats an event as Telegram HTML.
    pub fn format(&self, event: &Event) -> String {
        let body = match event {
            Event::PrOpened {
                issue,
                issue_title,
                pr,
                stats,
            } => {
                let pr_ref = ItemRef::pr(issue.repo.clone(), *pr);
                let mut parts = Vec::new();
                if let Some(t) = stats.turns {
                    parts.push(format!("{t} turns"));
                }
                if let Some(c) = stats.cost_usd.filter(|c| c.is_finite()) {
                    parts.push(format!("${c:.2}"));
                }
                if let Some(d) = stats.duration {
                    parts.push(format_duration(d));
                }
                let stats = if parts.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", parts.join(" · "))
                };
                format!(
                    "✅ <b>PR opened</b> in {}\n{}: {}\n{}{stats}",
                    self.text(&issue.repo.to_string(), TITLE_CHARS),
                    self.item_link(issue),
                    self.text(issue_title, TITLE_CHARS),
                    self.item_link(&pr_ref),
                )
            }
            Event::Blocked { item, question } => {
                let q = if question.trim().is_empty() {
                    "(no question text)".to_string()
                } else {
                    self.text(question.trim(), QUESTION_PREVIEW_CHARS)
                };
                format!(
                    "❓ <b>Blocked</b>: {}\n\n{q}\n\nAnswer on GitHub, then add <code>{}</code>.",
                    self.item_link(item),
                    match item.kind {
                        ItemKind::Issue => "agent-ready",
                        ItemKind::Pr => "agent-revise",
                    }
                )
            }
            Event::Failed {
                item,
                class,
                reason,
                log_path,
            } => format!(
                "❌ <b>Failed</b>: {} ({})\n{}\nLog: <code>{}</code>",
                self.item_link(item),
                class.as_str(),
                self.text(reason, REASON_CHARS),
                self.text(log_path, REASON_CHARS),
            ),
            Event::Paused {
                cause,
                reason,
                recent,
                until,
                failing_check,
            } => {
                let mut out = format!("⏸ <b>Paused</b>: {}", self.text(reason, REASON_CHARS));
                if let Some(t) = until {
                    out.push_str(&format!(
                        "\nUntil {}, then resumes by itself.",
                        self.time(*t)
                    ));
                }
                if !recent.is_empty() {
                    let items: Vec<String> = recent
                        .iter()
                        .map(|f| format!("{} ({})", self.item_link(&f.item), f.class.as_str()))
                        .collect();
                    out.push_str(&format!("\n\nRecent failures:\n{}", self.list(&items)));
                }
                let todo = match cause {
                    PauseCause::Class(FailureClass::ClaudeAuth) => Some(
                        "Run <code>claude setup-token</code> on your laptop, put the token in \
                         claude.env, then /resume."
                            .to_string(),
                    ),
                    PauseCause::Class(FailureClass::GithubAuth) => Some(
                        "Check that the GitHub App is still installed on the repo, its \
                         permissions, and that the private key on disk matches one of the \
                         app's keys, then /resume."
                            .to_string(),
                    ),
                    PauseCause::Class(FailureClass::Infra) => Some(format!(
                        "Failing check: {}. Fix it, then /resume.",
                        failing_check.as_deref().map_or_else(
                            || "see the log".to_string(),
                            |c| self.text(c, REASON_CHARS)
                        )
                    )),
                    PauseCause::Class(FailureClass::ClaudeUsageLimit) => None,
                    PauseCause::Conflict => Some(
                        "More than one task is in flight. Fix the labels so only one remains, \
                         then /resume."
                            .to_string(),
                    ),
                    PauseCause::Breaker | PauseCause::Class(_) | PauseCause::Manual => {
                        Some("Check the failures, then /resume.".to_string())
                    }
                };
                if let Some(todo) = todo {
                    out.push_str(&format!("\n\n{todo}"));
                }
                out
            }
            Event::Resumed { by, queue_size } => format!(
                "▶️ <b>Resumed</b> by {}. Queue: {queue_size}.",
                self.text(by, TITLE_CHARS)
            ),
            Event::TokenExpiry { days_left } => format!(
                "🔑 <b>Claude token expires in {days_left} day{}</b>.\nRun \
                 <code>claude setup-token</code> on your laptop, put the new token in \
                 claude.env, and update <code>github.claude_token_created</code>.",
                if *days_left == 1 { "" } else { "s" }
            ),
            Event::NetworkRestored { start, end } => {
                let dur = (*end - *start).to_std().unwrap_or_default();
                format!(
                    "🌐 <b>Network restored</b>\nOutage from {} to {} ({}).",
                    self.time(*start),
                    self.time(*end),
                    format_duration(dur)
                )
            }
            Event::ReviewReminder {
                pr,
                waited,
                queue_size,
            } => format!(
                "👀 <b>Review reminder</b>: {} has waited {}.\n{queue_size} issue(s) queued behind it.",
                self.item_link(pr),
                format_duration(*waited)
            ),
            Event::TaskReleased { issue, how, next } => {
                let what = match how {
                    Released::Merged => "PR merged",
                    Released::Other(ReleaseReason::PrClosedUnmerged) => "PR closed without merge",
                    Released::Other(ReleaseReason::IssueClosed) => "issue closed",
                };
                let next = match next {
                    Some((item, title)) => format!(
                        "Next: {} {}",
                        self.item_link(item),
                        self.text(title, TITLE_CHARS)
                    ),
                    None => "Queue is empty.".to_string(),
                };
                format!(
                    "🏁 <b>Task released</b>: {} ({what}).\n{next}",
                    self.item_link(issue)
                )
            }
            Event::ImageUpdated {
                old_version,
                new_version,
                channel,
            } => format!(
                "📦 <b>Agent image updated</b>: Claude Code {} → {} ({}).",
                self.text(old_version, TITLE_CHARS),
                self.text(new_version, TITLE_CHARS),
                self.text(channel, TITLE_CHARS)
            ),
            Event::ImageUpdateFailed { step, reason } => format!(
                "⚠️ <b>Agent image update failed</b> at {}: {}\nThe current image is kept.",
                self.text(step, TITLE_CHARS),
                self.text(reason, REASON_CHARS)
            ),
            Event::DailySummary(s) => {
                let in_flight = match &s.in_flight {
                    Some((item, state)) => {
                        format!("{} {}", self.item_link(item), self.text(state, TITLE_CHARS))
                    }
                    None => "none".to_string(),
                };
                let mut out = format!(
                    "📋 <b>Daily summary</b>\nIn flight: {in_flight}\nPRs opened: {}\nBlocked: {}\n\
                     Failed: {}\nQueue: {}\nCost today: ${:.2}",
                    s.prs_opened,
                    s.blocked,
                    s.failed,
                    s.queue_size,
                    if s.cost_usd.is_finite() {
                        s.cost_usd
                    } else {
                        0.0
                    }
                );
                if !s.ignored_revise.is_empty() {
                    let items: Vec<String> =
                        s.ignored_revise.iter().map(|i| self.item_link(i)).collect();
                    out.push_str(&format!(
                        "\n\nIgnored <code>agent-revise</code> labels:\n{}",
                        self.list(&items)
                    ));
                }
                out
            }
        };
        fit(&self.redactor.redact(&body), TELEGRAM_MAX_CHARS)
    }

    /// Formats a short plain test message for `notify-test`.
    pub fn test_message(&self, host: &str) -> String {
        format!(
            "🔔 singlelane test message from {}",
            self.text(host, TITLE_CHARS)
        )
    }
}

/// Keeps a formatted HTML message within `max` characters by dropping whole lines from the
/// end (tags never span lines here), then, if a single line is still too long, cutting it
/// outside any tag or entity and closing nothing (only lines without open tags are cut).
pub fn fit(html: &str, max: usize) -> String {
    if html.chars().count() <= max {
        return html.to_string();
    }
    const MORE: &str = "\n…";
    let budget = max.saturating_sub(MORE.chars().count());
    let mut out = String::new();
    let mut used = 0;
    for line in html.split('\n') {
        let n = line.chars().count() + usize::from(!out.is_empty());
        if used + n > budget {
            if out.is_empty() {
                // One huge first line: strip tags and cut plain text safely.
                return format!("{}{MORE}", cut_plain(&strip_tags(line), budget));
            }
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        used += n;
    }
    out.push_str(MORE);
    out
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Cuts escaped text to `max` chars without splitting an `&...;` entity.
fn cut_plain(escaped: &str, max: usize) -> String {
    let mut out: String = escaped.chars().take(max).collect();
    if let Some(amp) = out.rfind('&')
        && !out[amp..].contains(';')
    {
        out.truncate(amp);
    }
    out
}

/// Telegram credentials from the environment: both must be set, else Telegram is off.
pub fn telegram_credentials<'a>(
    token: Option<&'a str>,
    chat_id: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    match (
        token.map(str::trim).filter(|t| !t.is_empty()),
        chat_id.map(str::trim).filter(|c| !c.is_empty()),
    ) {
        (Some(t), Some(c)) => Some((t, c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::RepoName;

    fn repo() -> RepoName {
        RepoName::parse("owner/app").unwrap()
    }

    fn fmt() -> Formatter {
        Formatter::new(
            Redactor::with_secrets(["known-secret-value-123"]),
            chrono_tz::Europe::Kyiv,
        )
    }

    fn fake_token() -> String {
        format!("ghs_{}", "a1B2c3D4e5F6g7H8i9J0".repeat(2))
    }

    fn all_events() -> Vec<Event> {
        let issue = ItemRef::issue(repo(), 5);
        let pr = ItemRef::pr(repo(), 9);
        let t0 = DateTime::parse_from_rfc3339("2026-11-24T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        vec![
            Event::PrOpened {
                issue: issue.clone(),
                issue_title: "Add <b>CSV</b> export".into(),
                pr: 9,
                stats: RunStats {
                    turns: Some(42),
                    cost_usd: Some(1.234),
                    duration: Some(Duration::from_secs(3720)),
                },
            },
            Event::Blocked {
                item: issue.clone(),
                question: "Which format?".into(),
            },
            Event::Failed {
                item: pr.clone(),
                class: FailureClass::AgentFailed,
                reason: "no commits".into(),
                log_path: "/var/lib/singlelane/logs/x.log".into(),
            },
            Event::Paused {
                cause: PauseCause::Class(FailureClass::ClaudeAuth),
                reason: "Claude auth failed".into(),
                recent: vec![RecentFailure {
                    item: issue.clone(),
                    class: FailureClass::ClaudeAuth,
                }],
                until: None,
                failing_check: None,
            },
            Event::Resumed {
                by: "telegram".into(),
                queue_size: 3,
            },
            Event::TokenExpiry { days_left: 7 },
            Event::NetworkRestored {
                start: t0,
                end: t0 + chrono::Duration::hours(3),
            },
            Event::ReviewReminder {
                pr: pr.clone(),
                waited: Duration::from_secs(2 * 86_400 + 3600),
                queue_size: 2,
            },
            Event::TaskReleased {
                issue: issue.clone(),
                how: Released::Merged,
                next: Some((ItemRef::issue(repo(), 6), "Next thing".into())),
            },
            Event::ImageUpdated {
                old_version: "2.1.0".into(),
                new_version: "2.2.0".into(),
                channel: "stable".into(),
            },
            Event::ImageUpdateFailed {
                step: "smoke test".into(),
                reason: "chromium did not start".into(),
            },
            Event::DailySummary(DailySummary {
                in_flight: Some((issue, "in review".into())),
                prs_opened: 1,
                blocked: 0,
                failed: 1,
                queue_size: 4,
                cost_usd: 3.5,
                ignored_revise: vec![ItemRef::pr(repo(), 11)],
            }),
        ]
    }

    #[test]
    fn every_event_kind_formats_and_has_a_link_or_text() {
        let kinds: Vec<EventKind> = all_events().iter().map(Event::kind).collect();
        assert_eq!(kinds, EventKind::ALL.to_vec());
        for e in all_events() {
            let m = fmt().format(&e);
            assert!(!m.is_empty());
            assert!(m.chars().count() <= TELEGRAM_MAX_CHARS);
        }
    }

    #[test]
    fn event_kind_names_round_trip() {
        for k in EventKind::ALL {
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EventKind::parse("nope"), None);
    }

    #[test]
    fn pr_opened_content() {
        let m = fmt().format(&all_events()[0]);
        assert!(m.contains(r#"<a href="https://github.com/owner/app/issues/5">owner/app#5</a>"#));
        assert!(m.contains(r#"<a href="https://github.com/owner/app/pull/9">owner/app PR #9</a>"#));
        assert!(m.contains("Add &lt;b&gt;CSV&lt;/b&gt; export"));
        assert!(m.contains("42 turns · $1.23 · 1h 02m"));
    }

    #[test]
    fn dynamic_content_is_escaped() {
        let m = fmt().format(&Event::Blocked {
            item: ItemRef::issue(repo(), 1),
            question: r#"<script>alert("x")</script> & more"#.into(),
        });
        assert!(m.contains("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; more"));
        assert!(!m.contains("<script>"));
    }

    #[test]
    fn blocked_question_is_cut_to_300_chars() {
        let q = "x".repeat(1000);
        let m = fmt().format(&Event::Blocked {
            item: ItemRef::issue(repo(), 1),
            question: q,
        });
        let longest_run = m
            .split(|c| c != 'x')
            .map(str::len)
            .max()
            .unwrap_or_default();
        assert_eq!(longest_run, QUESTION_PREVIEW_CHARS - 1);
        assert!(m.contains('…'));
    }

    #[test]
    fn g8_telegram_messages_are_redacted() {
        let tok = fake_token();
        let events = vec![
            Event::Blocked {
                item: ItemRef::issue(repo(), 1),
                question: format!("I found {tok} in the logs, and known-secret-value-123"),
            },
            Event::Failed {
                item: ItemRef::issue(repo(), 1),
                class: FailureClass::FactoryBug,
                reason: format!("git push https://x-access-token:{tok}@github.com/o/r failed"),
                log_path: "/var/lib/singlelane/logs/a.log".into(),
            },
            Event::Paused {
                cause: PauseCause::Class(FailureClass::Infra),
                reason: "infra".into(),
                recent: vec![],
                until: None,
                failing_check: Some("ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnop".into()),
            },
            Event::ImageUpdateFailed {
                step: "build".into(),
                reason: "-----BEGIN PRIVATE KEY-----\nMIIEabc\n-----END PRIVATE KEY-----".into(),
            },
        ];
        for e in events {
            let m = fmt().format(&e);
            assert!(!m.contains(&tok), "{m}");
            assert!(!m.contains("known-secret-value-123"), "{m}");
            assert!(!m.contains("sk-ant-api03"), "{m}");
            assert!(!m.contains("MIIEabc"), "{m}");
            assert!(m.contains("[REDACTED]"), "{m}");
        }
    }

    #[test]
    fn g8_secret_split_by_truncation_is_still_redacted() {
        // Redaction runs before cutting, so a token at the cut point can't leak half-way.
        let tok = fake_token();
        let q = format!("{}{tok}", "y".repeat(QUESTION_PREVIEW_CHARS - 10));
        let m = fmt().format(&Event::Blocked {
            item: ItemRef::issue(repo(), 1),
            question: q,
        });
        assert!(!m.contains("ghs_a1B2"));
    }

    #[test]
    fn paused_messages_carry_instructions() {
        let make = |class| {
            fmt().format(&Event::Paused {
                cause: PauseCause::Class(class),
                reason: "r".into(),
                recent: vec![],
                until: None,
                failing_check: Some("docker daemon".into()),
            })
        };
        assert!(make(FailureClass::ClaudeAuth).contains("claude setup-token"));
        assert!(make(FailureClass::GithubAuth).contains("private key"));
        assert!(make(FailureClass::Infra).contains("docker daemon"));
    }

    #[test]
    fn usage_limit_pause_shows_local_until_time() {
        let until = DateTime::parse_from_rfc3339("2026-11-24T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let m = fmt().format(&Event::Paused {
            cause: PauseCause::Class(FailureClass::ClaudeUsageLimit),
            reason: "usage limit".into(),
            recent: vec![],
            until: Some(until),
            failing_check: None,
        });
        assert!(m.contains("2026-11-24 12:00 EET"), "{m}");
    }

    #[test]
    fn long_messages_fit_telegram_limit() {
        let many: Vec<ItemRef> = (1..500).map(|n| ItemRef::pr(repo(), n)).collect();
        let m = fmt().format(&Event::DailySummary(DailySummary {
            ignored_revise: many,
            ..DailySummary::default()
        }));
        assert!(m.chars().count() <= TELEGRAM_MAX_CHARS);
        assert!(m.contains("and 489 more"));

        let huge = format!("<b>{}</b> &amp; {}", "z".repeat(5000), "w".repeat(10));
        let cut = fit(&huge, 100);
        assert!(cut.chars().count() <= 100);
        assert!(!cut.contains('<'));

        let lines = (0..1000)
            .map(|i| format!("<i>line {i}</i>"))
            .collect::<Vec<_>>()
            .join("\n");
        let cut = fit(&lines, 200);
        assert!(cut.chars().count() <= 200);
        assert!(
            cut.lines()
                .all(|l| l == "…" || (l.starts_with("<i>") && l.ends_with("</i>")))
        );
    }

    #[test]
    fn cut_plain_never_splits_entities() {
        assert_eq!(cut_plain("ab &amp; cd", 5), "ab ");
        assert_eq!(cut_plain("ab &amp; cd", 8), "ab &amp;");
    }

    #[test]
    fn telegram_is_off_unless_both_values_are_set() {
        assert!(telegram_credentials(Some("t"), Some("1")).is_some());
        assert!(telegram_credentials(Some("t"), None).is_none());
        assert!(telegram_credentials(Some(""), Some("1")).is_none());
        assert!(telegram_credentials(None, Some("1")).is_none());
    }

    #[test]
    fn durations_format() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h 02m");
        assert_eq!(
            format_duration(Duration::from_secs(2 * 86_400 + 3600)),
            "2d 1h"
        );
    }
}

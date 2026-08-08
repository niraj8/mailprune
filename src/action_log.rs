//! Append-only JSONL log of triage decisions — ground truth for evaluating
//! any future suggestion policy (heuristics, per-sender recall, ML).
//!
//! One line per (stack, decision). Actions are logged as they execute;
//! stacks the user saw on screen but left alone are logged as `keep` at
//! session end. Logging is best-effort: it must never break triage, so
//! write failures are swallowed.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

use crate::stacks::Stack;

const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Trash,
    Archive,
    Read,
    Unsub,
    /// seen on screen during the session, never acted on
    Keep,
}

/// Stack features frozen at the moment the user saw or acted on it — the
/// inbox mutates, so they cannot be reconstructed from the log later.
#[derive(Debug, Clone, Serialize)]
pub struct StackSnapshot {
    /// lowercased sender address
    pub sender: String,
    /// normalized subject, present only when grouped by sender+subject
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub group_by: &'static str,
    pub count: usize,
    pub read_rate: u8,
    pub has_unsub: bool,
    pub one_click: bool,
    /// days between oldest and newest message in the stack
    pub span_days: i64,
    /// days since the newest message
    pub latest_age_days: i64,
}

impl StackSnapshot {
    pub fn of(stack: &Stack) -> Self {
        // the grouping key is "sender" or "sender\0normalized-subject"
        let (sender, subject) = match stack.key.split_once('\u{0}') {
            Some((s, subj)) => (s.to_string(), Some(subj.to_string())),
            None => (stack.key.clone(), None),
        };
        let newest = stack.msgs.first().and_then(|m| m.date);
        let oldest = stack.msgs.last().and_then(|m| m.date);
        Self {
            group_by: if subject.is_some() {
                "sender+subject"
            } else {
                "sender"
            },
            sender,
            subject,
            count: stack.msgs.len(),
            read_rate: stack.read_rate(),
            has_unsub: stack.can_unsubscribe,
            one_click: stack.msgs.iter().any(|m| m.one_click),
            span_days: match (newest, oldest) {
                (Some(n), Some(o)) => (n - o).num_days(),
                _ => 0,
            },
            latest_age_days: newest.map(|n| (Utc::now() - n).num_days()).unwrap_or(0),
        }
    }
}

#[derive(Debug, Serialize)]
struct Record<'a> {
    v: u8,
    ts: DateTime<Utc>,
    account: &'a str,
    action: Action,
    /// whether the tool suggested this action (no suggestion engine yet;
    /// recorded from day one so suggestion-biased labels are detectable)
    suggested: bool,
    #[serde(flatten)]
    stack: &'a StackSnapshot,
}

pub struct ActionLog {
    /// None = logging disabled
    path: Option<PathBuf>,
}

impl ActionLog {
    pub fn new(enabled: bool) -> Self {
        Self {
            path: enabled.then(default_path),
        }
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn log(&self, account: &str, action: Action, stacks: &[StackSnapshot]) {
        let Some(path) = &self.path else { return };
        if !stacks.is_empty() {
            let _ = append(path, account, action, stacks);
        }
    }
}

/// $XDG_STATE_HOME/mailprune, default ~/.local/state/mailprune
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("mailprune")
}

pub fn default_path() -> PathBuf {
    state_dir().join("actions.jsonl")
}

fn append(path: &PathBuf, account: &str, action: Action, stacks: &[StackSnapshot]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let ts = Utc::now();
    let mut buf = String::new();
    for stack in stacks {
        buf.push_str(&serde_json::to_string(&Record {
            v: SCHEMA_VERSION,
            ts,
            account,
            action,
            suggested: false,
            stack,
        })?);
        buf.push('\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(buf.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stacks::{GroupBy, MsgMeta, SortBy, build_stacks};
    use chrono::TimeZone;

    fn msg(subject: &str, days_ago: i64, unread: bool) -> MsgMeta {
        MsgMeta {
            uid: 1,
            sender_email: "News@Example.com".into(),
            sender_name: "Example".into(),
            subject: subject.into(),
            date: Some(
                Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
                    - chrono::Duration::days(days_ago),
            ),
            unread,
            has_attachment: false,
            list_unsubscribe: Some("<https://example.com/u>".into()),
            one_click: true,
        }
    }

    #[test]
    fn snapshot_splits_sender_subject_key() {
        let stacks = build_stacks(
            vec![
                msg("Order #12 shipped", 0, true),
                msg("Order #99 shipped", 10, false),
            ],
            GroupBy::SenderSubject,
            SortBy::Count,
        );
        let snap = StackSnapshot::of(&stacks[0]);
        assert_eq!(snap.sender, "news@example.com");
        assert_eq!(snap.subject.as_deref(), Some("order ## shipped"));
        assert_eq!(snap.group_by, "sender+subject");
        assert_eq!(snap.count, 2);
        assert_eq!(snap.read_rate, 50);
        assert!(snap.has_unsub);
        assert!(snap.one_click);
        assert_eq!(snap.span_days, 10);
    }

    #[test]
    fn snapshot_of_sender_grouped_stack_has_no_subject() {
        let stacks = build_stacks(
            vec![msg("a", 0, true), msg("b", 1, true)],
            GroupBy::Sender,
            SortBy::Count,
        );
        let snap = StackSnapshot::of(&stacks[0]);
        assert_eq!(snap.sender, "news@example.com");
        assert_eq!(snap.subject, None);
        assert_eq!(snap.group_by, "sender");
    }

    #[test]
    fn append_writes_one_json_line_per_stack() {
        let path = std::env::temp_dir().join(format!(
            "mailprune-test-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let log = ActionLog::at(path.clone());
        let stacks = build_stacks(vec![msg("hi", 0, true)], GroupBy::Sender, SortBy::Count);
        let snap = StackSnapshot::of(&stacks[0]);
        log.log("me@x.com", Action::Trash, &[snap.clone()]);
        log.log("me@x.com", Action::Keep, &[snap]);

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(rec["v"], 1);
        assert_eq!(rec["account"], "me@x.com");
        assert_eq!(rec["action"], "trash");
        assert_eq!(rec["suggested"], false);
        assert_eq!(rec["sender"], "news@example.com");
        assert!(rec.get("subject").is_none());
        let rec: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(rec["action"], "keep");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn disabled_log_writes_nothing() {
        let log = ActionLog::new(false);
        let stacks = build_stacks(vec![msg("hi", 0, true)], GroupBy::Sender, SortBy::Count);
        // must be a no-op, not a panic or a file in the state dir
        log.log("me@x.com", Action::Trash, &[StackSnapshot::of(&stacks[0])]);
    }
}

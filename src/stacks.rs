use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct MsgMeta {
    pub uid: u32,
    pub sender_email: String,
    pub sender_name: String,
    pub subject: String,
    pub date: Option<DateTime<Utc>>,
    pub unread: bool,
    pub list_unsubscribe: Option<String>,
    pub one_click: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Sender,
    SenderSubject,
}

impl GroupBy {
    pub fn toggle(self) -> Self {
        match self {
            GroupBy::Sender => GroupBy::SenderSubject,
            GroupBy::SenderSubject => GroupBy::Sender,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GroupBy::Sender => "sender",
            GroupBy::SenderSubject => "sender+subject",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Count,
    ReadRate,
}

impl SortBy {
    pub fn toggle(self) -> Self {
        match self {
            SortBy::Count => SortBy::ReadRate,
            SortBy::ReadRate => SortBy::Count,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortBy::Count => "count",
            SortBy::ReadRate => "read rate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Stack {
    /// lowercased sender address (+ normalized subject) — the grouping key
    pub key: String,
    pub display_name: String,
    /// representative subject, set when grouping by sender+subject
    pub subject: Option<String>,
    pub msgs: Vec<MsgMeta>,
    pub unread_count: usize,
    pub can_unsubscribe: bool,
}

impl Stack {
    pub fn latest(&self) -> &MsgMeta {
        &self.msgs[0]
    }

    /// percentage of messages in this stack that were opened (0–100)
    pub fn read_rate(&self) -> u8 {
        if self.msgs.is_empty() {
            return 100;
        }
        let read = self.msgs.len() - self.unread_count;
        (read * 100 / self.msgs.len()) as u8
    }

    /// most recent message carrying a List-Unsubscribe header
    pub fn unsubscribe_source(&self) -> Option<&MsgMeta> {
        self.msgs.iter().find(|m| m.list_unsubscribe.is_some())
    }

    pub fn uids(&self) -> Vec<u32> {
        self.msgs.iter().map(|m| m.uid).collect()
    }
}

pub fn build_stacks(mut msgs: Vec<MsgMeta>, group_by: GroupBy, sort_by: SortBy) -> Vec<Stack> {
    msgs.sort_by(|a, b| b.date.cmp(&a.date));
    let mut stacks: Vec<Stack> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for msg in msgs {
        let key = match group_by {
            GroupBy::Sender => msg.sender_email.to_lowercase(),
            GroupBy::SenderSubject => format!(
                "{}\u{0}{}",
                msg.sender_email.to_lowercase(),
                normalize_subject(&msg.subject)
            ),
        };
        match index.get(&key) {
            Some(&i) => stacks[i].msgs.push(msg),
            None => {
                index.insert(key.clone(), stacks.len());
                let display_name = if msg.sender_name.is_empty() {
                    msg.sender_email.clone()
                } else {
                    msg.sender_name.clone()
                };
                let subject = match group_by {
                    GroupBy::Sender => None,
                    GroupBy::SenderSubject => Some(msg.subject.clone()),
                };
                stacks.push(Stack {
                    key,
                    display_name,
                    subject,
                    msgs: vec![msg],
                    unread_count: 0,
                    can_unsubscribe: false,
                });
            }
        }
    }
    for stack in &mut stacks {
        stack.unread_count = stack.msgs.iter().filter(|m| m.unread).count();
        stack.can_unsubscribe = stack.msgs.iter().any(|m| m.list_unsubscribe.is_some());
    }
    sort_stacks(&mut stacks, sort_by);
    stacks
}

pub fn sort_stacks(stacks: &mut [Stack], sort_by: SortBy) {
    match sort_by {
        SortBy::Count => stacks.sort_by(|a, b| {
            b.msgs
                .len()
                .cmp(&a.msgs.len())
                .then(b.msgs[0].date.cmp(&a.msgs[0].date))
        }),
        // least-read first; ties broken by size so big never-read stacks
        // (prime unsubscribe candidates) float to the top
        SortBy::ReadRate => stacks.sort_by(|a, b| {
            a.read_rate()
                .cmp(&b.read_rate())
                .then(b.msgs.len().cmp(&a.msgs.len()))
        }),
    }
}

/// Grouping key for subjects: strip Re:/Fwd: prefixes, lowercase, and collapse
/// digit runs so "Order #123" and "Order #456" land in the same stack.
fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim().to_lowercase();
    loop {
        let t = s.trim_start();
        let stripped = t
            .strip_prefix("re:")
            .or_else(|| t.strip_prefix("fwd:"))
            .or_else(|| t.strip_prefix("fw:"));
        match stripped {
            Some(rest) => s = rest.trim_start().to_string(),
            None => break,
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut in_digits = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(sender: &str, unread: bool) -> MsgMeta {
        MsgMeta {
            uid: 1,
            sender_email: sender.into(),
            sender_name: String::new(),
            subject: "hi".into(),
            date: None,
            unread,
            list_unsubscribe: None,
            one_click: false,
        }
    }

    #[test]
    fn read_rate_sort_puts_unread_stacks_first() {
        let msgs = vec![
            // a@: 3 msgs, all read; b@: 2 msgs, never read
            msg("a@x.com", false),
            msg("a@x.com", false),
            msg("a@x.com", false),
            msg("b@x.com", true),
            msg("b@x.com", true),
        ];
        let count = build_stacks(msgs.clone(), GroupBy::Sender, SortBy::Count);
        assert_eq!(count[0].key, "a@x.com");
        let rate = build_stacks(msgs, GroupBy::Sender, SortBy::ReadRate);
        assert_eq!(rate[0].key, "b@x.com");
        assert_eq!(rate[0].read_rate(), 0);
        assert_eq!(rate[1].read_rate(), 100);
    }

    #[test]
    fn subject_normalization() {
        assert_eq!(normalize_subject("Re: Re: Hello"), "hello");
        assert_eq!(normalize_subject("Fwd: order #123 shipped"), "order ## shipped");
        assert_eq!(
            normalize_subject("Order #456 shipped"),
            normalize_subject("ORDER #99 shipped")
        );
    }
}

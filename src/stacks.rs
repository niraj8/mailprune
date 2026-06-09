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

#[derive(Debug, Clone)]
pub struct Stack {
    /// lowercased sender address — the grouping key
    pub key: String,
    pub display_name: String,
    pub msgs: Vec<MsgMeta>,
    pub unread_count: usize,
    pub can_unsubscribe: bool,
}

impl Stack {
    pub fn latest(&self) -> &MsgMeta {
        &self.msgs[0]
    }

    /// most recent message carrying a List-Unsubscribe header
    pub fn unsubscribe_source(&self) -> Option<&MsgMeta> {
        self.msgs.iter().find(|m| m.list_unsubscribe.is_some())
    }

    pub fn uids(&self) -> Vec<u32> {
        self.msgs.iter().map(|m| m.uid).collect()
    }
}

pub fn build_stacks(mut msgs: Vec<MsgMeta>) -> Vec<Stack> {
    msgs.sort_by(|a, b| b.date.cmp(&a.date));
    let mut by_sender: Vec<Stack> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for msg in msgs {
        let key = msg.sender_email.to_lowercase();
        match index.get(&key) {
            Some(&i) => by_sender[i].msgs.push(msg),
            None => {
                index.insert(key.clone(), by_sender.len());
                let display_name = if msg.sender_name.is_empty() {
                    msg.sender_email.clone()
                } else {
                    msg.sender_name.clone()
                };
                by_sender.push(Stack {
                    key,
                    display_name,
                    msgs: vec![msg],
                    unread_count: 0,
                    can_unsubscribe: false,
                });
            }
        }
    }
    for stack in &mut by_sender {
        stack.unread_count = stack.msgs.iter().filter(|m| m.unread).count();
        stack.can_unsubscribe = stack.msgs.iter().any(|m| m.list_unsubscribe.is_some());
    }
    by_sender.sort_by(|a, b| b.msgs.len().cmp(&a.msgs.len()).then(b.msgs[0].date.cmp(&a.msgs[0].date)));
    by_sender
}

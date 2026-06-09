use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::config::AccountConfig;
use crate::imap_client::ImapClient;
use crate::stacks::{build_stacks, sort_stacks, GroupBy, SortBy, Stack};
use crate::unsubscribe;
use std::collections::HashSet;

pub enum Mode {
    Normal,
    /// pending action awaiting y/n
    Confirm(PendingAction),
    Filter,
}

#[derive(Debug, Clone)]
pub enum PendingAction {
    Trash { stack_idxs: Vec<usize> },
    Archive { stack_idxs: Vec<usize> },
    Unsubscribe { stack_idxs: Vec<usize> },
    /// after a successful unsubscribe, offer to trash the stacks too
    TrashAfterUnsub { stack_idxs: Vec<usize> },
}

impl PendingAction {
    pub fn prompt(&self, acct: &AccountView) -> String {
        let summary = |idxs: &[usize]| -> String {
            let msgs: usize = idxs.iter().map(|&i| acct.stacks[i].msgs.len()).sum();
            if idxs.len() == 1 {
                format!("{} ({} msgs)", acct.stacks[idxs[0]].display_name, msgs)
            } else {
                format!("{} stacks ({} msgs)", idxs.len(), msgs)
            }
        };
        match self {
            PendingAction::Trash { stack_idxs } => {
                format!("Trash {}? [y/n]", summary(stack_idxs))
            }
            PendingAction::Archive { stack_idxs } => {
                format!("Archive {}? [y/n]", summary(stack_idxs))
            }
            PendingAction::Unsubscribe { stack_idxs } => {
                if let [i] = stack_idxs[..] {
                    let via = acct.stacks[i]
                        .unsubscribe_source()
                        .and_then(unsubscribe::pick_method)
                        .map(|m| m.describe())
                        .unwrap_or("?");
                    format!("Unsubscribe from {} via {via}? [y/n]", summary(stack_idxs))
                } else {
                    format!("Unsubscribe from {}? [y/n]", summary(stack_idxs))
                }
            }
            PendingAction::TrashAfterUnsub { stack_idxs } => {
                format!("Done. Also trash {}? [y/n]", summary(stack_idxs))
            }
        }
    }
}

pub struct AccountView {
    pub cfg: AccountConfig,
    pub password: Option<String>,
    pub client: Option<ImapClient>,
    pub stacks: Vec<Stack>,
    pub selected: usize,
    pub expanded: bool,
    pub msg_selected: usize,
    pub loaded: bool,
    /// stack keys marked for bulk actions
    pub marked: HashSet<String>,
}

impl AccountView {
    pub fn new(cfg: AccountConfig) -> Self {
        Self {
            cfg,
            password: None,
            client: None,
            stacks: Vec::new(),
            selected: 0,
            expanded: false,
            msg_selected: 0,
            loaded: false,
            marked: HashSet::new(),
        }
    }

    pub fn total_messages(&self) -> usize {
        self.stacks.iter().map(|s| s.msgs.len()).sum()
    }
}

#[derive(Default)]
pub struct SessionStats {
    pub trashed: usize,
    pub archived: usize,
    pub marked_read: usize,
    /// senders successfully unsubscribed from
    pub unsubscribed: usize,
}

pub struct App {
    pub accounts: Vec<AccountView>,
    pub active: usize,
    pub mode: Mode,
    pub group_by: GroupBy,
    pub sort_by: SortBy,
    pub filter: String,
    pub status: String,
    pub busy: bool,
    pub should_quit: bool,
    pub stats: SessionStats,
}

impl App {
    pub fn new(accounts: Vec<AccountConfig>) -> Self {
        Self {
            accounts: accounts.into_iter().map(AccountView::new).collect(),
            active: 0,
            mode: Mode::Normal,
            group_by: GroupBy::Sender,
            sort_by: SortBy::Count,
            filter: String::new(),
            status: String::from("loading…"),
            busy: false,
            should_quit: false,
            stats: SessionStats::default(),
        }
    }

    pub fn account(&self) -> &AccountView {
        &self.accounts[self.active]
    }

    pub fn account_mut(&mut self) -> &mut AccountView {
        &mut self.accounts[self.active]
    }

    /// indices into account().stacks that match the current filter
    pub fn visible_stacks(&self) -> Vec<usize> {
        let acct = self.account();
        if self.filter.is_empty() {
            return (0..acct.stacks.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        acct.stacks
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.key.contains(&needle)
                    || s.display_name.to_lowercase().contains(&needle)
                    || s.subject
                        .as_deref()
                        .is_some_and(|sub| sub.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// index into stacks of the currently selected (filtered) row
    pub fn selected_stack_idx(&self) -> Option<usize> {
        let visible = self.visible_stacks();
        visible.get(self.account().selected).copied()
    }

    /// stacks an action applies to: all marked, else the selected one
    pub fn target_stacks(&self) -> Vec<usize> {
        let acct = self.account();
        if acct.marked.is_empty() {
            self.selected_stack_idx().into_iter().collect()
        } else {
            acct.stacks
                .iter()
                .enumerate()
                .filter(|(_, s)| acct.marked.contains(&s.key))
                .map(|(i, _)| i)
                .collect()
        }
    }

    pub async fn load_active(&mut self) -> Result<()> {
        let active = self.active;
        let acct = &mut self.accounts[active];
        if acct.password.is_none() {
            acct.password = Some(crate::config::get_password(&acct.cfg.email)?);
        }
        if acct.client.is_none() {
            self.status = format!("connecting to {}…", acct.cfg.email);
            let client =
                ImapClient::connect(&acct.cfg, acct.password.as_deref().unwrap()).await?;
            acct.client = Some(client);
        }
        self.status = format!("fetching inbox for {}…", acct.cfg.email);
        let group_by = self.group_by;
        let sort_by = self.sort_by;
        let acct = &mut self.accounts[active];
        let msgs = acct.client.as_mut().unwrap().fetch_inbox().await?;
        let n = msgs.len();
        acct.stacks = build_stacks(msgs, group_by, sort_by);
        acct.selected = acct.selected.min(acct.stacks.len().saturating_sub(1));
        acct.expanded = false;
        acct.msg_selected = 0;
        acct.marked.clear();
        acct.loaded = true;
        self.status = format!(
            "{}: {} messages in {} stacks",
            acct.cfg.email,
            n,
            acct.stacks.len()
        );
        Ok(())
    }

    pub async fn handle_event(&mut self, ev: Event) {
        let Event::Key(key) = ev else { return };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key).await,
            Mode::Confirm(_) => self.handle_confirm(key).await,
            Mode::Filter => self.handle_filter(key),
        }
    }

    async fn handle_normal(&mut self, key: KeyEvent) {
        let visible = self.visible_stacks();
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => self.move_sel(1, &visible),
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => self.move_sel(-1, &visible),
            (KeyCode::Char('g'), _) => self.jump_sel(0, &visible),
            (KeyCode::Char('G'), _) => self.jump_sel(usize::MAX, &visible),
            (KeyCode::Enter, _) => {
                let acct = self.account_mut();
                if !acct.stacks.is_empty() {
                    acct.expanded = !acct.expanded;
                    acct.msg_selected = 0;
                }
            }
            (KeyCode::Esc, _) => {
                if self.account().expanded {
                    self.account_mut().expanded = false;
                } else if !self.account().marked.is_empty() {
                    self.account_mut().marked.clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.account_mut().selected = 0;
                }
            }
            (KeyCode::Char(' '), _) => {
                if !self.account().expanded {
                    if let Some(i) = self.selected_stack_idx() {
                        let key = self.account().stacks[i].key.clone();
                        let acct = self.account_mut();
                        if !acct.marked.remove(&key) {
                            acct.marked.insert(key);
                        }
                        // auto-advance for rapid marking
                        self.move_sel(1, &visible);
                    }
                }
            }
            (KeyCode::Char('a'), _) => {
                if !self.account().expanded {
                    let keys: Vec<String> = visible
                        .iter()
                        .map(|&i| self.account().stacks[i].key.clone())
                        .collect();
                    let acct = self.account_mut();
                    if keys.iter().all(|k| acct.marked.contains(k)) {
                        for k in &keys {
                            acct.marked.remove(k);
                        }
                    } else {
                        acct.marked.extend(keys);
                    }
                }
            }
            (KeyCode::Char('o'), _) => {
                self.sort_by = self.sort_by.toggle();
                let sort_by = self.sort_by;
                for acct in &mut self.accounts {
                    sort_stacks(&mut acct.stacks, sort_by);
                    acct.selected = 0;
                    acct.expanded = false;
                    acct.msg_selected = 0;
                }
                self.status = format!("sorting by {}", sort_by.label());
            }
            (KeyCode::Tab, _) => {
                self.active = (self.active + 1) % self.accounts.len();
                self.filter.clear();
                if !self.account().loaded {
                    self.run_load().await;
                }
            }
            (KeyCode::Char('R'), _) => self.run_load().await,
            (KeyCode::Char('s'), _) => {
                self.group_by = self.group_by.toggle();
                let group_by = self.group_by;
                let sort_by = self.sort_by;
                // regroup every loaded account from its already-fetched messages
                for acct in &mut self.accounts {
                    if !acct.loaded {
                        continue;
                    }
                    let msgs = acct
                        .stacks
                        .drain(..)
                        .flat_map(|s| s.msgs)
                        .collect::<Vec<_>>();
                    acct.stacks = build_stacks(msgs, group_by, sort_by);
                    acct.selected = 0;
                    acct.expanded = false;
                    acct.msg_selected = 0;
                    acct.marked.clear();
                }
                self.status = format!("grouping by {}", group_by.label());
            }
            (KeyCode::Char('/'), _) => {
                self.mode = Mode::Filter;
                self.filter.clear();
            }
            (KeyCode::Char('d'), _) => {
                let targets = self.target_stacks();
                if !targets.is_empty() {
                    self.mode = Mode::Confirm(PendingAction::Trash { stack_idxs: targets });
                }
            }
            (KeyCode::Char('e'), _) => {
                let targets = self.target_stacks();
                if !targets.is_empty() {
                    self.mode = Mode::Confirm(PendingAction::Archive { stack_idxs: targets });
                }
            }
            (KeyCode::Char('r'), _) => {
                let targets = self.target_stacks();
                if !targets.is_empty() {
                    self.mark_read(targets).await;
                }
            }
            (KeyCode::Char('u'), _) => {
                let targets: Vec<usize> = self
                    .target_stacks()
                    .into_iter()
                    .filter(|&i| {
                        self.account().stacks[i]
                            .unsubscribe_source()
                            .and_then(unsubscribe::pick_method)
                            .is_some()
                    })
                    .collect();
                if targets.is_empty() {
                    self.status = "no List-Unsubscribe header in selection".into();
                } else {
                    self.mode = Mode::Confirm(PendingAction::Unsubscribe { stack_idxs: targets });
                }
            }
            _ => {}
        }
    }

    async fn handle_confirm(&mut self, key: KeyEvent) {
        let Mode::Confirm(action) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.run_action(action).await,
            _ => self.status = "cancelled".into(),
        }
    }

    fn handle_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.account_mut().selected = 0;
            }
            _ => {}
        }
    }

    fn move_sel(&mut self, delta: i64, visible: &[usize]) {
        let acct = self.account_mut();
        if acct.expanded {
            if let Some(&i) = visible.get(acct.selected) {
                let len = acct.stacks[i].msgs.len();
                let cur = acct.msg_selected as i64 + delta;
                acct.msg_selected = cur.clamp(0, len as i64 - 1) as usize;
            }
        } else if !visible.is_empty() {
            let cur = acct.selected as i64 + delta;
            acct.selected = cur.clamp(0, visible.len() as i64 - 1) as usize;
        }
    }

    fn jump_sel(&mut self, pos: usize, visible: &[usize]) {
        let acct = self.account_mut();
        if acct.expanded {
            if let Some(&i) = visible.get(acct.selected) {
                acct.msg_selected = pos.min(acct.stacks[i].msgs.len().saturating_sub(1));
            }
        } else {
            acct.selected = pos.min(visible.len().saturating_sub(1));
        }
    }

    async fn run_load(&mut self) {
        self.busy = true;
        if let Err(e) = self.load_active().await {
            self.status = format!("error: {e:#}");
            // drop a possibly-broken session so next attempt reconnects
            self.account_mut().client = None;
        }
        self.busy = false;
    }

    async fn run_action(&mut self, action: PendingAction) {
        self.busy = true;
        let result = match action {
            PendingAction::Trash { stack_idxs } => self.trash(stack_idxs).await,
            PendingAction::Archive { stack_idxs } => self.archive(stack_idxs).await,
            PendingAction::TrashAfterUnsub { stack_idxs } => self.trash(stack_idxs).await,
            PendingAction::Unsubscribe { stack_idxs } => {
                let (ok, _failed) = self.unsubscribe_many(&stack_idxs).await;
                if ok > 0 {
                    // chain into "also trash?" prompt
                    self.mode = Mode::Confirm(PendingAction::TrashAfterUnsub { stack_idxs });
                }
                self.busy = false;
                return;
            }
        };
        if let Err(e) = result {
            self.status = format!("error: {e:#}");
            self.account_mut().client = None;
        }
        self.busy = false;
    }

    async fn trash(&mut self, stack_idxs: Vec<usize>) -> Result<()> {
        let (uids, label) = self.collect(&stack_idxs);
        let acct = self.account_mut();
        acct.client.as_mut().unwrap().trash(&uids).await?;
        self.remove_stacks(stack_idxs);
        self.stats.trashed += uids.len();
        self.status = format!("trashed {} messages from {label}", uids.len());
        Ok(())
    }

    async fn archive(&mut self, stack_idxs: Vec<usize>) -> Result<()> {
        let (uids, label) = self.collect(&stack_idxs);
        let acct = self.account_mut();
        acct.client.as_mut().unwrap().archive(&uids).await?;
        self.remove_stacks(stack_idxs);
        self.stats.archived += uids.len();
        self.status = format!("archived {} messages from {label}", uids.len());
        Ok(())
    }

    async fn mark_read(&mut self, stack_idxs: Vec<usize>) {
        self.busy = true;
        let (uids, label) = self.collect(&stack_idxs);
        let acct = self.account_mut();
        let res = acct.client.as_mut().unwrap().mark_read(&uids).await;
        match res {
            Ok(()) => {
                let acct = self.account_mut();
                for &i in &stack_idxs {
                    for m in &mut acct.stacks[i].msgs {
                        m.unread = false;
                    }
                    acct.stacks[i].unread_count = 0;
                }
                acct.marked.clear();
                self.stats.marked_read += uids.len();
                self.status = format!("marked {} messages read ({label})", uids.len());
            }
            Err(e) => {
                self.status = format!("error: {e:#}");
                self.account_mut().client = None;
            }
        }
        self.busy = false;
    }

    /// run unsubscribe for each stack that has a method; returns (ok, failed)
    async fn unsubscribe_many(&mut self, stack_idxs: &[usize]) -> (usize, usize) {
        let cfg = self.account().cfg.clone();
        let password = self.account().password.clone().unwrap_or_default();
        let mut ok = 0;
        let mut failed = 0;
        let mut last = String::new();
        for &i in stack_idxs {
            let method = self.account().stacks[i]
                .unsubscribe_source()
                .and_then(unsubscribe::pick_method);
            let Some(method) = method else {
                failed += 1;
                continue;
            };
            let name = self.account().stacks[i].display_name.clone();
            match unsubscribe::execute(&method, &cfg, &password).await {
                Ok(msg) => {
                    ok += 1;
                    last = format!("{name}: {msg}");
                }
                Err(e) => {
                    failed += 1;
                    last = format!("{name}: {e:#}");
                }
            }
        }
        self.status = if stack_idxs.len() == 1 {
            last
        } else if failed == 0 {
            format!("unsubscribed from {ok} stacks")
        } else {
            format!("unsubscribed {ok}/{} stacks (last: {last})", ok + failed)
        };
        self.stats.unsubscribed += ok;
        (ok, failed)
    }

    /// merged uids across stacks + a human label for the status line
    fn collect(&self, stack_idxs: &[usize]) -> (Vec<u32>, String) {
        let acct = self.account();
        let uids: Vec<u32> = stack_idxs
            .iter()
            .flat_map(|&i| acct.stacks[i].uids())
            .collect();
        let label = if let [i] = stack_idxs[..] {
            acct.stacks[i].display_name.clone()
        } else {
            format!("{} stacks", stack_idxs.len())
        };
        (uids, label)
    }

    fn remove_stacks(&mut self, mut stack_idxs: Vec<usize>) {
        stack_idxs.sort_unstable();
        stack_idxs.dedup();
        let acct = self.account_mut();
        for &i in stack_idxs.iter().rev() {
            acct.stacks.remove(i);
        }
        acct.marked.clear();
        acct.expanded = false;
        acct.msg_selected = 0;
        if acct.selected >= acct.stacks.len() {
            acct.selected = acct.stacks.len().saturating_sub(1);
        }
    }
}

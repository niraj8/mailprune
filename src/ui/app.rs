use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::action_log::{Action, ActionLog, StackSnapshot};
use crate::config::AccountConfig;
use crate::imap_client::ImapClient;
use crate::stacks::{GroupBy, SortBy, Stack, build_stacks, sort_stacks};
use crate::unsubscribe;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

pub enum Mode {
    Normal,
    /// pending action awaiting y/n
    Confirm(PendingAction),
    Filter,
}

#[derive(Debug, Clone)]
pub enum PendingAction {
    Trash {
        stack_idxs: Vec<usize>,
    },
    Archive {
        stack_idxs: Vec<usize>,
    },
    Unsubscribe {
        stack_idxs: Vec<usize>,
    },
    /// after a successful unsubscribe, offer to trash the stacks too
    TrashAfterUnsub {
        stack_idxs: Vec<usize>,
    },
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

/// messages sent from a spawned action task back to the event loop
pub enum TaskMsg {
    /// progress line for the status bar
    Status(String),
    Done(TaskDone),
}

#[derive(Debug, Clone, Copy)]
pub enum ImapKind {
    Trash,
    Archive,
    Read,
}

pub enum TaskDone {
    /// an IMAP mutation finished; the client comes back with it (discarded
    /// on error — the session state is unknown after a failure/timeout)
    Imap {
        client: Box<ImapClient>,
        kind: ImapKind,
        stack_idxs: Vec<usize>,
        n_msgs: usize,
        label: String,
        result: Result<()>,
    },
    Unsub {
        stack_idxs: Vec<usize>,
        ok_idxs: Vec<usize>,
        failed: usize,
        last: String,
    },
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
    /// stacks rendered on screen under the *current* grouping, keyed by stack
    /// key, features frozen at first sighting. Cleared when the grouping
    /// changes: a stack is a different object under a different grouping, and
    /// keeping both would emit two "keep" rows for the same messages.
    pub seen: HashMap<String, SeenStack>,
    /// uids acted on this session, excluded from "keep" at quit. Keyed by uid
    /// rather than stack key so the exclusion survives regrouping.
    pub acted: HashSet<u32>,
}

/// a stack as it was when first rendered
pub struct SeenStack {
    pub snap: StackSnapshot,
    /// the messages it was made of, for testing against `acted`
    pub uids: Vec<u32>,
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
            seen: HashMap::new(),
            acted: HashSet::new(),
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
    pub log: ActionLog,
    /// receiver for the in-flight action task, if any; while Some, mutating
    /// keys are blocked so stack indices captured by the task stay valid
    pub task_rx: Option<mpsc::UnboundedReceiver<TaskMsg>>,
}

impl App {
    pub fn new(accounts: Vec<AccountConfig>, log: ActionLog) -> Self {
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
            log,
            task_rx: None,
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

    /// called by the view with the stack rows actually on screen this frame;
    /// features are frozen at first sighting
    pub fn record_seen(&mut self, stack_idxs: &[usize]) {
        let acct = self.account_mut();
        for &i in stack_idxs {
            let stack = &acct.stacks[i];
            if !acct.seen.contains_key(&stack.key) {
                let seen = SeenStack {
                    snap: StackSnapshot::of(stack),
                    uids: stack.uids(),
                };
                acct.seen.insert(stack.key.clone(), seen);
            }
        }
    }

    /// snapshot the stacks as they are right now and append to the log
    fn log_action(&mut self, action: Action, stack_idxs: &[usize]) {
        let acct = &mut self.accounts[self.active];
        let snaps: Vec<StackSnapshot> = stack_idxs
            .iter()
            .map(|&i| StackSnapshot::of(&acct.stacks[i]))
            .collect();
        for &i in stack_idxs {
            acct.acted.extend(acct.stacks[i].uids());
        }
        self.log.log(&acct.cfg.email, action, &snaps);
    }

    /// rebuild every loaded account's stacks from its already-fetched messages
    fn regroup(&mut self, group_by: GroupBy) {
        self.group_by = group_by;
        let sort_by = self.sort_by;
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
            // snapshots describe stacks that no longer exist; keeping them
            // would log these messages as "keep" twice, once per grouping.
            // `acted` is uid-keyed and survives.
            acct.seen.clear();
        }
        self.status = format!("grouping by {}", group_by.label());
    }

    /// at session end, stacks seen on screen but never acted on are "keep".
    /// A stack containing any acted-on message is not a keep — the user
    /// already made a decision about it, possibly under another grouping.
    pub fn flush_keeps(&mut self) {
        for acct in &self.accounts {
            let mut keeps: Vec<StackSnapshot> = acct
                .seen
                .values()
                .filter(|seen| !seen.uids.iter().any(|uid| acct.acted.contains(uid)))
                .map(|seen| seen.snap.clone())
                .collect();
            keeps.sort_by(|a, b| (&a.sender, &a.subject).cmp(&(&b.sender, &b.subject)));
            self.log.log(&acct.cfg.email, Action::Keep, &keeps);
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
            let client = ImapClient::connect(&acct.cfg, acct.password.as_deref().unwrap()).await?;
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
            Mode::Confirm(_) => self.handle_confirm(key),
            Mode::Filter => self.handle_filter(key),
        }
    }

    async fn handle_normal(&mut self, key: KeyEvent) {
        // while an action task is in flight, only allow keys that can't
        // mutate or reorder stacks — the task holds indices into them
        if self.busy {
            let allowed = matches!(
                (key.code, key.modifiers),
                (KeyCode::Char('q'), _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('j'), _)
                    | (KeyCode::Char('k'), _)
                    | (KeyCode::Down, _)
                    | (KeyCode::Up, _)
                    | (KeyCode::Char('g'), _)
                    | (KeyCode::Char('G'), _)
                    | (KeyCode::Enter, _)
                    | (KeyCode::Esc, _)
            );
            if !allowed {
                return;
            }
        }
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
            (KeyCode::Char('s'), _) => self.regroup(self.group_by.toggle()),
            (KeyCode::Char('/'), _) => {
                self.mode = Mode::Filter;
                self.filter.clear();
            }
            (KeyCode::Char('d'), _) => {
                let targets = self.target_stacks();
                if !targets.is_empty() {
                    self.mode = Mode::Confirm(PendingAction::Trash {
                        stack_idxs: targets,
                    });
                }
            }
            (KeyCode::Char('e'), _) => {
                let targets = self.target_stacks();
                if !targets.is_empty() {
                    self.mode = Mode::Confirm(PendingAction::Archive {
                        stack_idxs: targets,
                    });
                }
            }
            (KeyCode::Char('r'), _) => {
                let targets = self.target_stacks();
                if !targets.is_empty() {
                    self.spawn_imap(ImapKind::Read, targets);
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
                    self.mode = Mode::Confirm(PendingAction::Unsubscribe {
                        stack_idxs: targets,
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_confirm(&mut self, key: KeyEvent) {
        let Mode::Confirm(action) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.run_action(action),
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

    /// dispatch a confirmed action to a background task so the event loop
    /// keeps drawing while the network work runs
    fn run_action(&mut self, action: PendingAction) {
        match action {
            PendingAction::Trash { stack_idxs } | PendingAction::TrashAfterUnsub { stack_idxs } => {
                self.spawn_imap(ImapKind::Trash, stack_idxs)
            }
            PendingAction::Archive { stack_idxs } => self.spawn_imap(ImapKind::Archive, stack_idxs),
            PendingAction::Unsubscribe { stack_idxs } => self.spawn_unsub(stack_idxs),
        }
    }

    fn spawn_imap(&mut self, kind: ImapKind, stack_idxs: Vec<usize>) {
        let (uids, label) = self.collect(&stack_idxs);
        let acct = self.account_mut();
        let Some(mut client) = acct.client.take() else {
            self.status = "not connected — press R to reconnect".into();
            return;
        };
        let (tx, rx) = mpsc::unbounded_channel();
        self.task_rx = Some(rx);
        self.busy = true;
        let verb = match kind {
            ImapKind::Trash => "trashing",
            ImapKind::Archive => "archiving",
            ImapKind::Read => "marking read",
        };
        self.status = format!("{verb} {} messages…", uids.len());
        tokio::spawn(async move {
            let result = match kind {
                ImapKind::Trash => client.trash(&uids).await,
                ImapKind::Archive => client.archive(&uids).await,
                ImapKind::Read => client.mark_read(&uids).await,
            };
            let _ = tx.send(TaskMsg::Done(TaskDone::Imap {
                client: Box::new(client),
                kind,
                stack_idxs,
                n_msgs: uids.len(),
                label,
                result,
            }));
        });
    }

    fn spawn_unsub(&mut self, stack_idxs: Vec<usize>) {
        let acct = self.account();
        let cfg = acct.cfg.clone();
        let password = acct.password.clone().unwrap_or_default();
        // resolve method + name per stack up front; the stacks stay untouched
        // while the task runs because mutating keys are blocked when busy
        let jobs: Vec<(usize, String, Option<unsubscribe::Method>)> = stack_idxs
            .iter()
            .map(|&i| {
                let s = &acct.stacks[i];
                (
                    i,
                    s.display_name.clone(),
                    s.unsubscribe_source().and_then(unsubscribe::pick_method),
                )
            })
            .collect();
        let (tx, rx) = mpsc::unbounded_channel();
        self.task_rx = Some(rx);
        self.busy = true;
        self.status = format!("unsubscribing from {} stacks…", jobs.len());
        tokio::spawn(async move {
            let total = jobs.len();
            let mut ok_idxs: Vec<usize> = Vec::new();
            let mut failed = 0;
            let mut last = String::new();
            for (done, (i, name, method)) in jobs.into_iter().enumerate() {
                let Some(method) = method else {
                    failed += 1;
                    continue;
                };
                let _ = tx.send(TaskMsg::Status(format!(
                    "unsubscribing {}/{total}: {name}…",
                    done + 1
                )));
                crate::debuglog::write(format!("unsub start {name} via {}", method.describe()));
                match unsubscribe::execute(&method, &cfg, &password).await {
                    Ok(msg) => {
                        ok_idxs.push(i);
                        crate::debuglog::write(format!("unsub done {name}: {msg}"));
                        last = format!("{name}: {msg}");
                    }
                    Err(e) => {
                        failed += 1;
                        crate::debuglog::write(format!("unsub FAILED {name}: {e:#}"));
                        last = format!("{name}: {e:#}");
                    }
                }
            }
            let _ = tx.send(TaskMsg::Done(TaskDone::Unsub {
                stack_idxs,
                ok_idxs,
                failed,
                last,
            }));
        });
    }

    /// apply a finished task's outcome to app state (runs on the event loop)
    pub fn on_task_done(&mut self, done: TaskDone) {
        self.busy = false;
        self.task_rx = None;
        match done {
            TaskDone::Imap {
                client,
                kind,
                stack_idxs,
                n_msgs,
                label,
                result,
            } => match result {
                Ok(()) => {
                    self.account_mut().client = Some(*client);
                    match kind {
                        ImapKind::Trash => {
                            self.log_action(Action::Trash, &stack_idxs);
                            self.remove_stacks(stack_idxs);
                            self.stats.trashed += n_msgs;
                            self.status = format!("trashed {n_msgs} messages from {label}");
                        }
                        ImapKind::Archive => {
                            self.log_action(Action::Archive, &stack_idxs);
                            self.remove_stacks(stack_idxs);
                            self.stats.archived += n_msgs;
                            self.status = format!("archived {n_msgs} messages from {label}");
                        }
                        ImapKind::Read => {
                            // snapshot before mutating flags so the log shows
                            // the read rate the user actually acted on
                            self.log_action(Action::Read, &stack_idxs);
                            let acct = self.account_mut();
                            for &i in &stack_idxs {
                                for m in &mut acct.stacks[i].msgs {
                                    m.unread = false;
                                }
                                acct.stacks[i].unread_count = 0;
                            }
                            acct.marked.clear();
                            self.stats.marked_read += n_msgs;
                            self.status = format!("marked {n_msgs} messages read ({label})");
                        }
                    }
                }
                Err(e) => {
                    // session state unknown after failure/timeout: drop the
                    // client so the next R reconnects fresh
                    drop(client);
                    self.account_mut().client = None;
                    self.status = format!("error: {e:#}");
                }
            },
            TaskDone::Unsub {
                stack_idxs,
                ok_idxs,
                failed,
                last,
            } => {
                let ok = ok_idxs.len();
                self.status = if stack_idxs.len() == 1 {
                    last
                } else if failed == 0 {
                    format!("unsubscribed from {ok} stacks")
                } else {
                    format!("unsubscribed {ok}/{} stacks (last: {last})", ok + failed)
                };
                self.stats.unsubscribed += ok;
                self.log_action(Action::Unsub, &ok_idxs);
                if ok > 0 {
                    // chain into "also trash?" prompt
                    self.mode = Mode::Confirm(PendingAction::TrashAfterUnsub { stack_idxs });
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stacks::MsgMeta;

    fn msg(uid: u32, sender: &str, subject: &str) -> MsgMeta {
        MsgMeta {
            uid,
            sender_email: sender.into(),
            sender_name: String::new(),
            subject: subject.into(),
            date: None,
            unread: true,
            list_unsubscribe: None,
            one_click: false,
        }
    }

    /// two senders, two subjects each — so regrouping actually changes the
    /// stack set rather than being a no-op
    fn test_msgs() -> Vec<MsgMeta> {
        vec![
            msg(1, "a@x.com", "one"),
            msg(2, "a@x.com", "two"),
            msg(3, "b@x.com", "one"),
            msg(4, "b@x.com", "two"),
        ]
    }

    fn test_app(log: ActionLog) -> App {
        let cfg = AccountConfig {
            name: "t".into(),
            email: "me@x.com".into(),
            imap_host: "imap".into(),
            smtp_host: "smtp".into(),
        };
        let mut app = App::new(vec![cfg], log);
        app.accounts[0].stacks = build_stacks(test_msgs(), GroupBy::Sender, SortBy::Count);
        app.accounts[0].loaded = true;
        app
    }

    fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(path).unwrap();
        raw.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// each test writes to its own file; the process id alone collides
    /// when tests run in parallel
    fn temp_log(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("mailprune-app-{name}-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn stack_idx(app: &App, key: &str) -> usize {
        app.accounts[0]
            .stacks
            .iter()
            .position(|s| s.key == key)
            .unwrap()
    }

    #[test]
    fn keeps_are_seen_minus_acted() {
        let path = temp_log("keeps");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.record_seen(&[0, 1]);
        // re-sighting must not overwrite the first snapshot
        app.record_seen(&[0]);
        app.log_action(Action::Trash, &[stack_idx(&app, "a@x.com")]);
        app.flush_keeps();

        let recs = read_records(&path);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["action"], "trash");
        assert_eq!(recs[0]["sender"], "a@x.com");
        assert_eq!(recs[1]["action"], "keep");
        assert_eq!(recs[1]["sender"], "b@x.com");
        std::fs::remove_file(&path).unwrap();
    }

    /// regrouping rebuilds the stacks under new keys. Without clearing
    /// `seen`, the same messages are logged as "keep" once per grouping.
    #[test]
    fn regrouping_does_not_double_log_keeps() {
        let path = temp_log("regroup");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.record_seen(&[0, 1]);

        app.regroup(GroupBy::SenderSubject);
        let all: Vec<usize> = (0..app.account().stacks.len()).collect();
        assert_eq!(all.len(), 4, "one stack per sender+subject pair");
        app.record_seen(&all);
        app.flush_keeps();

        let recs = read_records(&path);
        assert!(recs.iter().all(|r| r["action"] == "keep"));
        assert_eq!(recs.len(), 4, "only the current grouping is reported");
        assert!(recs.iter().all(|r| r["group_by"] == "sender+subject"));
        std::fs::remove_file(&path).unwrap();
    }

    /// `acted` is uid-keyed, so a decision made under one grouping still
    /// suppresses the "keep" for those messages under another.
    #[test]
    fn acting_then_regrouping_does_not_relabel_as_keep() {
        let path = temp_log("acted-regroup");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.record_seen(&[0, 1]);
        // marking read leaves the stack in place, unlike trash/archive
        app.log_action(Action::Read, &[stack_idx(&app, "a@x.com")]);

        app.regroup(GroupBy::SenderSubject);
        let all: Vec<usize> = (0..app.account().stacks.len()).collect();
        app.record_seen(&all);
        app.flush_keeps();

        let recs = read_records(&path);
        assert_eq!(recs[0]["action"], "read");
        let keeps: Vec<&serde_json::Value> =
            recs.iter().filter(|r| r["action"] == "keep").collect();
        assert_eq!(keeps.len(), 2, "only b@x.com's two subject stacks");
        assert!(keeps.iter().all(|r| r["sender"] == "b@x.com"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unsub_done_logs_chains_trash_prompt_and_clears_busy() {
        let path = temp_log("unsub");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.busy = true;
        app.on_task_done(TaskDone::Unsub {
            stack_idxs: vec![0, 1],
            ok_idxs: vec![0],
            failed: 1,
            last: "x".into(),
        });
        assert!(!app.busy);
        assert_eq!(app.stats.unsubscribed, 1);
        assert!(matches!(
            app.mode,
            Mode::Confirm(PendingAction::TrashAfterUnsub { .. })
        ));
        let raw = std::fs::read_to_string(&path).unwrap();
        let recs: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(recs.len(), 1); // only the successful unsub is logged
        assert_eq!(recs[0]["action"], "unsub");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unseen_stacks_are_not_logged_as_keeps() {
        let path = std::env::temp_dir().join(format!(
            "mailprune-app-test-unseen-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut app = test_app(ActionLog::at(path.clone()));
        app.record_seen(&[0]); // second stack never rendered
        app.flush_keeps();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1);
        std::fs::remove_file(&path).unwrap();
    }
}

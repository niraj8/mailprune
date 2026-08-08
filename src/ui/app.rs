use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::action_log::{Action, ActionLog, StackSnapshot};
use crate::config::AccountConfig;
use crate::imap_client::{self, ImapClient, SenderBatch};
use crate::stacks::{GroupBy, SortBy, Stack, build_stacks, sort_stacks};
use crate::unsubscribe;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

pub enum Mode {
    Normal,
    /// pending action awaiting y/n
    Confirm(PendingAction),
    Filter,
    /// full keybinding overlay, dismissed by any key
    Help,
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
    /// the fresh uid list from a reset load. Sent ahead of the batch so the
    /// pane title can show the true mailbox total while stacks are still
    /// streaming in.
    Uids {
        acct_idx: usize,
        uids: Vec<u32>,
    },
    /// one sender resolved. Fan-out is serial on the single session — two
    /// round trips per sender — so waiting for the whole batch would be
    /// seconds of blank screen; streaming puts the first stacks up at ~200ms.
    Sender {
        acct_idx: usize,
        batch: Box<SenderBatch>,
    },
    Done(TaskDone),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapKind {
    Trash,
    Archive,
    Read,
}

/// what a load batch does to what is already on screen
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// `R` and first load: re-take the uid list and clear everything loaded
    Reset,
    /// `m`: continue from the cursor and append
    More,
}

pub enum TaskDone {
    /// an IMAP mutation finished; the action session comes back with it
    /// (None when the session is unusable — a connect that failed, or a
    /// failure/timeout that left the session in an unknown state)
    Imap {
        client: Option<Box<ImapClient>>,
        /// the keychain lookup's result, cached so later work skips it
        password: Option<String>,
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
        password: Option<String>,
    },
    /// a load batch finished for `acct_idx`; its stacks already arrived as
    /// `TaskMsg::Sender`s
    Batch {
        acct_idx: usize,
        /// None when the session is unusable (connect failed, or a timeout
        /// left the session in an unknown state)
        client: Option<Box<ImapClient>>,
        /// the keychain lookup's result, cached so later loads skip it
        password: Option<String>,
        kind: Load,
        /// how far discovery got. Reported alongside the outcome rather than
        /// inside it: a failure does not un-scan the UIDs already read, and
        /// dropping the cursor would make the next `m` re-read them.
        cursor: usize,
        result: Result<()>,
    },
}

pub struct AccountView {
    pub cfg: AccountConfig,
    pub password: Option<String>,
    /// the session a load runs on, held for the whole of that load
    pub client: Option<ImapClient>,
    /// actions run on a session of their own so they never queue behind a
    /// load, which owns `client` from the moment it starts. Connected on
    /// first use and kept for the session.
    pub action_client: Option<ImapClient>,
    pub stacks: Vec<Stack>,
    pub selected: usize,
    pub loaded: bool,
    /// every uid in INBOX, newest first, taken once per reset. UIDs are
    /// immutable, so the cursor below survives trashing — sequence numbers
    /// would be renumbered by every `UID MOVE`.
    pub uids: Vec<u32>,
    /// how far discovery has walked `uids`
    pub cursor: usize,
    /// lowercased addresses already fanned out, so `m` yields a batch of
    /// senders that are new rather than sightings of the ones on screen
    pub known_senders: HashSet<String>,
    /// senders whose fan-out the server refused: their stacks hold only the
    /// discovery sample, so their counts under-report and are marked `~`
    pub partial_senders: HashSet<String>,
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
    /// uids an action removed from INBOX while a load was in flight. The
    /// load's cursor indexes the list as it was when the load started, so
    /// these come out only once that cursor has landed.
    pub pending_prune: HashSet<u32>,
    /// a completed reset load's sort, waiting on the action task whose
    /// captured stack indices it would reorder
    pub pending_sort: bool,
}

/// The account's password, from the cache if it is there and the keychain if
/// it is not. Always called from inside a spawned task: the keychain read is
/// sync and can take seconds on first unlock, so it stays off the event loop
/// with everything else.
async fn resolve_password(email: &str, cached: Option<String>) -> Result<String> {
    if let Some(p) = cached {
        return Ok(p);
    }
    let email = email.to_owned();
    tokio::task::spawn_blocking(move || crate::config::get_password(&email))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
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
            action_client: None,
            stacks: Vec::new(),
            selected: 0,
            loaded: false,
            uids: Vec::new(),
            cursor: 0,
            known_senders: HashSet::new(),
            partial_senders: HashSet::new(),
            marked: HashSet::new(),
            seen: HashMap::new(),
            acted: HashSet::new(),
            pending_prune: HashSet::new(),
            pending_sort: false,
        }
    }

    /// messages currently in stacks — a recency window over the mailbox
    pub fn loaded_messages(&self) -> usize {
        self.stacks.iter().map(|s| s.msgs.len()).sum()
    }

    /// every message in INBOX. Falls as mail is trashed, which is the number
    /// this tool exists to move.
    pub fn inbox_total(&self) -> usize {
        self.uids.len()
    }

    /// no more senders left to discover
    pub fn exhausted(&self) -> bool {
        self.cursor >= self.uids.len()
    }

    /// Drop uids that have left INBOX from the discovery list. The cursor is
    /// pulled back by however many of them sat behind it, keeping it over the
    /// same message — it is an index, so removing entries behind it would
    /// otherwise slide it forward over senders that were never discovered.
    fn drop_uids(&mut self, gone: &HashSet<u32>) {
        let cursor = self.cursor;
        let mut removed_before = 0;
        let mut i = 0;
        // retain visits in order, so `i` tracks the position in the old list
        self.uids.retain(|uid| {
            let keep = !gone.contains(uid);
            if !keep && i < cursor {
                removed_before += 1;
            }
            i += 1;
            keep
        });
        self.cursor = cursor - removed_before;
    }

    /// this stack's messages are only the discovery sample — the server
    /// refused to enumerate the sender
    pub fn is_partial(&self, stack: &Stack) -> bool {
        !self.partial_senders.is_empty()
            && self
                .partial_senders
                .contains(&stack.latest().sender_email.to_lowercase())
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
    /// an action task is in flight. It captured stack indices before it
    /// started, so mutating keys are blocked until it lands.
    pub busy: bool,
    /// frame counter for the status-bar spinner, advanced by the event loop's
    /// ticker while a task runs
    pub spinner: usize,
    /// which account has a load in flight, if any. It only ever appends
    /// stacks and is read-only besides, so it blocks no keys and quitting can
    /// abandon it — unlike a mutation, it has no outcome to log. Named by
    /// account rather than a bare flag because `Tab` is live during a load,
    /// so the account loading and the account being acted on can differ.
    pub loading_acct: Option<usize>,
    pub should_quit: bool,
    pub stats: SessionStats,
    pub log: ActionLog,
    /// receiver for the in-flight action task, if any
    pub action_rx: Option<mpsc::UnboundedReceiver<TaskMsg>>,
    /// receiver for the in-flight load task, if any. Separate from
    /// `action_rx` so a load and an action can run at the same time.
    pub load_rx: Option<mpsc::UnboundedReceiver<TaskMsg>>,
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
            spinner: 0,
            loading_acct: None,
            should_quit: false,
            stats: SessionStats::default(),
            log,
            action_rx: None,
            load_rx: None,
        }
    }

    pub fn account(&self) -> &AccountView {
        &self.accounts[self.active]
    }

    /// a load is in flight, on whichever account
    pub fn loading(&self) -> bool {
        self.loading_acct.is_some()
    }

    /// the account on screen is the one being loaded, so an empty pane means
    /// "not here yet" rather than "nothing to show"
    pub fn active_is_loading(&self) -> bool {
        self.loading_acct == Some(self.active)
    }

    /// some task is in flight, so the spinner has something to say
    pub fn working(&self) -> bool {
        self.busy || self.loading()
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

    /// status line describing an account's stacks as they stand now
    fn account_summary(&self, idx: usize) -> String {
        let acct = &self.accounts[idx];
        format!(
            "{}: {} of {} messages in {} stacks",
            acct.cfg.email,
            acct.loaded_messages(),
            acct.inbox_total(),
            acct.stacks.len()
        )
    }

    /// `m`: one more batch of senders, appended. The mailbox is never loaded
    /// whole — loading is explicit, and the pane refills only on this key.
    /// The uid list is untouched, so this is bounded work however big it is.
    fn load_more(&mut self) {
        let acct = self.account();
        if acct.loaded && acct.exhausted() {
            self.status = "no more senders — R to refresh".into();
            return;
        }
        self.spawn_batch(Load::More);
    }

    /// connect (if needed) and load one batch of senders in a background task,
    /// so the event loop keeps drawing while the network work runs
    pub fn spawn_batch(&mut self, kind: Load) {
        // there is one load task slot, and a load owns it and the account's
        // session for its whole run, so a second one has nothing to run on
        if self.loading() {
            self.status = "already loading — wait for this batch".into();
            return;
        }
        let acct_idx = self.active;
        // without a uid list there is no cursor to continue from
        let reset = kind == Load::Reset || !self.accounts[acct_idx].loaded;
        let kind = if reset { Load::Reset } else { Load::More };
        let acct = &mut self.accounts[acct_idx];
        let cfg = acct.cfg.clone();
        let cached_password = acct.password.clone();
        // a live session is reused on refresh; taking it keeps the account from
        // being used by two paths at once
        let existing = acct.client.take();
        if reset {
            acct.stacks.clear();
            acct.uids.clear();
            acct.cursor = 0;
            acct.known_senders.clear();
            acct.partial_senders.clear();
            acct.marked.clear();
            acct.selected = 0;
            acct.loaded = false;
            // `seen` is kept: a reset keeps the current grouping, so its
            // snapshots still describe the same stacks
        }
        let uids = acct.uids.clone();
        let cursor = acct.cursor;
        let known = acct.known_senders.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        self.load_rx = Some(rx);
        self.loading_acct = Some(acct_idx);
        self.status = if existing.is_some() {
            format!("finding senders for {}…", cfg.email)
        } else {
            format!("connecting to {}…", cfg.email)
        };
        tokio::spawn(async move {
            // handed back in the Done message so later loads and the
            // unsubscribe path skip the keychain
            let mut resolved = cached_password;
            let mut client = match existing {
                Some(c) => c,
                None => {
                    let password = match resolve_password(&cfg.email, resolved.clone()).await {
                        Ok(p) => {
                            resolved = Some(p.clone());
                            p
                        }
                        Err(e) => {
                            let _ = tx.send(TaskMsg::Done(TaskDone::Batch {
                                acct_idx,
                                client: None,
                                password: None,
                                kind,
                                cursor,
                                result: Err(e),
                            }));
                            return;
                        }
                    };
                    match ImapClient::connect(&cfg, &password).await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(TaskMsg::Done(TaskDone::Batch {
                                acct_idx,
                                client: None,
                                password: resolved,
                                kind,
                                cursor,
                                result: Err(e),
                            }));
                            return;
                        }
                    }
                }
            };
            let mut uids = uids;
            if reset {
                let _ = tx.send(TaskMsg::Status(format!("listing {}…", cfg.email)));
                match client.uid_list().await {
                    Ok(list) => {
                        uids = list;
                        let _ = tx.send(TaskMsg::Uids {
                            acct_idx,
                            uids: uids.clone(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(TaskMsg::Done(TaskDone::Batch {
                            acct_idx,
                            client: None,
                            password: resolved,
                            kind,
                            cursor,
                            result: Err(e),
                        }));
                        return;
                    }
                }
            }
            let _ = tx.send(TaskMsg::Status(format!(
                "finding senders for {}…",
                cfg.email
            )));
            let (cursor, result) =
                imap_client::load_batch(&mut client, &uids, cursor, &known, |batch| {
                    let _ = tx.send(TaskMsg::Sender {
                        acct_idx,
                        batch: Box::new(batch),
                    });
                })
                .await;
            // after a timeout the session state is unknown, so the client goes;
            // a server refusal leaves the connection perfectly usable
            let dead = result.as_ref().err().is_some_and(imap_client::is_timeout);
            let client = (!dead).then(|| Box::new(client));
            let _ = tx.send(TaskMsg::Done(TaskDone::Batch {
                acct_idx,
                client,
                password: resolved,
                kind,
                cursor,
                result,
            }));
        });
    }

    pub fn handle_event(&mut self, ev: Event) {
        let Event::Key(key) = ev else { return };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Confirm(_) => self.handle_confirm(key),
            Mode::Filter => self.handle_filter(key),
            Mode::Help => self.mode = Mode::Normal,
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) {
        // While an action task is in flight, only allow keys that can't mutate
        // or reorder stacks — the task holds indices into them. A load holds
        // none, so it locks nothing: `busy` is the action, not the network.
        if self.busy {
            let allowed = matches!(
                (key.code, key.modifiers),
                (KeyCode::Char('q'), _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('j'), _)
                    | (KeyCode::Char('k'), _)
                    | (KeyCode::Down, _)
                    | (KeyCode::Up, _)
                    | (KeyCode::Home, _)
                    | (KeyCode::End, _)
                    | (KeyCode::Char('G'), _)
                    | (KeyCode::Esc, _)
                    | (KeyCode::Char('?'), _)
            );
            if !allowed {
                // silently dropping the key looks like a hung TUI
                self.status = "busy — wait for the current action to finish".into();
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
            (KeyCode::Home, _) => self.jump_sel(0, &visible),
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => self.jump_sel(usize::MAX, &visible),
            (KeyCode::Esc, _) => {
                if !self.account().marked.is_empty() {
                    self.account_mut().marked.clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.account_mut().selected = 0;
                }
            }
            (KeyCode::Char(' '), _) => {
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
            (KeyCode::Char('a'), _) => {
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
            (KeyCode::Char('s'), _) => {
                self.sort_by = self.sort_by.toggle();
                let sort_by = self.sort_by;
                for acct in &mut self.accounts {
                    sort_stacks(&mut acct.stacks, sort_by);
                    acct.selected = 0;
                }
                self.status = format!("sorting by {}", sort_by.label());
            }
            (KeyCode::Tab, _) => {
                self.active = (self.active + 1) % self.accounts.len();
                self.filter.clear();
                if self.account().loaded {
                    // the status line is global: without this it keeps
                    // describing the account we just left
                    self.status = self.account_summary(self.active);
                } else {
                    self.spawn_batch(Load::Reset);
                }
            }
            (KeyCode::Char('?'), _) => self.mode = Mode::Help,
            (KeyCode::Char('m'), _) => self.load_more(),
            (KeyCode::Char('R'), _) => self.spawn_batch(Load::Reset),
            (KeyCode::Char('g'), _) => self.regroup(self.group_by.toggle()),
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
        if !visible.is_empty() {
            let cur = acct.selected as i64 + delta;
            acct.selected = cur.clamp(0, visible.len() as i64 - 1) as usize;
        }
    }

    fn jump_sel(&mut self, pos: usize, visible: &[usize]) {
        let acct = self.account_mut();
        acct.selected = pos.min(visible.len().saturating_sub(1));
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
        let cfg = acct.cfg.clone();
        let cached_password = acct.password.clone();
        let existing = acct.action_client.take();
        let (tx, rx) = mpsc::unbounded_channel();
        self.action_rx = Some(rx);
        self.busy = true;
        let verb = match kind {
            ImapKind::Trash => "trashing",
            ImapKind::Archive => "archiving",
            ImapKind::Read => "marking read",
        };
        self.status = format!("{verb} {} messages…", uids.len());
        tokio::spawn(async move {
            let mut resolved = cached_password;
            // the first action of the session opens the connection it runs on,
            // rather than waiting for whatever the load is doing with its own
            let mut client = match existing {
                Some(c) => c,
                None => {
                    let connected = match resolve_password(&cfg.email, resolved.clone()).await {
                        Ok(p) => {
                            resolved = Some(p.clone());
                            ImapClient::connect(&cfg, &p).await
                        }
                        Err(e) => Err(e),
                    };
                    match connected {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(TaskMsg::Done(TaskDone::Imap {
                                client: None,
                                password: resolved,
                                kind,
                                stack_idxs,
                                n_msgs: uids.len(),
                                label,
                                result: Err(e),
                            }));
                            return;
                        }
                    }
                }
            };
            let result = match kind {
                ImapKind::Trash => client.trash(&uids).await,
                ImapKind::Archive => client.archive(&uids).await,
                ImapKind::Read => client.mark_read(&uids).await,
            };
            let _ = tx.send(TaskMsg::Done(TaskDone::Imap {
                client: Some(Box::new(client)),
                password: resolved,
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
        let cached_password = acct.password.clone();
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
        self.action_rx = Some(rx);
        self.busy = true;
        self.status = format!("unsubscribing from {} stacks…", jobs.len());
        tokio::spawn(async move {
            // a mailto unsubscribe is sent over SMTP, so it needs the password
            // even on the first action of the session, before a load has
            // cached one. A one-click POST does not, so a keychain failure
            // here is left to `execute` to report per stack.
            let resolved = resolve_password(&cfg.email, cached_password).await.ok();
            let password = resolved.clone().unwrap_or_default();
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
                password: resolved,
            }));
        });
    }

    /// apply a finished task's outcome to app state (runs on the event loop)
    pub fn on_task_done(&mut self, done: TaskDone) {
        match done {
            TaskDone::Imap {
                client,
                password,
                kind,
                stack_idxs,
                n_msgs,
                label,
                result,
            } => {
                self.busy = false;
                let acct = self.account_mut();
                if password.is_some() {
                    acct.password = password;
                }
                match result {
                    Ok(()) => {
                        self.account_mut().action_client = client.map(|c| *c);
                        match kind {
                            ImapKind::Trash => {
                                self.log_action(Action::Trash, &stack_idxs);
                                self.prune_uids(&stack_idxs);
                                self.remove_stacks(stack_idxs);
                                self.stats.trashed += n_msgs;
                                self.status = format!("trashed {n_msgs} messages from {label}");
                            }
                            ImapKind::Archive => {
                                self.log_action(Action::Archive, &stack_idxs);
                                self.prune_uids(&stack_idxs);
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
                        // client so the next action connects fresh
                        drop(client);
                        self.account_mut().action_client = None;
                        self.status = format!("error: {e:#}");
                    }
                }
                self.apply_deferred_sort();
            }
            TaskDone::Unsub {
                stack_idxs,
                ok_idxs,
                failed,
                last,
                password,
            } => {
                self.busy = false;
                if password.is_some() {
                    self.account_mut().password = password;
                }
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
                self.apply_deferred_sort();
            }
            TaskDone::Batch {
                acct_idx,
                client,
                password,
                kind,
                cursor,
                result,
            } => {
                self.loading_acct = None;
                let busy = self.busy;
                let mut failure = None;
                let acct = &mut self.accounts[acct_idx];
                acct.client = client.map(|c| *c);
                if password.is_some() {
                    acct.password = password;
                }
                // the cursor lands either way: a failure does not un-scan the
                // UIDs discovery already read
                acct.cursor = cursor;
                match result {
                    Ok(()) => {
                        acct.loaded = true;
                        if kind == Load::Reset {
                            // the batch streamed in one sender at a time, so
                            // until now the order is discovery order
                            acct.pending_sort = true;
                        }
                    }
                    // already-streamed stacks stay on screen: a failure that
                    // arrives halfway through is not a reason to throw away
                    // the half that worked
                    Err(e) => failure = Some(format!("error: {e:#}")),
                }
                // now that the cursor the load walked to has landed, uids an
                // action pruned mid-load can come out of the list it indexes
                let acct = &mut self.accounts[acct_idx];
                let pruned = std::mem::take(&mut acct.pending_prune);
                acct.drop_uids(&pruned);
                // an action in flight holds indices the sort would reorder, so
                // it waits — the action's own completion runs it instead
                if !busy {
                    self.apply_deferred_sort();
                }
                // the action the user is waiting on owns the status line: a
                // load's summary is not worth talking over it, though a
                // failure still is
                if let Some(e) = failure {
                    self.status = e;
                } else if !busy {
                    self.status = self.account_summary(acct_idx);
                }
            }
        }
    }

    /// Run the sort a reset load put off. Called when nothing holds a stack
    /// index any more — an action task has landed, or none was running — so
    /// reordering the list can no longer move a stack out from under one.
    /// Every account is checked, not just the active one: `Tab` is live
    /// during a load, so the account that finished loading may not be on
    /// screen when the action that was blocking its sort completes.
    fn apply_deferred_sort(&mut self) {
        let sort_by = self.sort_by;
        for acct in &mut self.accounts {
            if std::mem::take(&mut acct.pending_sort) {
                sort_stacks(&mut acct.stacks, sort_by);
                acct.selected = 0;
            }
        }
    }

    /// the uid list for a reset load, which arrives before the stacks do
    pub fn on_uids(&mut self, acct_idx: usize, uids: Vec<u32>) {
        let acct = &mut self.accounts[acct_idx];
        acct.uids = uids;
        acct.cursor = 0;
    }

    /// one sender resolved mid-batch. It is new by construction — discovery
    /// skips known senders — so its stacks cannot collide with any already on
    /// screen and are simply appended.
    pub fn on_sender(&mut self, acct_idx: usize, batch: SenderBatch) {
        let (group_by, sort_by) = (self.group_by, self.sort_by);
        let acct = &mut self.accounts[acct_idx];
        acct.known_senders.insert(batch.addr.clone());
        if batch.partial {
            acct.partial_senders.insert(batch.addr);
        }
        if batch.msgs.is_empty() {
            return;
        }
        acct.stacks
            .append(&mut build_stacks(batch.msgs, group_by, sort_by));
        acct.loaded = true;
    }

    /// advance the status-bar spinner one frame
    pub fn tick_spinner(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
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

    /// Take the acted-on stacks' uids out of the discovery list, so a later
    /// `m` does not spend its scan budget reading a graveyard — unless the
    /// account is mid-load, in which case they wait for it.
    fn prune_uids(&mut self, stack_idxs: &[usize]) {
        let loading = self.active_is_loading();
        let acct = self.account_mut();
        let gone: HashSet<u32> = stack_idxs
            .iter()
            .flat_map(|&i| acct.stacks[i].uids())
            .collect();
        if gone.is_empty() {
            return;
        }
        if loading {
            // the in-flight load's cursor counts the list as it stands right
            // now, so removing from it here would land that cursor over
            // senders the load never read. It comes out once the cursor does.
            acct.pending_prune.extend(gone);
        } else {
            acct.drop_uids(&gone);
        }
    }

    fn remove_stacks(&mut self, mut stack_idxs: Vec<usize>) {
        stack_idxs.sort_unstable();
        stack_idxs.dedup();
        let acct = self.account_mut();
        for &i in stack_idxs.iter().rev() {
            acct.stacks.remove(i);
        }
        acct.marked.clear();
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
        app.group_by = GroupBy::Sender;
        app.accounts[0].stacks = build_stacks(test_msgs(), GroupBy::Sender, SortBy::Count);
        app.accounts[0].uids = vec![4, 3, 2, 1];
        app.accounts[0].cursor = 4;
        app.accounts[0].loaded = true;
        app
    }

    fn sender(addr: &str, msgs: Vec<MsgMeta>) -> SenderBatch {
        SenderBatch {
            addr: addr.into(),
            msgs,
            partial: false,
        }
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

    fn keys(app: &App) -> Vec<&str> {
        app.accounts[0]
            .stacks
            .iter()
            .map(|s| s.key.as_str())
            .collect()
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
            password: None,
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
    fn a_reset_batch_streams_stacks_in_then_sorts_once_it_completes() {
        let path = temp_log("load-ok");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].stacks.clear();
        app.accounts[0].loaded = false;
        app.loading_acct = Some(0);

        app.on_uids(0, vec![4, 3, 2, 1]);
        assert_eq!(app.accounts[0].inbox_total(), 4);
        // the smaller sender resolves first, so a sorted result proves the
        // completion sort ran rather than discovery order surviving
        app.on_sender(0, sender("b@x.com", vec![msg(3, "b@x.com", "one")]));
        assert_eq!(app.accounts[0].stacks.len(), 1, "stacks appear mid-batch");
        app.on_sender(
            0,
            sender(
                "a@x.com",
                vec![msg(1, "a@x.com", "one"), msg(2, "a@x.com", "two")],
            ),
        );
        app.on_task_done(TaskDone::Batch {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::Reset,
            cursor: 4,
            result: Ok(()),
        });

        assert!(!app.loading());
        assert!(app.accounts[0].loaded);
        assert_eq!(app.accounts[0].cursor, 4);
        let keys: Vec<&str> = app.accounts[0]
            .stacks
            .iter()
            .map(|s| s.key.as_str())
            .collect();
        assert_eq!(keys, ["a@x.com", "b@x.com"], "sorted by count on reset");
        assert!(
            app.status.contains("3 of 4 messages"),
            "status: {}",
            app.status
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `m` means one thing: a batch more senders, on the end. It must not reorder
    /// what the user is already looking at.
    #[test]
    fn a_continuation_batch_appends_without_reordering() {
        let path = temp_log("load-more");
        let mut app = test_app(ActionLog::at(path.clone()));
        let before: Vec<String> = app.accounts[0]
            .stacks
            .iter()
            .map(|s| s.key.clone())
            .collect();

        // one message, so sorting by count would put it first
        app.on_sender(0, sender("c@x.com", vec![msg(9, "c@x.com", "hi")]));
        app.on_task_done(TaskDone::Batch {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            cursor: 4,
            result: Ok(()),
        });

        let after: Vec<String> = app.accounts[0]
            .stacks
            .iter()
            .map(|s| s.key.clone())
            .collect();
        assert_eq!(after[..before.len()], before[..], "the head is untouched");
        assert_eq!(after.last().unwrap(), "c@x.com");
        assert!(app.accounts[0].known_senders.contains("c@x.com"));
        let _ = std::fs::remove_file(&path);
    }

    /// omission from a triage list is undetectable, so a refused fan-out shows
    /// what it managed to read and says the count is short
    #[test]
    fn a_refused_sender_becomes_a_partial_stack_rather_than_a_gap() {
        let path = temp_log("partial");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.on_sender(
            0,
            SenderBatch {
                addr: "c@x.com".into(),
                msgs: vec![msg(9, "c@x.com", "hi")],
                partial: true,
            },
        );
        let acct = &app.accounts[0];
        let stack = acct.stacks.last().unwrap();
        assert!(acct.is_partial(stack));
        assert!(
            !acct.is_partial(&acct.stacks[0]),
            "the others are unaffected"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn batch_error_clears_loading_and_keeps_what_already_streamed_in() {
        let path = temp_log("load-err");
        let mut app = test_app(ActionLog::at(path.clone()));
        let before = app.accounts[0].stacks.len();
        app.loading_acct = Some(0);
        app.on_task_done(TaskDone::Batch {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::Reset,
            cursor: 7,
            result: Err(anyhow::anyhow!("boom")),
        });
        assert!(!app.loading());
        assert!(app.accounts[0].client.is_none());
        assert_eq!(app.accounts[0].stacks.len(), before);
        // a failure does not un-scan what discovery already read; dropping the
        // cursor would make the next `m` spend its whole budget re-reading it
        assert_eq!(app.accounts[0].cursor, 7);
        assert!(app.status.starts_with("error:"), "status: {}", app.status);
        let _ = std::fs::remove_file(&path);
    }

    /// the cursor is an index, so removing entries behind it would slide it
    /// forward over senders that were never discovered
    #[test]
    fn pruning_acted_uids_holds_the_cursor_over_the_same_message() {
        let path = temp_log("prune");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].uids = vec![5, 4, 3, 2, 1];
        app.accounts[0].cursor = 3; // next unread uid is 2
        // stack "a@x.com" holds uids 1 and 2 — one behind the cursor, one ahead
        let a = stack_idx(&app, "a@x.com");
        app.prune_uids(&[a]);

        assert_eq!(app.accounts[0].uids, vec![5, 4, 3]);
        assert_eq!(app.accounts[0].cursor, 3, "uid 3 is still next");
        assert!(app.accounts[0].exhausted());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn m_on_an_exhausted_list_says_so_instead_of_reconnecting() {
        let path = temp_log("m-exhausted");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.handle_normal(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(!app.loading(), "no load was spawned");
        assert!(app.status.contains("no more senders"), "{}", app.status);
        let _ = std::fs::remove_file(&path);
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_normal(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// `g` groups, `s` sorts — the two keys whose old names (`s` and `o`) said
    /// nothing about what they did
    #[test]
    fn g_regroups_and_s_resorts() {
        let path = temp_log("keys-gs");
        let mut app = test_app(ActionLog::at(path.clone()));
        assert_eq!(app.group_by, GroupBy::Sender);
        assert_eq!(app.sort_by, SortBy::Count);

        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.group_by, GroupBy::SenderSubject);
        assert_eq!(
            app.account().stacks.len(),
            4,
            "regrouped, one stack per sender+subject"
        );

        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.sort_by, SortBy::ReadRate);
        assert!(app.status.contains("read rate"), "{}", app.status);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn home_and_end_jump_the_stack_list() {
        let path = temp_log("keys-homeend");
        let mut app = test_app(ActionLog::at(path.clone()));
        let last = app.account().stacks.len() - 1;

        press(&mut app, KeyCode::End);
        assert_eq!(app.account().selected, last);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.account().selected, 0);
        // G stays bound for vim muscle memory
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.account().selected, last);
        let _ = std::fs::remove_file(&path);
    }

    /// the expand mode is gone: Enter had no per-message action behind it, and
    /// `o` was freed rather than kept as a hidden sort alias
    #[test]
    fn enter_and_o_do_nothing() {
        let path = temp_log("keys-dead");
        let mut app = test_app(ActionLog::at(path.clone()));
        press(&mut app, KeyCode::Char('j'));
        let selected = app.account().selected;

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('o'));

        assert_eq!(app.account().selected, selected, "selection untouched");
        assert_eq!(app.group_by, GroupBy::Sender);
        assert_eq!(app.sort_by, SortBy::Count);
        let _ = std::fs::remove_file(&path);
    }

    /// `g` reorders the stacks an in-flight task indexes into, so it has to be
    /// blocked while busy even though the key it replaced (jump to top) was safe
    #[test]
    fn g_is_refused_while_an_action_is_in_flight() {
        let path = temp_log("keys-busy");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.busy = true;

        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.group_by, GroupBy::Sender, "regroup refused");
        assert!(app.status.starts_with("busy"), "{}", app.status);

        app.status.clear();
        press(&mut app, KeyCode::End);
        assert_eq!(
            app.account().selected,
            app.account().stacks.len() - 1,
            "moving the cursor is still allowed"
        );
        assert!(app.status.is_empty(), "{}", app.status);
        let _ = std::fs::remove_file(&path);
    }

    /// A load is not an action, and must not raise the flag the key gate
    /// reads: doing so puts the whole triage keyboard behind the fetch.
    #[tokio::test]
    async fn a_load_in_flight_leaves_the_keyboard_unlocked() {
        let path = temp_log("load-not-busy");
        let mut app = test_app(ActionLog::at(path.clone()));
        // a cached password keeps the spawned task off the keychain; it dies
        // on the unresolvable host a moment later, unobserved
        app.accounts[0].password = Some("x".into());
        app.accounts[0].cursor = 0;

        press(&mut app, KeyCode::Char('m'));

        assert!(app.loading(), "the load is in flight");
        assert!(!app.busy, "and it locks nothing");
        assert!(app.load_rx.is_some());
        assert!(app.action_rx.is_none(), "a load is not an action");
        let _ = std::fs::remove_file(&path);
    }

    /// a load only ever appends stacks, so it holds no indices for a key to
    /// invalidate — the whole triage keyboard stays live while it runs
    #[tokio::test]
    async fn triage_keys_stay_live_while_the_mailbox_loads() {
        let path = temp_log("keys-loading");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.loading_acct = Some(0);
        // as above: a cached password keeps `r`'s task off the keychain
        app.accounts[0].password = Some("x".into());

        press(&mut app, KeyCode::Char('d'));
        assert!(
            matches!(app.mode, Mode::Confirm(PendingAction::Trash { .. })),
            "trash prompt opened mid-load"
        );
        app.mode = Mode::Normal;

        press(&mut app, KeyCode::Char('e'));
        assert!(matches!(
            app.mode,
            Mode::Confirm(PendingAction::Archive { .. })
        ));
        app.mode = Mode::Normal;

        // g/s/'/' reshape the view rather than the mailbox
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.group_by, GroupBy::SenderSubject, "regroup accepted");
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.sort_by, SortBy::ReadRate, "re-sort accepted");
        press(&mut app, KeyCode::Char('/'));
        assert!(matches!(app.mode, Mode::Filter), "filter accepted");
        app.mode = Mode::Normal;

        // these stacks carry no List-Unsubscribe header, so `u` reaching its
        // own refusal is what proves the gate let it through
        press(&mut app, KeyCode::Char('u'));
        assert!(
            app.status.contains("no List-Unsubscribe"),
            "status: {}",
            app.status
        );

        // `r` needs no confirmation, so it is the one triage key that reaches
        // the network straight from the gate. It goes last: it is an action,
        // and an action does lock the keys behind it.
        press(&mut app, KeyCode::Char('r'));
        assert!(app.busy, "mark-read dispatched mid-load");
        assert!(app.action_rx.is_some());
        assert!(app.loading(), "and the load is still going");
        assert!(!app.status.contains("busy"), "status: {}", app.status);
        let _ = std::fs::remove_file(&path);
    }

    /// `Tab` is live during a load, so the account being acted on need not be
    /// the one loading. Only the loading account has a cursor mid-walk, so
    /// only its prune waits — deferring the other's would strand those uids
    /// until that account happened to load, leaving `exhausted` counting mail
    /// that is already in the bin.
    #[test]
    fn a_trash_on_an_account_that_is_not_loading_prunes_at_once() {
        let path = temp_log("prune-other-acct");
        let mut app = test_app(ActionLog::at(path.clone()));
        let mut second = AccountView::new(AccountConfig {
            name: "other".into(),
            email: "other@x.com".into(),
            imap_host: "imap".into(),
            smtp_host: "smtp".into(),
        });
        second.stacks = build_stacks(
            vec![msg(9, "c@x.com", "hi")],
            GroupBy::Sender,
            SortBy::Count,
        );
        second.uids = vec![9, 8];
        second.cursor = 2;
        second.loaded = true;
        app.accounts.push(second);
        // the first account is loading; the user has tabbed to the second
        app.loading_acct = Some(0);
        app.active = 1;

        app.prune_uids(&[0]);

        assert_eq!(app.accounts[1].uids, vec![8]);
        assert_eq!(app.accounts[1].cursor, 1, "uid 9 sat behind the cursor");
        assert!(
            app.accounts[1].pending_prune.is_empty(),
            "nothing to wait for on an account with no load in flight"
        );
        assert!(app.accounts[0].pending_prune.is_empty(), "wrong account");
        let _ = std::fs::remove_file(&path);
    }

    /// the one key a load does own: a second load would fight the first for
    /// the session and the task slot
    #[test]
    fn a_second_load_is_refused_while_one_is_already_running() {
        let path = temp_log("keys-double-load");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.loading_acct = Some(0);
        app.accounts[0].cursor = 0; // senders left, so `m` would otherwise fetch

        press(&mut app, KeyCode::Char('m'));
        assert!(app.load_rx.is_none(), "no second load spawned");
        assert!(app.status.contains("already loading"), "{}", app.status);
        let _ = std::fs::remove_file(&path);
    }

    /// a reset load sorts on completion. An action in flight captured indices
    /// into the list that sort would reorder, so it waits for the action.
    #[test]
    fn a_load_landing_during_an_action_does_not_reorder_the_stacks() {
        let path = temp_log("deferred-sort");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].stacks.clear();
        app.accounts[0].loaded = false;
        app.loading_acct = Some(0);
        // the one-message sender streams in first, so sorting by count moves it
        app.on_sender(0, sender("b@x.com", vec![msg(3, "b@x.com", "one")]));
        app.on_sender(
            0,
            sender(
                "a@x.com",
                vec![msg(1, "a@x.com", "one"), msg(2, "a@x.com", "two")],
            ),
        );
        // an action is now holding index 0 — b@x.com
        app.busy = true;

        app.on_task_done(TaskDone::Batch {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::Reset,
            cursor: 4,
            result: Ok(()),
        });
        assert_eq!(keys(&app), ["b@x.com", "a@x.com"], "discovery order held");
        assert!(!app.loading());
        assert!(app.busy, "the action is still in flight");

        // the action lands, releasing the indices — the sort runs now
        app.on_task_done(TaskDone::Unsub {
            stack_idxs: vec![0],
            ok_idxs: vec![],
            failed: 1,
            last: "x".into(),
            password: None,
        });
        assert_eq!(keys(&app), ["a@x.com", "b@x.com"], "deferred sort applied");
        let _ = std::fs::remove_file(&path);
    }

    /// A trash mid-load prunes uids the load is still walking. The load's
    /// cursor indexes the un-pruned list, so the prune waits for it to land —
    /// applying it first would slide the cursor over undiscovered senders.
    #[test]
    fn uids_pruned_during_a_load_are_applied_after_its_cursor_lands() {
        let path = temp_log("deferred-prune");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].uids = vec![5, 4, 3, 2, 1];
        app.accounts[0].cursor = 0;
        app.loading_acct = Some(0);

        // "a@x.com" holds uids 1 and 2, at indices 4 and 3
        let a = stack_idx(&app, "a@x.com");
        app.prune_uids(&[a]);
        assert_eq!(
            app.accounts[0].uids,
            vec![5, 4, 3, 2, 1],
            "the list the load is walking is untouched"
        );

        app.on_task_done(TaskDone::Batch {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            cursor: 4, // the load stopped over uid 1
            result: Ok(()),
        });
        assert_eq!(app.accounts[0].uids, vec![5, 4, 3]);
        assert_eq!(app.accounts[0].cursor, 3, "uid 2 sat behind the cursor");
        assert!(app.accounts[0].exhausted());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn spinner_advances_a_frame_at_a_time() {
        let path = temp_log("spinner");
        let mut app = test_app(ActionLog::at(path.clone()));
        let before = app.spinner;
        app.tick_spinner();
        assert_eq!(app.spinner, before + 1);
        let _ = std::fs::remove_file(&path);
    }

    /// the status line is shared by every account, so switching to one that
    /// needs no fetch must still rewrite it
    #[test]
    fn tab_to_a_loaded_account_restates_its_summary() {
        let path = temp_log("tab-status");
        let mut app = test_app(ActionLog::at(path.clone()));
        let mut second = AccountView::new(AccountConfig {
            name: "other".into(),
            email: "other@x.com".into(),
            imap_host: "imap".into(),
            smtp_host: "smtp".into(),
        });
        second.stacks = build_stacks(
            vec![msg(9, "c@x.com", "hi")],
            GroupBy::Sender,
            SortBy::Count,
        );
        second.uids = vec![9];
        second.cursor = 1;
        second.loaded = true;
        app.accounts.push(second);
        app.status = "me@x.com: 4 of 4 messages in 2 stacks".into();

        app.handle_normal(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.active, 1);
        assert_eq!(app.status, "other@x.com: 1 of 1 messages in 1 stacks");
        let _ = std::fs::remove_file(&path);
    }

    /// a dropped keypress with no feedback reads as a hung TUI
    #[test]
    fn mutating_keys_while_busy_say_why() {
        let path = temp_log("busy-key");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.busy = true;
        app.handle_normal(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal), "no confirm prompt opened");
        assert!(app.status.contains("busy"), "status: {}", app.status);
        let _ = std::fs::remove_file(&path);
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

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::action_log::{Action, ActionLog, StackSnapshot};
use crate::config::AccountConfig;
use crate::imap_client::{self, ImapClient, SweepProgress};
use crate::stacks::{GroupBy, MsgMeta, SortBy, Stack, build_stacks, sort_stacks};
use crate::unsubscribe;

use super::view::commas;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

/// The one centered slot for "the app is busy" and for "the app wants an
/// answer" (ADR 0004). The confirm is the third state of the same box, but its
/// copy already lives in `Mode::Confirm` — the view merges the two rather than
/// duplicating a pending action here.
pub enum Alert {
    /// a sweep is running and the TUI is inert (ADR 0001). `progress` is None
    /// until the first chunk reports, which is where `starting` earns its keep.
    Sweeping {
        /// what the sweep is doing before it has a count to show
        starting: String,
        progress: Option<SweepProgress>,
    },
    /// The last sweep stopped. It stays up until a key dismisses it: the status
    /// row behind it keeps the detail, but the count is the thing the user has
    /// to be told, and a line that scrolls past unread has not told them.
    Failed(String),
}

/// May a sweep of this kind carry on with the session it was handed?
///
/// Widening does — it is the same connection mid-triage, and re-dialling for
/// every `m` would put a login in front of a key the user presses often. A
/// reset does not: `R` is the reconnect key, so the one thing it must not do
/// is keep a session whose death is the reason it was pressed.
fn reuses_session(kind: Load) -> bool {
    matches!(kind, Load::More)
}

/// who an action is about: "DoorDash", or "3 stacks" once it is more than one
fn whose(acct: &AccountView, stack_idxs: &[usize]) -> String {
    match stack_idxs {
        [i] => acct.stacks[*i].display_name.clone(),
        many => format!("{} stacks", many.len()),
    }
}

/// What the server refusing to fan a sender out means for the action: it runs,
/// but for that sender it reaches only the mail already in view. This is
/// **partial** — an action-time error, never a state a stack sits in, because
/// nothing is fanned out until the user acts (ADR 0002). Empty when every
/// search answered, so callers can append it unconditionally.
fn partial_note(refused: &[String]) -> String {
    match refused {
        [] => String::new(),
        [one] => format!(" — {one}'s search was refused, so only its mail in view"),
        many => format!(
            " — {} searches were refused, so only their mail in view",
            many.len()
        ),
    }
}

/// the two keys that leave, live in every state including a running sweep
fn is_quit(key: KeyEvent) -> bool {
    matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL)
    )
}

pub enum Mode {
    Normal,
    /// pending action awaiting y/n
    Confirm(PendingAction),
    Filter,
    /// full keybinding overlay, dismissed by any key
    Help,
}

/// What an action's fan-out found: every message the targeted senders still
/// have in the mailbox, however far back the window reaches.
///
/// Produced by the `SEARCH` the user's keypress runs, before the confirm
/// prompt — the round trip the action needs anyway, moved ahead of the question
/// so the prompt can state a real number (ADR 0002).
#[derive(Debug, Clone, Default)]
pub struct FanOut {
    /// the set the action works on
    pub uids: Vec<u32>,
    /// how many of them the window was already showing — the `(N in view)` the
    /// prompt states alongside the mailbox-wide count
    pub in_view: usize,
    /// Senders whose `SEARCH` the server refused. The action still runs, but
    /// for them it reaches only the mail already in view — which is what
    /// **partial** means now: an outcome an action reports, never a state a
    /// stack sits in.
    pub refused: Vec<String>,
}

/// what a fan-out is being run for — the confirm it opens once it lands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanFor {
    Trash,
    Archive,
    /// the "also trash?" offer chained off a successful unsubscribe
    TrashAfterUnsub,
}

impl FanFor {
    /// the question to put to the user now that there is a count behind it
    fn confirm(self, stack_idxs: Vec<usize>, fan: FanOut) -> PendingAction {
        match self {
            FanFor::Trash => PendingAction::Trash { stack_idxs, fan },
            FanFor::Archive => PendingAction::Archive { stack_idxs, fan },
            FanFor::TrashAfterUnsub => PendingAction::TrashAfterUnsub { stack_idxs, fan },
        }
    }
}

#[derive(Debug, Clone)]
pub enum PendingAction {
    Trash {
        stack_idxs: Vec<usize>,
        fan: FanOut,
    },
    Archive {
        stack_idxs: Vec<usize>,
        fan: FanOut,
    },
    Unsubscribe {
        stack_idxs: Vec<usize>,
    },
    /// after a successful unsubscribe, offer to trash the stacks too
    TrashAfterUnsub {
        stack_idxs: Vec<usize>,
        fan: FanOut,
    },
}

impl PendingAction {
    /// The alert's headline. It does not name `y`/`n`: the hint line under it
    /// does, and the box saying the same thing twice is the ADR 0004 layout
    /// wasting one of its three lines.
    pub fn prompt(&self, acct: &AccountView) -> String {
        // "400 messages from DoorDash (12 in view)" — what moves, where it
        // moves from, and how much of it the user has actually seen. Without
        // the first number a stack showing 12 silently deletes 400; without
        // the second the two numbers on screen contradict each other with no
        // explanation (ADR 0002).
        let mail = |idxs: &[usize], fan: &FanOut| -> String {
            let n = fan.uids.len();
            let plural = if n == 1 { "" } else { "s" };
            format!(
                "{} message{plural} from {} ({} in view)",
                commas(n),
                whose(acct, idxs),
                commas(fan.in_view)
            )
        };
        match self {
            PendingAction::Trash { stack_idxs, fan } => format!("trash {}?", mail(stack_idxs, fan)),
            PendingAction::Archive { stack_idxs, fan } => {
                format!("archive {}?", mail(stack_idxs, fan))
            }
            PendingAction::Unsubscribe { stack_idxs } => {
                // unsubscribing is per sender, not per message, so this one
                // counts senders rather than the mail behind them
                let who = whose(acct, stack_idxs);
                if let [i] = stack_idxs[..] {
                    let via = acct.stacks[i]
                        .unsubscribe_source()
                        .and_then(unsubscribe::pick_method)
                        .map(|m| m.describe())
                        .unwrap_or("?");
                    format!("unsubscribe from {who} via {via}?")
                } else {
                    format!("unsubscribe from {who}?")
                }
            }
            PendingAction::TrashAfterUnsub { stack_idxs, fan } => {
                format!("unsubscribed — also trash {}?", mail(stack_idxs, fan))
            }
        }
    }
}

/// messages sent from a spawned action task back to the event loop
pub enum TaskMsg {
    /// progress line for the status bar
    Status(String),
    /// One chunk of a sweep landed. Numbers rather than a formatted line: the
    /// alert owns the wording, and it needs the counts for its bar as well as
    /// its headline. A sweep's chunks report here and nowhere else — nothing
    /// renders until the window completes (ADR 0003).
    Sweeping(SweepProgress),
    Done(TaskDone),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapKind {
    Trash,
    Archive,
    Read,
}

/// what a sweep does to what is already on screen
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// `R` and first load: discard the window and sweep it again from the top
    Reset,
    /// `m`: widen the window by another sweep behind the one already read
    More,
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
        /// senders the fan-out could not enumerate, carried through so the
        /// outcome can say the action reached only their mail in view
        refused: Vec<String>,
        result: Result<()>,
    },
    /// a fan-out finished; its count is what the confirm prompt states
    Fanned {
        /// None when a timeout left the session unusable
        client: Option<Box<ImapClient>>,
        what: FanFor,
        stack_idxs: Vec<usize>,
        fan: FanOut,
        /// the fan-out gave up entirely — a refusal per sender is inside `fan`
        result: Result<()>,
    },
    Unsub {
        stack_idxs: Vec<usize>,
        ok_idxs: Vec<usize>,
        failed: usize,
        last: String,
    },
    /// a sweep finished for `acct_idx`, carrying the whole window it read —
    /// the list is built once, here, and never chunk by chunk
    Swept {
        acct_idx: usize,
        /// None when the session is unusable (connect failed, or a timeout
        /// left the session in an unknown state)
        client: Option<Box<ImapClient>>,
        /// the keychain lookup's result, cached so later loads skip it
        password: Option<String>,
        kind: Load,
        /// what the sweep read. Reported alongside the outcome rather than
        /// inside it: a failure does not un-read the chunks that landed, and
        /// dropping them would make the next `m` re-read them.
        sweep: Box<imap_client::Sweep>,
        result: Result<()>,
    },
}

pub struct AccountView {
    pub cfg: AccountConfig,
    pub password: Option<String>,
    pub client: Option<ImapClient>,
    pub stacks: Vec<Stack>,
    pub selected: usize,
    /// A sweep has been run against this account this session. Distinct from
    /// `loaded`, which says one *landed*: `Tab` is a navigation key, and an
    /// account whose password is wrong would otherwise hold the TUI inert for
    /// the connect timeout on every pass through the tab bar. `R` is the retry.
    pub visited: bool,
    pub loaded: bool,
    /// every message in INBOX, from the last sweep's `EXISTS`
    pub total: usize,
    /// How far back from the newest message the window reaches — the count the
    /// next sweep starts behind. Not a UID: sequence numbers are renumbered by
    /// every arrival and every `UID MOVE`, so the window is a distance from the
    /// top and is re-anchored off a fresh `EXISTS` each sweep (ADR 0003).
    pub back: usize,
    /// the window has reached the oldest message in the mailbox
    pub reached_end: bool,
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
            visited: false,
            loaded: false,
            total: 0,
            back: 0,
            reached_end: false,
            marked: HashSet::new(),
            seen: HashMap::new(),
            acted: HashSet::new(),
        }
    }

    /// messages currently in stacks — a recency window over the mailbox
    pub fn loaded_messages(&self) -> usize {
        self.stacks.iter().map(|s| s.msgs.len()).sum()
    }

    /// every message in INBOX. Falls as mail is trashed, which is the number
    /// this tool exists to move.
    pub fn inbox_total(&self) -> usize {
        self.total
    }

    /// How far back the window reaches, in messages — the number the title
    /// states as `newest 5,000`. Not `loaded_messages()`: trashed mail leaves
    /// dead UIDs that a sweep reads and gets nothing for, so the window is
    /// always the wider of the two, and it is the window the user is deciding
    /// whether to trust.
    pub fn window(&self) -> usize {
        self.back
    }

    /// the window reaches the oldest message — there is nothing left to sweep
    pub fn exhausted(&self) -> bool {
        self.reached_end
    }

    /// Fold a completed window's messages in and rebuild every stack, so the
    /// list is grouped and sorted once rather than merged chunk by chunk
    /// (ADR 0003). Windows can overlap when mail arrived between two sweeps,
    /// so a message already held wins over the repeat.
    /// The selection is restored by the caller, which is the only place that
    /// can see the filter — `selected` is a row in the *visible* list, not an
    /// index into `stacks`.
    fn absorb(&mut self, msgs: Vec<MsgMeta>, group_by: GroupBy, sort_by: SortBy) {
        let mut all: Vec<MsgMeta> = self.stacks.drain(..).flat_map(|s| s.msgs).collect();
        all.extend(msgs);
        let mut seen = HashSet::new();
        all.retain(|m| seen.insert(m.uid));
        self.stacks = build_stacks(all, group_by, sort_by);
    }

    /// keep the selection on a row that exists, for the paths with no stack to
    /// follow — a removal takes its row with it
    fn clamp_selection(&mut self) {
        if self.selected >= self.stacks.len() {
            self.selected = self.stacks.len().saturating_sub(1);
        }
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
    /// the one centered slot, when something is in it (ADR 0004)
    pub alert: Option<Alert>,
    /// Colour is off (`NO_COLOR`), so the alert's frame carries on its own.
    /// Read once at startup — the environment does not change under a running
    /// TUI. #5 lifts this into the theme module that owns every colour here.
    pub no_color: bool,
    pub busy: bool,
    /// frame counter for the status-bar spinner, advanced by the event loop's
    /// ticker while `busy`
    pub spinner: usize,
    /// the in-flight task is a read-only inbox fetch, so quitting can abandon
    /// it — unlike a mutation, it has no outcome to log
    pub loading: bool,
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
            alert: None,
            // https://no-color.org: set and non-empty turns colour off
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            busy: false,
            spinner: 0,
            loading: false,
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
        self.visible_in(self.active)
    }

    /// the same, for an account that may not be the active one — a sweep
    /// finishes against the account it was spawned for, whatever `Tab` did
    /// meanwhile
    fn visible_in(&self, acct_idx: usize) -> Vec<usize> {
        let acct = &self.accounts[acct_idx];
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

    /// key of the stack an account's cursor is on, the one thing about the
    /// selection that survives a regroup or a re-sort
    fn selected_key(&self, acct_idx: usize) -> Option<String> {
        let acct = &self.accounts[acct_idx];
        let i = *self.visible_in(acct_idx).get(acct.selected)?;
        Some(acct.stacks[i].key.clone())
    }

    /// put the cursor back on the stack it was on, wherever the rebuild moved
    /// it. A key can vanish — every one of its messages was acted on — so the
    /// row it left behind is the fallback.
    fn follow_selection(&mut self, acct_idx: usize, was_on: Option<String>) {
        let visible = self.visible_in(acct_idx);
        let row = was_on.and_then(|key| {
            visible
                .iter()
                .position(|&i| self.accounts[acct_idx].stacks[i].key == key)
        });
        let acct = &mut self.accounts[acct_idx];
        acct.selected = row.unwrap_or_else(|| acct.selected.min(visible.len().saturating_sub(1)));
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

    /// Called by the view with the stack rows actually on screen this frame.
    /// Features are frozen at first sighting, except when a widened window has
    /// grown the stack: `m` folds new messages into stacks already on screen,
    /// and a "keep" that reported the count from before the widening would
    /// under-report the mail the user actually decided to keep.
    pub fn record_seen(&mut self, stack_idxs: &[usize]) {
        let acct = self.account_mut();
        for &i in stack_idxs {
            let stack = &acct.stacks[i];
            let grown = acct
                .seen
                .get(&stack.key)
                .is_some_and(|seen| seen.uids.len() < stack.msgs.len());
            if grown || !acct.seen.contains_key(&stack.key) {
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

    /// `m`: widen the window by one more sweep, behind the one already read.
    /// The mailbox is never loaded whole — every sweep is the same bounded
    /// work however big it is.
    fn load_more(&mut self) {
        let acct = self.account();
        if acct.loaded && acct.exhausted() {
            self.status = "no more messages — R to refresh".into();
            return;
        }
        self.spawn_batch(Load::More);
    }

    /// connect (if needed) and sweep one window in a background task, so the
    /// event loop keeps drawing while the network work runs
    pub fn spawn_batch(&mut self, kind: Load) {
        let acct_idx = self.active;
        // nothing swept yet means there is no window to widen
        let reset = kind == Load::Reset || !self.accounts[acct_idx].loaded;
        let kind = if reset { Load::Reset } else { Load::More };
        let acct = &mut self.accounts[acct_idx];
        let cfg = acct.cfg.clone();
        let cached_password = acct.password.clone();
        // Taking it keeps the account from being used by two paths at once; a
        // reset then throws it away rather than reusing it, because `R`
        // reconnects (ADR 0002). It is the recovery path after a dead socket,
        // and a socket that died without saying so answers every command by
        // not answering — reusing it spends the timeout again and lands back
        // here. Dropped rather than logged out: there may be nothing on the
        // other end to say goodbye to.
        let existing = acct.client.take().filter(|_| reuses_session(kind));
        if reset {
            acct.stacks.clear();
            acct.total = 0;
            acct.back = 0;
            acct.reached_end = false;
            acct.marked.clear();
            acct.selected = 0;
            acct.loaded = false;
            // `seen` is kept: a reset keeps the current grouping, so its
            // snapshots still describe the same stacks
        }
        // marked here rather than on the outcome: a sweep that failed has still
        // been spent, and `Tab` must not spend another one on the way past
        acct.visited = true;
        let back = acct.back;
        let (tx, rx) = mpsc::unbounded_channel();
        self.task_rx = Some(rx);
        self.busy = true;
        self.loading = true;
        // ADR 0001 requires the UI to say plainly that keys are refused rather
        // than swallow them in silence; the alert's hint line is where it says
        // so, for the sweep's whole length. The status row keeps the same words
        // for what shows behind the box and after it comes down.
        self.status = if existing.is_some() {
            format!("sweeping {}…", cfg.email)
        } else {
            format!("connecting to {}…", cfg.email)
        };
        self.alert = Some(Alert::Sweeping {
            starting: self.status.clone(),
            progress: None,
        });
        tokio::spawn(async move {
            // handed back in the Done message so later loads and the
            // unsubscribe path skip the keychain
            let mut resolved = cached_password;
            let mut client = match existing {
                Some(c) => c,
                None => {
                    // the keychain read is sync and can take seconds on first
                    // unlock, so it stays off the event loop with everything
                    // else in this task
                    let password = match resolved.clone() {
                        Some(p) => p,
                        None => {
                            let email = cfg.email.clone();
                            let looked_up = tokio::task::spawn_blocking(move || {
                                crate::config::get_password(&email)
                            })
                            .await;
                            match looked_up.map_err(anyhow::Error::from).and_then(|r| r) {
                                Ok(p) => {
                                    resolved = Some(p.clone());
                                    p
                                }
                                Err(e) => {
                                    let _ = tx.send(TaskMsg::Done(TaskDone::Swept {
                                        acct_idx,
                                        client: None,
                                        password: None,
                                        kind,
                                        sweep: Box::default(),
                                        result: Err(e),
                                    }));
                                    return;
                                }
                            }
                        }
                    };
                    match ImapClient::connect(&cfg, &password).await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(TaskMsg::Done(TaskDone::Swept {
                                acct_idx,
                                client: None,
                                password: resolved,
                                kind,
                                sweep: Box::default(),
                                result: Err(e),
                            }));
                            return;
                        }
                    }
                }
            };
            let (sweep, result) = imap_client::sweep(&mut client, back, |p| {
                // the only feedback a sweep gives: the list itself does not
                // move until the window is complete (ADR 0003)
                let _ = tx.send(TaskMsg::Sweeping(p));
            })
            .await;
            // after a timeout the session state is unknown, so the client goes;
            // a server refusal leaves the connection perfectly usable
            let dead = result.as_ref().err().is_some_and(imap_client::is_timeout);
            let client = (!dead).then(|| Box::new(client));
            let _ = tx.send(TaskMsg::Done(TaskDone::Swept {
                acct_idx,
                client,
                password: resolved,
                kind,
                sweep: Box::new(sweep),
                result,
            }));
        });
    }

    pub fn handle_event(&mut self, ev: Event) {
        let Event::Key(key) = ev else { return };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return;
        }
        // The TUI is inert for a sweep's whole length (ADR 0001). This is
        // wider than the `busy` gate below: that one only has to keep an
        // action's stack indices valid, so it still allows scrolling and help.
        // A sweep refuses those too — on a widen the previous window is still
        // on screen, and letting the cursor roam a list that is about to be
        // rebuilt under it is the silent retargeting ADR 0003 rules out.
        // The refusal is stated in `status` for the whole sweep, not posted
        // per keypress, so nothing here writes to it.
        if self.loading {
            if is_quit(key) {
                self.should_quit = true;
            }
            return;
        }
        // The failed alert names two keys and swallows the rest, which is what
        // "any key to continue" means: whatever the user pressed, the box comes
        // down and that keypress is spent doing it. Letting the key through as
        // well would act on a list the user was not looking at.
        if matches!(self.alert, Some(Alert::Failed(_))) {
            self.alert = None;
            if is_quit(key) {
                self.should_quit = true;
            } else if key.code == KeyCode::Char('m') {
                self.load_more();
            }
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
                // Each account sweeps on the first `Tab` to it, not at launch:
                // with N accounts, sweeping all of them at startup multiplies
                // the wait the bound exists to protect (ADR 0002). Its window
                // is its own — a shared budget would make one account's window
                // depend on another's mail.
                if !self.account().visited {
                    self.spawn_batch(Load::Reset);
                } else if self.account().loaded {
                    // the status line is global: without this it keeps
                    // describing the account we just left
                    self.status = self.account_summary(self.active);
                } else {
                    // swept once and nothing landed. `Tab` does not try again
                    // — that is `R`, and it is named here rather than spent on
                    // every pass through the tab bar.
                    self.status = format!(
                        "{}: nothing swept — press R to try again",
                        self.account().cfg.email
                    );
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
                self.ask_before(FanFor::Trash, targets);
            }
            (KeyCode::Char('e'), _) => {
                let targets = self.target_stacks();
                self.ask_before(FanFor::Archive, targets);
            }
            (KeyCode::Char('r'), _) => {
                let targets = self.target_stacks();
                if !targets.is_empty() {
                    // marking read asks nothing and moves nothing out of the
                    // mailbox, so it has no count to state and stays on the
                    // mail the window is showing
                    let uids = self.window_uids(&targets);
                    let fan = FanOut {
                        in_view: uids.len(),
                        uids,
                        refused: Vec::new(),
                    };
                    self.spawn_imap(ImapKind::Read, targets, fan);
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
        // the fan-out already ran, ahead of the prompt: `y` acts on the set the
        // user was given a number for, and no second SEARCH happens (ADR 0002)
        match action {
            PendingAction::Trash { stack_idxs, fan }
            | PendingAction::TrashAfterUnsub { stack_idxs, fan } => {
                self.spawn_imap(ImapKind::Trash, stack_idxs, fan)
            }
            PendingAction::Archive { stack_idxs, fan } => {
                self.spawn_imap(ImapKind::Archive, stack_idxs, fan)
            }
            PendingAction::Unsubscribe { stack_idxs } => self.spawn_unsub(stack_idxs),
        }
    }

    /// Ask before acting, with the mailbox-wide number in the question. The
    /// SEARCH behind that number has to run first — it is the round trip the
    /// action needs anyway, moved ahead of the question (ADR 0002).
    ///
    /// Except under sender+subject grouping, where the row is one thread of a
    /// sender rather than the sender. `SEARCH` has no key for the normalized
    /// subject the stack was grouped on, so a fan-out there would reach the
    /// sender's whole mailbox for a row that names a slice of it. The window's
    /// own mail is the set instead, and the prompt's two numbers agree because
    /// they are the same number.
    fn ask_before(&mut self, what: FanFor, stack_idxs: Vec<usize>) {
        if stack_idxs.is_empty() {
            return;
        }
        if self.group_by == GroupBy::SenderSubject {
            let uids = self.window_uids(&stack_idxs);
            let fan = FanOut {
                in_view: uids.len(),
                uids,
                refused: Vec::new(),
            };
            self.mode = Mode::Confirm(what.confirm(stack_idxs, fan));
            return;
        }
        self.spawn_fan_out(what, stack_idxs);
    }

    fn spawn_fan_out(&mut self, what: FanFor, stack_idxs: Vec<usize>) {
        // resolved before the client is taken, and before the task starts: the
        // stacks stay untouched while it runs, because mutating keys are
        // blocked when busy
        let acct = self.account();
        let senders: Vec<(String, Vec<u32>)> = stack_idxs
            .iter()
            .map(|&i| {
                let s = &acct.stacks[i];
                (s.latest().sender_email.clone(), s.uids())
            })
            .collect();
        let Some(mut client) = self.account_mut().client.take() else {
            self.status = "not connected — press R to reconnect".into();
            return;
        };
        let (tx, rx) = mpsc::unbounded_channel();
        self.task_rx = Some(rx);
        self.busy = true;
        self.status = match &senders[..] {
            [(addr, _)] => format!("searching for every message from {addr}…"),
            many => format!("searching for every message from {} senders…", many.len()),
        };
        tokio::spawn(async move {
            let mut fan = FanOut::default();
            // the window's own mail is in the set whatever the search says, so
            // an action can never reach less than what the user is looking at
            let mut uids: HashSet<u32> = HashSet::new();
            let mut in_view: HashSet<u32> = HashSet::new();
            let mut result = Ok(());
            let total = senders.len();
            for (done, (addr, window_uids)) in senders.into_iter().enumerate() {
                if total > 1 {
                    // one SEARCH per sender, so a bulk action's wait grows with
                    // the marks — the row says which one it is on
                    let _ = tx.send(TaskMsg::Status(format!(
                        "searching {}/{total}: {addr}…",
                        done + 1
                    )));
                }
                in_view.extend(window_uids.iter().copied());
                uids.extend(window_uids);
                match imap_client::fan_out(&mut client, &addr).await {
                    Ok(found) => uids.extend(found),
                    // the session state is unknown after a timeout, so there is
                    // nothing left to search with — the rest of the senders
                    // would only produce more of the same error
                    Err(e) if imap_client::is_timeout(&e) => {
                        result = Err(e);
                        break;
                    }
                    Err(e) => {
                        crate::debuglog::write(format!("imap fan-out refused {addr}: {e:#}"));
                        fan.refused.push(addr);
                    }
                }
            }
            fan.uids = uids.into_iter().collect();
            fan.in_view = in_view.len();
            let dead = result.as_ref().err().is_some_and(imap_client::is_timeout);
            let client = (!dead).then(|| Box::new(client));
            let _ = tx.send(TaskMsg::Done(TaskDone::Fanned {
                client,
                what,
                stack_idxs,
                fan,
                result,
            }));
        });
    }

    /// Run `kind` over the set the fan-out produced. Every caller has one: the
    /// keys that ask get theirs from a SEARCH, and `r`, which asks nothing,
    /// builds one from the window.
    fn spawn_imap(&mut self, kind: ImapKind, stack_idxs: Vec<usize>, fan: FanOut) {
        let label = self.action_label(&stack_idxs);
        let FanOut { uids, refused, .. } = fan;
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
                refused,
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

    /// one chunk of the running sweep landed
    pub fn on_sweep_progress(&mut self, p: SweepProgress) {
        if let Some(Alert::Sweeping { progress, .. }) = &mut self.alert {
            *progress = Some(p);
        }
    }

    /// apply a finished task's outcome to app state (runs on the event loop)
    pub fn on_task_done(&mut self, done: TaskDone) {
        self.busy = false;
        self.loading = false;
        self.task_rx = None;
        // whatever raised it is over; a failed sweep puts its own back up below
        self.alert = None;
        match done {
            TaskDone::Fanned {
                client,
                what,
                stack_idxs,
                fan,
                result,
            } => {
                self.account_mut().client = client.map(|c| *c);
                match result {
                    // nothing was asked, so nothing is confirmed: the search
                    // that would have produced the number never came back
                    Err(e) => self.status = format!("error: {e:#}"),
                    Ok(()) => {
                        // said now rather than after `y`, because it changes
                        // what answering `y` costs — and the status row is
                        // still on screen behind the prompt
                        self.status = match partial_note(&fan.refused) {
                            note if note.is_empty() => self.account_summary(self.active),
                            note => format!("this action{note}"),
                        };
                        self.mode = Mode::Confirm(what.confirm(stack_idxs, fan));
                    }
                }
            }
            TaskDone::Imap {
                client,
                kind,
                stack_idxs,
                n_msgs,
                label,
                refused,
                result,
            } => match result {
                Ok(()) => {
                    self.account_mut().client = Some(*client);
                    let short = partial_note(&refused);
                    match kind {
                        ImapKind::Trash => {
                            self.log_action(Action::Trash, &stack_idxs);
                            self.prune_window(&stack_idxs, n_msgs);
                            self.remove_stacks(stack_idxs);
                            self.stats.trashed += n_msgs;
                            self.status = format!("trashed {n_msgs} messages from {label}{short}");
                        }
                        ImapKind::Archive => {
                            self.log_action(Action::Archive, &stack_idxs);
                            self.prune_window(&stack_idxs, n_msgs);
                            self.remove_stacks(stack_idxs);
                            self.stats.archived += n_msgs;
                            self.status = format!("archived {n_msgs} messages from {label}{short}");
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
                    // chain into "also trash?" — which states a mailbox-wide
                    // count like any other trash, so it fans out first
                    self.spawn_fan_out(FanFor::TrashAfterUnsub, stack_idxs);
                }
            }
            TaskDone::Swept {
                acct_idx,
                client,
                password,
                kind,
                mut sweep,
                result,
            } => {
                let (group_by, sort_by) = (self.group_by, self.sort_by);
                // The user pressed `m` with a stack in mind, and the
                // completion sort can move it anywhere. Held as a row index it
                // would retarget the cursor silently, which matters the moment
                // the next key is `d` (ADR 0003).
                let was_on = self.selected_key(acct_idx);
                let acct = &mut self.accounts[acct_idx];
                acct.client = client.map(|c| *c);
                if password.is_some() {
                    acct.password = password;
                }
                // A sweep that never reached its `EXISTS` — a failed connect or
                // keychain read — knows nothing about the mailbox, and must not
                // overwrite what the last one learned with zeroes.
                if sweep.anchored {
                    // the window lands either way: a failure does not un-read
                    // the chunks that arrived, and dropping them would make the
                    // next `m` re-read them. A short window is one the sweep
                    // stopped inside, so `back` moves by what was actually
                    // swept, not by the bound.
                    acct.total = sweep.total;
                    acct.back += sweep.swept;
                    acct.reached_end = sweep.reached_end;
                    acct.loaded = true;
                    if sweep.swept > 0 {
                        acct.absorb(std::mem::take(&mut sweep.msgs), group_by, sort_by);
                    }
                    if kind == Load::Reset {
                        // a reset discards the window, so there is no stack
                        // left to follow — the cursor goes home
                        self.accounts[acct_idx].selected = 0;
                    } else {
                        self.follow_selection(acct_idx, was_on);
                    }
                }
                match result {
                    Ok(()) => {
                        self.status = self.account_summary(acct_idx);
                    }
                    // the stacks that landed stay on screen: a failure halfway
                    // through the window is not a reason to throw away the
                    // half that worked. `back` stopped where the sweep did, so
                    // `m` retries the remainder of this window before it
                    // advances (ADR 0003).
                    // `anchored` as well as `short`, because "stopped at 0 of
                    // 5,000" would be a claim about a mailbox this sweep never
                    // counted. Today an unanchored sweep also has `bound == 0`
                    // and so is never short, but that is the task's choice of
                    // `Sweep::default()`, not something this arm should rest on.
                    Err(e) if sweep.anchored && sweep.short() => {
                        // the count goes in the alert, where it cannot be
                        // missed; the reason stays in the status row, which is
                        // still there once the alert is dismissed
                        self.alert = Some(Alert::Failed(format!(
                            "stopped at {} of {}",
                            commas(sweep.swept),
                            commas(sweep.bound)
                        )));
                        self.status = format!(
                            "sweep stopped at {} of {} — press m to retry ({e:#})",
                            commas(sweep.swept),
                            commas(sweep.bound)
                        )
                    }
                    // a sweep that never anchored has no count to report, so
                    // the alert carries the reason itself
                    Err(e) => {
                        self.alert = Some(Alert::Failed(format!("{e:#}")));
                        self.status = format!("error: {e:#}");
                    }
                }
            }
        }
    }

    /// advance the status-bar spinner one frame
    pub fn tick_spinner(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    /// the mail these stacks hold inside the window — what the user is looking
    /// at. An action's own set comes from the fan-out instead.
    fn window_uids(&self, stack_idxs: &[usize]) -> Vec<u32> {
        let acct = self.account();
        stack_idxs
            .iter()
            .flat_map(|&i| acct.stacks[i].uids())
            .collect()
    }

    /// who an action is about, for the status line
    fn action_label(&self, stack_idxs: &[usize]) -> String {
        whose(self.account(), stack_idxs)
    }

    /// Account for the `gone` messages that just left INBOX. All of them are
    /// out of the mailbox, so the total falls by all of them — but only the
    /// ones the window was showing move its reach, because expunging mail
    /// older than the window does not shift what sits above it. Without the
    /// second, the next `m` would start that many messages too deep and leave
    /// a hole where the trashed mail used to sit.
    fn prune_window(&mut self, stack_idxs: &[usize], gone: usize) {
        let in_window: HashSet<u32> = self.window_uids(stack_idxs).into_iter().collect();
        let acct = self.account_mut();
        acct.total = acct.total.saturating_sub(gone);
        acct.back = acct.back.saturating_sub(in_window.len());
    }

    fn remove_stacks(&mut self, mut stack_idxs: Vec<usize>) {
        stack_idxs.sort_unstable();
        stack_idxs.dedup();
        let acct = self.account_mut();
        for &i in stack_idxs.iter().rev() {
            acct.stacks.remove(i);
        }
        acct.marked.clear();
        acct.clamp_selection();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap_client::Sweep;
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
        app.accounts[0].total = 4;
        app.accounts[0].back = 4;
        app.accounts[0].reached_end = true;
        app.accounts[0].visited = true;
        app.accounts[0].loaded = true;
        app
    }

    /// a fan-out that found `uids`, `in_view` of them inside the window
    fn fanned(uids: &[u32], in_view: usize) -> FanOut {
        FanOut {
            uids: uids.to_vec(),
            in_view,
            refused: Vec::new(),
        }
    }

    /// a window that read `swept` of `bound` and came back with `msgs`
    fn window(msgs: Vec<MsgMeta>, total: usize, bound: usize, swept: usize) -> Box<Sweep> {
        Box::new(Sweep {
            msgs,
            total,
            anchored: true,
            bound,
            swept,
            reached_end: swept == total,
        })
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
        // the "also trash?" offer states a mailbox-wide count like any other
        // trash, so it waits on a fan-out rather than opening blind. There is
        // no session in a unit test, so the search cannot be spawned — what is
        // asserted is that nothing was confirmed without a number behind it.
        assert!(!matches!(app.mode, Mode::Confirm(_)));
        assert!(app.status.contains("not connected"), "{}", app.status);
        let raw = std::fs::read_to_string(&path).unwrap();
        let recs: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(recs.len(), 1); // only the successful unsub is logged
        assert_eq!(recs[0]["action"], "unsub");
        std::fs::remove_file(&path).unwrap();
    }

    /// ADR 0003: nothing renders until the window completes, and the list is
    /// grouped and sorted once, from everything the sweep read.
    #[test]
    fn a_completed_sweep_builds_the_whole_list_at_once() {
        let path = temp_log("sweep-ok");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].stacks.clear();
        app.accounts[0].loaded = false;
        app.accounts[0].total = 0;
        app.accounts[0].back = 0;
        app.accounts[0].reached_end = false;
        app.busy = true;

        // b@ has one message and a@ two, handed over in that order: a sorted
        // list proves the completion sort ran rather than arrival order
        // surviving
        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::Reset,
            sweep: window(
                vec![
                    msg(3, "b@x.com", "one"),
                    msg(1, "a@x.com", "one"),
                    msg(2, "a@x.com", "two"),
                ],
                4,
                4,
                4,
            ),
            result: Ok(()),
        });

        assert!(!app.busy);
        assert!(app.task_rx.is_none());
        assert!(app.accounts[0].loaded);
        assert_eq!(app.accounts[0].inbox_total(), 4, "the total is EXISTS");
        assert_eq!(app.accounts[0].back, 4, "the window reaches what it swept");
        assert!(
            app.accounts[0].exhausted(),
            "the sweep hit the oldest message"
        );
        let keys: Vec<&str> = app.accounts[0]
            .stacks
            .iter()
            .map(|s| s.key.as_str())
            .collect();
        assert_eq!(keys, ["a@x.com", "b@x.com"], "sorted by count");
        assert!(
            app.status.contains("3 of 4 messages"),
            "status: {}",
            app.status
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `m` widens the window, and the list is rebuilt from every window the
    /// session has read — one grouping pass over the whole store, not a merge
    #[test]
    fn m_folds_the_new_window_into_the_stacks_already_on_screen() {
        let path = temp_log("sweep-more");
        let mut app = test_app(ActionLog::at(path.clone()));

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            // one more from a@, and a sender nobody has seen yet
            sweep: window(
                vec![msg(5, "a@x.com", "three"), msg(9, "c@x.com", "hi")],
                9,
                2,
                2,
            ),
            result: Ok(()),
        });

        let acct = &app.accounts[0];
        assert_eq!(acct.back, 6, "the window reaches four deeper than before");
        let counts: Vec<(&str, usize)> = acct
            .stacks
            .iter()
            .map(|s| (s.key.as_str(), s.msgs.len()))
            .collect();
        assert_eq!(
            counts,
            [("a@x.com", 3), ("b@x.com", 2), ("c@x.com", 1)],
            "a@ grew inside its own stack and the list re-sorted"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Windows overlap when mail arrives between two sweeps, so the same
    /// message can be read twice. The store is keyed by UID, so a repeat must
    /// not become a second copy inside the stack.
    #[test]
    fn a_message_swept_twice_lands_in_the_stack_once() {
        let path = temp_log("sweep-dupe");
        let mut app = test_app(ActionLog::at(path.clone()));

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(vec![msg(1, "a@x.com", "one")], 4, 1, 1),
            result: Ok(()),
        });

        let a = stack_idx(&app, "a@x.com");
        assert_eq!(
            app.accounts[0].stacks[a].msgs.len(),
            2,
            "still uids 1 and 2"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Spec: a refused chunk keeps the chunks that landed and reports a short
    /// window. `back` stops where the sweep did, so `m` retries the remainder
    /// of that window before advancing (ADR 0003).
    #[test]
    fn a_short_window_keeps_its_stacks_and_leaves_the_remainder_for_m() {
        let path = temp_log("sweep-short");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].back = 0;
        app.accounts[0].reached_end = false;
        app.busy = true;

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(vec![msg(9, "c@x.com", "hi")], 5_000, 5_000, 2_400),
            result: Err(anyhow::anyhow!("NO [CANNOT] fetch")),
        });

        assert!(!app.busy);
        let acct = &app.accounts[0];
        assert_eq!(acct.back, 2_400, "m picks up where the sweep stopped");
        assert!(!acct.exhausted(), "there is more mailbox behind the hole");
        assert_eq!(acct.stacks.len(), 3, "the stacks that landed stay");
        assert!(
            app.status.contains("stopped at 2,400 of 5,000"),
            "status: {}",
            app.status
        );
        let _ = std::fs::remove_file(&path);
    }

    /// a refusal on the very first chunk is still a short window, and the
    /// count is the thing the user needs told
    #[test]
    fn a_window_that_lost_every_chunk_still_reports_how_far_it_got() {
        let path = temp_log("sweep-none");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].back = 0;
        app.accounts[0].reached_end = false;

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(vec![], 5_000, 5_000, 0),
            result: Err(anyhow::anyhow!("NO [CANNOT] fetch")),
        });

        assert_eq!(app.accounts[0].back, 0, "nothing was swept");
        assert!(
            app.status.contains("stopped at 0 of 5,000"),
            "status: {}",
            app.status
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `m` grows stacks that are already on screen, so a snapshot taken before
    /// the widening would log a keep for fewer messages than the user kept
    #[test]
    fn a_stack_that_grew_under_a_widened_window_is_re_snapshotted() {
        let path = temp_log("seen-grew");
        let mut app = test_app(ActionLog::at(path.clone()));
        let a = stack_idx(&app, "a@x.com");
        app.record_seen(&[a]);

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(vec![msg(5, "a@x.com", "three")], 9, 1, 1),
            result: Ok(()),
        });
        let a = stack_idx(&app, "a@x.com");
        app.record_seen(&[a]);
        app.flush_keeps();

        let recs = read_records(&path);
        let keep = recs.iter().find(|r| r["sender"] == "a@x.com").unwrap();
        assert_eq!(keep["action"], "keep");
        assert_eq!(keep["count"], 3, "the keep counts the widened stack");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_sweep_that_never_started_clears_busy_and_keeps_the_stacks() {
        let path = temp_log("sweep-err");
        let mut app = test_app(ActionLog::at(path.clone()));
        let before = app.accounts[0].stacks.len();
        app.busy = true;
        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::Reset,
            sweep: Box::default(),
            result: Err(anyhow::anyhow!("boom")),
        });
        assert!(!app.busy);
        assert!(app.accounts[0].client.is_none());
        assert_eq!(app.accounts[0].stacks.len(), before);
        assert_eq!(
            app.accounts[0].total, 4,
            "a sweep that never anchored does not blank the mailbox total"
        );
        assert!(
            app.accounts[0].reached_end,
            "nor does it claim there is more mailbox behind the window"
        );
        assert_eq!(app.accounts[0].back, 4, "nor does it move the window");
        assert!(app.status.starts_with("error:"), "status: {}", app.status);
        let _ = std::fs::remove_file(&path);
    }

    /// Trashed mail leaves the mailbox, so the window's own reach shrinks with
    /// it. Without that, the next `m` would start that many messages too deep
    /// and skip the mail that slid up into the hole.
    ///
    /// The two numbers move by different amounts: an action reaches the whole
    /// fan-out, but only the part inside the window sits above the window's
    /// oldest edge, and only that part shifts it.
    #[test]
    fn trashing_pulls_the_window_back_by_what_left_it_and_the_total_by_all_of_it() {
        let path = temp_log("prune");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].total = 137_482;
        app.accounts[0].back = 5_000;
        // "a@x.com" holds two of the swept messages; the fan-out found 400
        let a = stack_idx(&app, "a@x.com");
        app.prune_window(&[a], 400);

        assert_eq!(app.accounts[0].total, 137_082, "all 400 left the mailbox");
        assert_eq!(app.accounts[0].back, 4_998, "two of them were in the window");
        let _ = std::fs::remove_file(&path);
    }

    /// A socket that died mid-sweep leaves a window whose size nobody can know,
    /// so `R` does not widen or repair — it throws the window away and sweeps a
    /// fresh one from the top, inert like any other sweep (ADR 0002).
    #[tokio::test]
    async fn r_discards_the_window_and_sweeps_it_again_from_the_top() {
        let path = temp_log("r-resweep");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].marked.insert("a@x.com".into());
        app.accounts[0].selected = 1;

        press(&mut app, KeyCode::Char('R'));

        let acct = &app.accounts[0];
        assert!(acct.stacks.is_empty(), "the window was discarded");
        assert_eq!(acct.back, 0, "and the next sweep starts at the top");
        assert_eq!(acct.total, 0);
        assert!(!acct.reached_end, "nothing is known about the mailbox yet");
        assert!(!acct.loaded);
        assert_eq!(acct.selected, 0);
        assert!(acct.marked.is_empty());
        assert!(app.loading, "and it is an inert-TUI event like any other sweep");
        assert!(matches!(app.alert, Some(Alert::Sweeping { .. })));
        assert!(acct.client.is_none(), "and it dials again rather than reusing");
        let _ = std::fs::remove_file(&path);
    }

    /// `R` is the reconnect key, so the one thing it must not do is keep a
    /// session whose death is the reason it was pressed. `m` is mid-triage on
    /// a connection that is answering, and re-dialling for every widen would
    /// put a login in front of a key the user presses often.
    #[test]
    fn only_a_widen_carries_on_with_the_session_it_was_handed() {
        assert!(reuses_session(Load::More));
        assert!(!reuses_session(Load::Reset));
    }

    #[test]
    fn m_at_the_end_of_the_mailbox_says_so_instead_of_reconnecting() {
        let path = temp_log("m-exhausted");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.handle_normal(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(app.task_rx.is_none(), "no sweep was spawned");
        assert!(app.status.contains("no more messages"), "{}", app.status);
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

    #[test]
    fn spinner_advances_a_frame_at_a_time() {
        let path = temp_log("spinner");
        let mut app = test_app(ActionLog::at(path.clone()));
        let before = app.spinner;
        app.tick_spinner();
        assert_eq!(app.spinner, before + 1);
        let _ = std::fs::remove_file(&path);
    }

    fn account(email: &str) -> AccountView {
        AccountView::new(AccountConfig {
            name: email.into(),
            email: email.into(),
            imap_host: "imap".into(),
            smtp_host: "smtp".into(),
        })
    }

    /// an account that has been swept and has mail in its window
    fn swept_account(email: &str) -> AccountView {
        let mut acct = account(email);
        acct.stacks = build_stacks(
            vec![msg(9, "c@x.com", "hi")],
            GroupBy::Sender,
            SortBy::Count,
        );
        acct.total = 1;
        acct.back = 1;
        acct.visited = true;
        acct.loaded = true;
        acct
    }

    /// the status line is shared by every account, so switching to one that
    /// needs no fetch must still rewrite it
    #[test]
    fn tab_to_a_loaded_account_restates_its_summary() {
        let path = temp_log("tab-status");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts.push(swept_account("other@x.com"));
        app.status = "me@x.com: 4 of 4 messages in 2 stacks".into();

        press(&mut app, KeyCode::Tab);

        assert_eq!(app.active, 1);
        assert_eq!(app.status, "other@x.com: 1 of 1 messages in 1 stacks");
        let _ = std::fs::remove_file(&path);
    }

    /// ADR 0002: with N accounts, sweeping all of them at startup multiplies
    /// the wait the bound exists to protect. Only the account on screen sweeps.
    #[tokio::test]
    async fn launching_with_three_accounts_costs_one_sweep() {
        let path = temp_log("tab-launch");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0] = account("me@x.com");
        app.accounts.push(account("two@x.com"));
        app.accounts.push(account("three@x.com"));

        // what main.rs does before the event loop
        app.spawn_batch(Load::Reset);

        assert!(app.accounts[0].visited, "the account on screen sweeps");
        assert!(
            app.accounts[1..].iter().all(|a| !a.visited),
            "the other two wait for the Tab that shows them"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// each account's window is its own — a shared budget would make one
    /// account's window depend on another's mail
    #[tokio::test]
    async fn tab_sweeps_a_never_visited_account_once_and_not_again() {
        let path = temp_log("tab-once");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts.push(account("other@x.com"));

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, 1);
        assert!(app.loading, "the first Tab to it sweeps, inert like any sweep");
        assert!(app.accounts[1].visited);

        // its window lands, its own 5,000 rather than a share of anything
        app.on_task_done(TaskDone::Swept {
            acct_idx: 1,
            client: None,
            password: None,
            kind: Load::Reset,
            sweep: window(vec![msg(9, "c@x.com", "hi")], 20_000, 5_000, 5_000),
            result: Ok(()),
        });
        assert_eq!(app.accounts[1].window(), 5_000);
        assert_eq!(app.accounts[0].window(), 4, "account 0's is untouched");

        press(&mut app, KeyCode::Tab); // back to 0
        press(&mut app, KeyCode::Tab); // and to 1 again
        assert_eq!(app.active, 1);
        assert!(app.task_rx.is_none(), "tabbing back and forth re-swept it");
        assert!(!app.loading);
        let _ = std::fs::remove_file(&path);
    }

    /// `Tab` is a navigation key. An account whose sweep found nothing — a bad
    /// password, a dead host — would otherwise hold the TUI inert for the
    /// connect timeout on every pass through the tab bar. `R` is the retry, and
    /// the row that says so is what `Tab` leaves behind instead.
    #[tokio::test]
    async fn tab_does_not_re_sweep_an_account_whose_sweep_landed_nothing() {
        let path = temp_log("tab-failed");
        let mut app = test_app(ActionLog::at(path.clone()));
        let mut failed = account("other@x.com");
        failed.visited = true;
        app.accounts.push(failed);

        press(&mut app, KeyCode::Tab);

        assert_eq!(app.active, 1);
        assert!(app.task_rx.is_none(), "no sweep was spawned");
        assert!(!app.loading);
        assert!(app.status.contains("press R"), "{}", app.status);

        // and `R` is that retry
        press(&mut app, KeyCode::Char('R'));
        assert!(app.loading);
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

    fn send(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.handle_event(Event::Key(KeyEvent::new(code, mods)));
    }

    /// ADR 0001: the TUI is inert for the sweep's whole length. Even the keys
    /// an action leaves live — scrolling, help — are refused: nothing renders
    /// until the window completes, so there is no list to move under them.
    #[test]
    fn every_key_but_quit_is_refused_while_a_sweep_runs() {
        let path = temp_log("sweep-inert");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].selected = 1;
        app.busy = true;
        app.loading = true;
        // whatever the sweep last posted; the refused keys must not touch it
        let progress = app.status.clone();

        for code in [
            KeyCode::Char('k'),
            KeyCode::Char('j'),
            KeyCode::Home,
            KeyCode::Char('?'),
            KeyCode::Char('g'),
            KeyCode::Char('s'),
            KeyCode::Char('/'),
            KeyCode::Char(' '),
            KeyCode::Char('d'),
            KeyCode::Char('m'),
            KeyCode::Char('R'),
            KeyCode::Tab,
        ] {
            send(&mut app, code, KeyModifiers::NONE);
        }

        assert!(!app.should_quit);
        assert_eq!(app.accounts[0].selected, 1, "the cursor did not move");
        assert!(matches!(app.mode, Mode::Normal), "no mode was entered");
        assert_eq!(app.group_by, GroupBy::Sender);
        assert_eq!(app.sort_by, SortBy::Count);
        assert!(app.account().marked.is_empty());
        assert!(app.task_rx.is_none(), "no second sweep was spawned");
        assert_eq!(
            app.status, progress,
            "the sweep's own line survives the refused keys"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// ADR 0001: the refusal is said plainly rather than swallowed in silence,
    /// and ADR 0004 puts it on the alert's hint line, standing for the sweep's
    /// whole length. The status row keeps a plain line for what shows behind
    /// the box and once it comes down.
    // `spawn_batch` puts the sweep on the runtime; the task never runs here,
    // the state it sets on the way out is the whole assertion
    #[tokio::test]
    async fn a_running_sweep_raises_the_alert_that_names_the_live_keys() {
        let path = temp_log("sweep-hint");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].loaded = false;
        app.spawn_batch(Load::Reset);
        assert!(app.loading);
        assert!(matches!(app.alert, Some(Alert::Sweeping { .. })));
        assert!(
            !app.status.contains("q quit"),
            "the hint line owns the refusal now: {}",
            app.status
        );
        let _ = std::fs::remove_file(&path);
    }

    /// the two keys that stay live: a sweep is read-only, so quitting mid-way
    /// abandons nothing that needs logging
    #[test]
    fn q_and_ctrl_c_still_quit_during_a_sweep() {
        let path = temp_log("sweep-quit");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.loading = true;
        send(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.should_quit);

        let mut app = test_app(ActionLog::at(path.clone()));
        app.loading = true;
        send(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
        let _ = std::fs::remove_file(&path);
    }

    /// ADR 0003: selection follows the stack it was on, by key, across the
    /// completion sort — holding the row index instead retargets the cursor
    /// silently, which matters the moment the next key is `d`.
    #[test]
    fn selection_follows_its_stack_by_key_across_the_completion_sort() {
        let path = temp_log("sweep-sel");
        let mut app = test_app(ActionLog::at(path.clone()));
        let a = stack_idx(&app, "a@x.com");
        app.accounts[0].selected = a;

        // b@ overtakes a@ on count, so the row a@ sits on is no longer a@'s
        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(
                vec![
                    msg(6, "b@x.com", "three"),
                    msg(7, "b@x.com", "four"),
                    msg(8, "c@x.com", "hi"),
                ],
                9,
                3,
                3,
            ),
            result: Ok(()),
        });

        let keys: Vec<&str> = app.accounts[0]
            .stacks
            .iter()
            .map(|s| s.key.as_str())
            .collect();
        assert_eq!(keys, ["b@x.com", "a@x.com", "c@x.com"], "the sort moved a@");
        assert_eq!(
            app.accounts[0].selected,
            stack_idx(&app, "a@x.com"),
            "the cursor followed a@ rather than holding its row"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `selected` is a row in the *filtered* list, not an index into `stacks`.
    /// A filter survives `m` — set it, `Esc` back to Normal, widen — so the
    /// follow has to read and write the selection in that space or it lands on
    /// whatever stack happens to sit at that absolute index.
    #[test]
    fn selection_follows_its_stack_by_key_under_a_filter_too() {
        let path = temp_log("sweep-sel-filter");
        let mut app = test_app(ActionLog::at(path.clone()));
        // "x.com" matches every sender, so the filtered list is the whole list
        // shifted only by the sort — and row 1 is a@ before the widen
        app.filter = "b@".into();
        app.accounts[0].selected = 0;

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            // c@ outranks both, so b@ moves down the unfiltered list
            sweep: window(
                vec![
                    msg(6, "c@x.com", "one"),
                    msg(7, "c@x.com", "two"),
                    msg(8, "c@x.com", "three"),
                ],
                7,
                3,
                3,
            ),
            result: Ok(()),
        });

        assert_eq!(
            stack_idx(&app, "b@x.com"),
            2,
            "b@ is the last stack overall"
        );
        assert_eq!(
            app.accounts[0].selected, 0,
            "but still the only filtered row, so the cursor stays on it"
        );
        assert_eq!(
            app.selected_stack_idx(),
            Some(2),
            "and resolves to b@, not to whatever sits at row 0"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A reset discards the window and sweeps from the top, so there is no
    /// stack to follow — the cursor goes home.
    #[test]
    fn a_reset_puts_the_selection_back_at_the_top() {
        let path = temp_log("sweep-sel-reset");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].selected = 1;

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::Reset,
            sweep: window(
                vec![msg(1, "a@x.com", "one"), msg(3, "b@x.com", "one")],
                2,
                2,
                2,
            ),
            result: Ok(()),
        });

        assert_eq!(app.accounts[0].selected, 0);
        let _ = std::fs::remove_file(&path);
    }

    /// ADR 0004: a sweep's progress is the alert's business. The status row
    /// keeps a standing line for what is behind the alert, but the counts the
    /// user watches arrive as numbers, not as a pre-formatted string.
    #[tokio::test]
    async fn a_sweep_reports_its_progress_into_the_alert_as_numbers() {
        let path = temp_log("alert-progress");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].loaded = false;
        app.spawn_batch(Load::Reset);

        let Some(Alert::Sweeping { starting, progress }) = &app.alert else {
            panic!("no sweeping alert: spawn_batch must raise one");
        };
        assert!(starting.contains("me@x.com"), "{starting:?}");
        assert!(progress.is_none(), "nothing has been swept yet");

        app.on_sweep_progress(SweepProgress {
            swept: 3_000,
            bound: 5_000,
            stacks: 41,
        });
        let Some(Alert::Sweeping {
            progress: Some(p), ..
        }) = &app.alert
        else {
            panic!("progress did not land on the alert");
        };
        assert_eq!((p.swept, p.bound, p.stacks), (3_000, 5_000, 41));
        let _ = std::fs::remove_file(&path);
    }

    /// A sweep that stopped has something to say and no way to say it once the
    /// alert comes down, so the alert stays up until a key dismisses it.
    #[test]
    fn a_short_window_leaves_a_failed_alert_up_until_a_key_dismisses_it() {
        let path = temp_log("alert-failed");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].back = 0;
        app.accounts[0].reached_end = false;
        app.loading = true;
        app.busy = true;

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(vec![], 5_000, 5_000, 2_400),
            result: Err(anyhow::anyhow!("NO [CANNOT] fetch")),
        });

        let Some(Alert::Failed(headline)) = &app.alert else {
            panic!("a stopped sweep must raise the alert, not only a status line");
        };
        assert_eq!(headline, "stopped at 2,400 of 5,000");

        // any key clears it, and does nothing else
        send(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(app.alert.is_none(), "the key dismissed the alert");
        assert_eq!(app.accounts[0].selected, 0, "and was swallowed doing it");
        let _ = std::fs::remove_file(&path);
    }

    /// `m retry` on the failed alert is the whole point of naming it there
    #[tokio::test]
    async fn m_on_a_failed_alert_retries_the_remainder_of_the_window() {
        let path = temp_log("alert-retry");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.accounts[0].reached_end = false;
        app.alert = Some(Alert::Failed("stopped at 2,400 of 5,000".into()));

        send(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);

        assert!(app.loading, "m spawned the retry");
        assert!(
            matches!(app.alert, Some(Alert::Sweeping { .. })),
            "and the alert moved on to the sweep it started"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// quitting is live in every state, the failed alert included
    #[test]
    fn q_still_quits_from_a_failed_alert() {
        let path = temp_log("alert-quit");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.alert = Some(Alert::Failed("stopped at 0 of 5,000".into()));
        send(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.should_quit);
        let _ = std::fs::remove_file(&path);
    }

    /// a sweep that lands cleanly has nothing left to say, so nothing stays up
    #[test]
    fn a_completed_sweep_takes_its_alert_down() {
        let path = temp_log("alert-done");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.alert = Some(Alert::Sweeping {
            starting: "sweeping me@x.com…".into(),
            progress: None,
        });

        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(vec![msg(9, "c@x.com", "hi")], 9, 1, 1),
            result: Ok(()),
        });

        assert!(app.alert.is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// a fan-out landing for `what`, with `uids` found and `in_view` on screen
    fn fan_landed(app: &mut App, what: FanFor, stack_idxs: Vec<usize>, fan: FanOut) {
        app.on_task_done(TaskDone::Fanned {
            client: None,
            what,
            stack_idxs,
            fan,
            result: Ok(()),
        });
    }

    /// ADR 0002: counts on screen are window counts, the number the user
    /// confirms is mailbox-wide, and the prompt is the only place the two are
    /// reconciled. Without both, a stack showing 12 silently deletes 400.
    #[test]
    fn the_confirm_states_the_mailbox_wide_count_and_the_one_in_view() {
        let path = temp_log("fan-prompt");
        let mut app = test_app(ActionLog::at(path.clone()));
        let a = stack_idx(&app, "a@x.com");

        fan_landed(
            &mut app,
            FanFor::Trash,
            vec![a],
            fanned(&(1..=400).collect::<Vec<u32>>(), 12),
        );

        let Mode::Confirm(action) = &app.mode else {
            panic!("a landed fan-out opens the confirm it was run for");
        };
        assert_eq!(
            action.prompt(app.account()),
            "trash 400 messages from a@x.com (12 in view)?"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The prompt cannot be drawn before the number in it exists, so `d` runs
    /// the search and only then asks. Pressing it opens nothing by itself.
    #[test]
    fn d_asks_nothing_until_the_search_behind_the_number_has_run() {
        let path = temp_log("fan-d");
        let mut app = test_app(ActionLog::at(path.clone()));

        press(&mut app, KeyCode::Char('d'));

        assert!(
            !matches!(app.mode, Mode::Confirm(_)),
            "no confirm without a count behind it"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A stack under sender+subject grouping is one thread of a sender, not
    /// the sender. Fanning it out would trash everything DoorDash ever sent
    /// for a row that names one order — so grouping this way asks about the
    /// mail on screen, and the two numbers agree because they are one number.
    #[test]
    fn a_subject_stack_acts_on_what_it_shows_rather_than_on_its_whole_sender() {
        let path = temp_log("fan-subject");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.regroup(GroupBy::SenderSubject);
        let one = app
            .account()
            .stacks
            .iter()
            .position(|s| s.key.starts_with("a@x.com"))
            .unwrap();
        app.account_mut().selected = one;

        press(&mut app, KeyCode::Char('d'));

        assert!(app.task_rx.is_none(), "no SEARCH was spawned");
        let Mode::Confirm(action) = &app.mode else {
            panic!("the confirm opens straight away — there is nothing to look up");
        };
        assert_eq!(
            action.prompt(app.account()),
            "trash 1 message from a@x.com (1 in view)?"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A timeout leaves the session unknown, so the number the prompt would
    /// have stated never arrived — there is nothing to confirm.
    #[test]
    fn a_fan_out_that_never_came_back_asks_nothing() {
        let path = temp_log("fan-timeout");
        let mut app = test_app(ActionLog::at(path.clone()));
        app.busy = true;

        app.on_task_done(TaskDone::Fanned {
            client: None,
            what: FanFor::Trash,
            stack_idxs: vec![0],
            fan: FanOut::default(),
            result: Err(anyhow::anyhow!("imap search timed out after 60s")),
        });

        assert!(!app.busy);
        assert!(matches!(app.mode, Mode::Normal), "nothing was asked");
        assert!(app.status.starts_with("error:"), "{}", app.status);
        assert!(app.accounts[0].client.is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// The fan-out is the round trip the action needs anyway, moved ahead of
    /// the question — so answering `y` spends no second search, and answering
    /// `n` has already spent the only one.
    #[test]
    fn declining_costs_the_one_search_and_nothing_else() {
        let path = temp_log("fan-decline");
        let mut app = test_app(ActionLog::at(path.clone()));
        let a = stack_idx(&app, "a@x.com");
        let before = app.accounts[0].stacks.len();

        fan_landed(&mut app, FanFor::Trash, vec![a], fanned(&[1, 2, 7], 2));
        send(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);

        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.task_rx.is_none(), "nothing was spawned");
        assert_eq!(app.accounts[0].stacks.len(), before);
        assert_eq!(app.status, "cancelled");
        let _ = std::fs::remove_file(&path);
    }

    /// Partial is an action-time error now: the SEARCH was refused, so the
    /// action reaches only that sender's mail in view. It is never a state the
    /// stack sits in, and the row it came from carries no mark of it.
    #[test]
    fn a_refused_search_is_reported_against_the_action_not_the_stack() {
        let path = temp_log("fan-refused");
        let mut app = test_app(ActionLog::at(path.clone()));
        let a = stack_idx(&app, "a@x.com");
        let fan = FanOut {
            uids: vec![1, 2],
            in_view: 2,
            refused: vec!["a@x.com".into()],
        };

        fan_landed(&mut app, FanFor::Trash, vec![a], fan);

        assert!(
            app.status.contains("a@x.com's search was refused"),
            "the refusal is told before `y` is pressed, because it changes \
             what `y` costs: {}",
            app.status
        );
        assert!(
            matches!(app.mode, Mode::Confirm(_)),
            "and the action still runs, on the mail it can reach"
        );
        // and the outcome repeats it, because the status row will have moved
        // on by the time the action lands
        assert!(partial_note(&["a@x.com".into()]).contains("a@x.com"));
        assert!(partial_note(&["a@x.com".into()]).contains("only its mail in view"));
        assert_eq!(partial_note(&[]), "", "a clean action says nothing extra");
        let _ = std::fs::remove_file(&path);
    }

    /// A sender whose action partially failed reappears in a later window as
    /// an ordinary stack — the error already told the user at the time, and
    /// there is no cross-window memory of it to render.
    #[test]
    fn a_refusal_leaves_no_trace_on_the_stacks_it_touched() {
        let path = temp_log("fan-no-memory");
        let mut app = test_app(ActionLog::at(path.clone()));
        let a = stack_idx(&app, "a@x.com");
        let fan = FanOut {
            uids: vec![1, 2],
            in_view: 2,
            refused: vec!["a@x.com".into()],
        };
        fan_landed(&mut app, FanFor::Trash, vec![a], fan);

        // a later window folds a@ back in like any other sender
        app.mode = Mode::Normal;
        app.on_task_done(TaskDone::Swept {
            acct_idx: 0,
            client: None,
            password: None,
            kind: Load::More,
            sweep: window(vec![msg(5, "a@x.com", "three")], 9, 1, 1),
            result: Ok(()),
        });

        let a = stack_idx(&app, "a@x.com");
        assert_eq!(app.accounts[0].stacks[a].msgs.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    /// the hint line answers `[y/n]`, so the prompt saying it too is the UI
    /// stating the same thing twice in one box
    #[test]
    fn the_confirm_prompt_leaves_the_keys_to_the_hint_line() {
        let path = temp_log("alert-prompt");
        let app = test_app(ActionLog::at(path.clone()));
        let prompt = PendingAction::Trash {
            stack_idxs: vec![0],
            fan: fanned(&[1, 2], 2),
        }
        .prompt(app.account());
        assert!(!prompt.contains("[y/n]"), "{prompt:?}");
        assert!(prompt.ends_with('?'), "{prompt:?}");
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

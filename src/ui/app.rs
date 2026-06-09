use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::config::AccountConfig;
use crate::imap_client::ImapClient;
use crate::stacks::{build_stacks, GroupBy, Stack};
use crate::unsubscribe::{self, Method};

pub enum Mode {
    Normal,
    /// pending action awaiting y/n
    Confirm(PendingAction),
    Filter,
}

#[derive(Debug, Clone)]
pub enum PendingAction {
    Trash { stack_idx: usize },
    Archive { stack_idx: usize },
    Unsubscribe { stack_idx: usize, method: Method },
    /// after a successful unsubscribe, offer to trash the stack too
    TrashAfterUnsub { stack_idx: usize },
}

impl PendingAction {
    pub fn prompt(&self, app: &AccountView) -> String {
        let stack = |i: usize| -> String {
            let s = &app.stacks[i];
            format!("{} ({} msgs)", s.display_name, s.msgs.len())
        };
        match self {
            PendingAction::Trash { stack_idx } => {
                format!("Trash {}? [y/n]", stack(*stack_idx))
            }
            PendingAction::Archive { stack_idx } => {
                format!("Archive {}? [y/n]", stack(*stack_idx))
            }
            PendingAction::Unsubscribe { stack_idx, method } => {
                format!(
                    "Unsubscribe from {} via {}? [y/n]",
                    stack(*stack_idx),
                    method.describe()
                )
            }
            PendingAction::TrashAfterUnsub { stack_idx } => {
                format!("Done. Also trash {}? [y/n]", stack(*stack_idx))
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
        }
    }

    pub fn total_messages(&self) -> usize {
        self.stacks.iter().map(|s| s.msgs.len()).sum()
    }
}

pub struct App {
    pub accounts: Vec<AccountView>,
    pub active: usize,
    pub mode: Mode,
    pub group_by: GroupBy,
    pub filter: String,
    pub status: String,
    pub busy: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(accounts: Vec<AccountConfig>) -> Self {
        Self {
            accounts: accounts.into_iter().map(AccountView::new).collect(),
            active: 0,
            mode: Mode::Normal,
            group_by: GroupBy::Sender,
            filter: String::new(),
            status: String::from("loading…"),
            busy: false,
            should_quit: false,
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
        let acct = &mut self.accounts[active];
        let msgs = acct.client.as_mut().unwrap().fetch_inbox().await?;
        let n = msgs.len();
        acct.stacks = build_stacks(msgs, group_by);
        acct.selected = acct.selected.min(acct.stacks.len().saturating_sub(1));
        acct.expanded = false;
        acct.msg_selected = 0;
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
                let acct = self.account_mut();
                if acct.expanded {
                    acct.expanded = false;
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.account_mut().selected = 0;
                }
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
                    acct.stacks = build_stacks(msgs, group_by);
                    acct.selected = 0;
                    acct.expanded = false;
                    acct.msg_selected = 0;
                }
                self.status = format!("grouping by {}", group_by.label());
            }
            (KeyCode::Char('/'), _) => {
                self.mode = Mode::Filter;
                self.filter.clear();
            }
            (KeyCode::Char('d'), _) => {
                if let Some(i) = self.selected_stack_idx() {
                    self.mode = Mode::Confirm(PendingAction::Trash { stack_idx: i });
                }
            }
            (KeyCode::Char('e'), _) => {
                if let Some(i) = self.selected_stack_idx() {
                    self.mode = Mode::Confirm(PendingAction::Archive { stack_idx: i });
                }
            }
            (KeyCode::Char('r'), _) => {
                if let Some(i) = self.selected_stack_idx() {
                    self.mark_read(i).await;
                }
            }
            (KeyCode::Char('u'), _) => {
                if let Some(i) = self.selected_stack_idx() {
                    let method = self.account().stacks[i]
                        .unsubscribe_source()
                        .and_then(unsubscribe::pick_method);
                    match method {
                        Some(method) => {
                            self.mode = Mode::Confirm(PendingAction::Unsubscribe {
                                stack_idx: i,
                                method,
                            });
                        }
                        None => {
                            self.status = "no List-Unsubscribe header in this stack".into()
                        }
                    }
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
            PendingAction::Trash { stack_idx } => self.trash(stack_idx).await,
            PendingAction::Archive { stack_idx } => self.archive(stack_idx).await,
            PendingAction::TrashAfterUnsub { stack_idx } => self.trash(stack_idx).await,
            PendingAction::Unsubscribe { stack_idx, method } => {
                match self.unsubscribe(stack_idx, &method).await {
                    Ok(()) => {
                        // chain into "also trash?" prompt
                        self.mode =
                            Mode::Confirm(PendingAction::TrashAfterUnsub { stack_idx });
                        self.busy = false;
                        return;
                    }
                    Err(e) => Err(e),
                }
            }
        };
        if let Err(e) = result {
            self.status = format!("error: {e:#}");
            self.account_mut().client = None;
        }
        self.busy = false;
    }

    async fn trash(&mut self, stack_idx: usize) -> Result<()> {
        let uids = self.account().stacks[stack_idx].uids();
        let acct = self.account_mut();
        acct.client.as_mut().unwrap().trash(&uids).await?;
        let name = acct.stacks[stack_idx].display_name.clone();
        self.remove_stack(stack_idx);
        self.status = format!("trashed {} messages from {name}", uids.len());
        Ok(())
    }

    async fn archive(&mut self, stack_idx: usize) -> Result<()> {
        let uids = self.account().stacks[stack_idx].uids();
        let acct = self.account_mut();
        acct.client.as_mut().unwrap().archive(&uids).await?;
        let name = acct.stacks[stack_idx].display_name.clone();
        self.remove_stack(stack_idx);
        self.status = format!("archived {} messages from {name}", uids.len());
        Ok(())
    }

    async fn mark_read(&mut self, stack_idx: usize) {
        self.busy = true;
        let uids = self.account().stacks[stack_idx].uids();
        let acct = self.account_mut();
        let res = acct.client.as_mut().unwrap().mark_read(&uids).await;
        match res {
            Ok(()) => {
                let acct = self.account_mut();
                for m in &mut acct.stacks[stack_idx].msgs {
                    m.unread = false;
                }
                acct.stacks[stack_idx].unread_count = 0;
                self.status = format!("marked {} messages read", uids.len());
            }
            Err(e) => {
                self.status = format!("error: {e:#}");
                self.account_mut().client = None;
            }
        }
        self.busy = false;
    }

    async fn unsubscribe(&mut self, stack_idx: usize, method: &Method) -> Result<()> {
        let acct = self.account();
        let cfg = acct.cfg.clone();
        let password = acct.password.clone().unwrap_or_default();
        let msg = unsubscribe::execute(method, &cfg, &password).await?;
        self.status = format!("{}: {msg}", self.account().stacks[stack_idx].display_name);
        Ok(())
    }

    fn remove_stack(&mut self, stack_idx: usize) {
        let acct = self.account_mut();
        acct.stacks.remove(stack_idx);
        acct.expanded = false;
        acct.msg_selected = 0;
        if acct.selected >= acct.stacks.len() && acct.selected > 0 {
            acct.selected -= 1;
        }
    }
}

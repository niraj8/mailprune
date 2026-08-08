mod action_log;
mod config;
mod debuglog;
mod imap_client;
mod stacks;
mod ui;
mod unsubscribe;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("auth") => {
            let email = args
                .get(2)
                .map(String::to_owned)
                .unwrap_or_else(|| prompt("email: "));
            let password = rpassword::prompt_password(format!("app password for {email}: "))?;
            config::store_password(&email, &password)?;
            println!("stored in keychain (service \"mailprune\", account {email})");

            // verify right away so bad credentials surface here, not in the TUI
            let account = config::load()
                .ok()
                .and_then(|c| c.accounts.into_iter().find(|a| a.email == email))
                .unwrap_or(config::AccountConfig {
                    name: email.clone(),
                    email: email.clone(),
                    imap_host: "imap.gmail.com".into(),
                    smtp_host: "smtp.gmail.com".into(),
                });
            print!("verifying IMAP login… ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let verified = config::get_password(&email)?;
            match imap_client::ImapClient::connect(&account, &verified).await {
                Ok(client) => {
                    println!("ok ✓");
                    client.logout().await;
                }
                Err(e) => {
                    println!("FAILED\n{e:#}\n");
                    println!("checklist:");
                    println!(
                        "  - use an app password (https://myaccount.google.com/apppasswords), not your normal password"
                    );
                    println!("  - 2FA must be enabled on the account to create app passwords");
                    println!("  - the email must match the account the password was generated for");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        Some("stacks") => return cli_stacks().await,
        Some("help") | Some("--help") | Some("-h") => {
            println!(
                "mailprune — email triage TUI\n\n  mailprune            run the TUI\n  mailprune auth <em>  store a Gmail app password in the keychain\n  mailprune stacks     print stacks to stdout (no TUI)\n\nconfig: ~/.config/mailprune/config.toml\n{}",
                config::SAMPLE_CONFIG
            );
            return Ok(());
        }
        _ => {}
    }

    debuglog::write(format!(
        "=== session start v{} pid {} ===",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
    let cfg = config::load()?;
    let log = action_log::ActionLog::new(cfg.action_log);
    let mut app = ui::app::App::new(cfg.accounts, log);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app).await;
    ratatui::restore();
    // stacks seen on screen but never acted on become implicit "keep" labels
    app.flush_keeps();

    let s = &app.stats;
    let cleaned = s.trashed + s.archived;
    if cleaned + s.marked_read + s.unsubscribed > 0 {
        println!("this session:");
        if cleaned > 0 {
            println!(
                "  cleaned {cleaned} emails ({} trashed, {} archived)",
                s.trashed, s.archived
            );
        }
        if s.marked_read > 0 {
            println!("  marked {} read", s.marked_read);
        }
        if s.unsubscribed > 0 {
            println!("  unsubscribed from {} senders", s.unsubscribed);
        }
    }
    result
}

async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut ui::app::App) -> Result<()> {
    // draw the loading frame before the first (slow) fetch
    terminal.draw(|f| ui::view::draw(f, app))?;
    app.spawn_batch(ui::app::Load::Reset);

    let mut events = EventStream::new();
    // drives the spinner; only polled while an action is in flight
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal.draw(|f| ui::view::draw(f, app))?;
        if app.should_quit {
            // an in-flight action must land before we exit: its outcome is
            // what gets logged, and quitting first would both abandon the
            // IMAP move mid-flight and mislabel the stack as "keep".
            // Bounded by the client's own operation timeouts.
            // a load is read-only: nothing to log, nothing left half-done, so
            // it is abandoned rather than waited out
            app.load_rx = None;
            app.loading_acct = None;
            let Some(mut rx) = app.action_rx.take() else {
                break;
            };
            app.status = "finishing action before exit…".into();
            terminal.draw(|f| ui::view::draw(f, app))?;
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(msg) => if !apply(app, msg, Source::Action) { break },
                        None => break,
                    },
                    _ = ticker.tick() => app.tick_spinner(),
                }
                terminal.draw(|f| ui::view::draw(f, app))?;
            }
            break;
        }
        // A load and an action can be in flight at once, so both receivers are
        // raced against key events — the UI stays live (progress in the status
        // bar, the whole triage keyboard working) while either runs.
        let mut load_rx = app.load_rx.take();
        let mut action_rx = app.action_rx.take();
        let (mut load_spent, mut action_spent) = (false, false);
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => app.handle_event(ev),
                Some(Err(_)) => {}
                None => break,
            },
            // disabled with no task in flight, so the spinner can never freeze
            // mid-animation
            _ = ticker.tick(), if app.working() => app.tick_spinner(),
            msg = recv(&mut load_rx) => match msg {
                Some(msg) => load_spent = !apply(app, msg, Source::Load),
                None => {
                    // task died without reporting (bug); recover the UI
                    load_spent = true;
                    app.loading_acct = None;
                    app.status = "error: load task exited unexpectedly".into();
                }
            },
            msg = recv(&mut action_rx) => match msg {
                Some(msg) => action_spent = !apply(app, msg, Source::Action),
                None => {
                    action_spent = true;
                    app.busy = false;
                    app.status = "error: action task exited unexpectedly".into();
                }
            },
        }
        // put back what is still running. A key handled this pass may have
        // spawned a fresh task into either slot; that one is live and the one
        // we took out is not, so it keeps the slot.
        if !load_spent && app.load_rx.is_none() {
            app.load_rx = load_rx;
        }
        if !action_spent && app.action_rx.is_none() {
            app.action_rx = action_rx;
        }
    }
    Ok(())
}

/// Await a receiver that may not exist. A `None` slot never resolves, so the
/// `select!` arm holding it simply never fires.
async fn recv(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ui::app::TaskMsg>>,
) -> Option<ui::app::TaskMsg> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// which of the two in-flight tasks a message came from
#[derive(PartialEq)]
enum Source {
    Load,
    Action,
}

/// Apply one message from an in-flight task. Returns whether that task is
/// still running — `false` means `Done` landed and the receiver is spent.
fn apply(app: &mut ui::app::App, msg: ui::app::TaskMsg, from: Source) -> bool {
    match msg {
        // an action is the thing the user is waiting on, so a load's progress
        // line does not talk over it
        ui::app::TaskMsg::Status(s) => {
            if from == Source::Action || !app.busy {
                app.status = s;
            }
        }
        ui::app::TaskMsg::Uids { acct_idx, uids } => app.on_uids(acct_idx, uids),
        ui::app::TaskMsg::Sender { acct_idx, batch } => app.on_sender(acct_idx, *batch),
        ui::app::TaskMsg::Done(done) => {
            app.on_task_done(done);
            return false;
        }
    }
    true
}

/// headless checkpoint: connect, load one batch of senders, print stacks
async fn cli_stacks() -> Result<()> {
    let cfg = config::load()?;
    for account in &cfg.accounts {
        println!("== {} ({}) ==", account.name, account.email);
        let password = config::get_password(&account.email)?;
        let mut client = imap_client::ImapClient::connect(account, &password).await?;
        let uids = client.uid_list().await?;
        let mut msgs = Vec::new();
        let mut partial = Vec::new();
        let (_, outcome) = imap_client::load_batch(
            &mut client,
            &uids,
            0,
            &std::collections::HashSet::new(),
            |batch| {
                if batch.partial {
                    partial.push(batch.addr);
                }
                msgs.extend(batch.msgs);
            },
        )
        .await;
        outcome?;
        let total = msgs.len();
        let stacks = stacks::build_stacks(msgs, stacks::GroupBy::Sender, stacks::SortBy::Count);
        println!(
            "{total} of {} messages, {} stacks{}\n",
            uids.len(),
            stacks.len(),
            if partial.is_empty() {
                String::new()
            } else {
                format!(" ({} senders only partly listed)", partial.len())
            }
        );
        for s in &stacks {
            println!(
                "{:>5}  {}  {} <{}>  — {}",
                s.msgs.len(),
                if s.can_unsubscribe { "U" } else { " " },
                s.display_name,
                s.key,
                s.latest().subject
            );
        }
        client.logout().await;
        println!();
    }
    Ok(())
}

fn prompt(msg: &str) -> String {
    use std::io::Write;
    print!("{msg}");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).ok();
    buf.trim().to_string()
}

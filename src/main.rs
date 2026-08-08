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
            // a fetch is read-only: nothing to log, nothing left half-done, so
            // quitting mid-load exits now instead of waiting out the fetch
            let Some(mut rx) = app.task_rx.take().filter(|_| !app.loading) else {
                break;
            };
            app.status = "finishing action before exit…".into();
            terminal.draw(|f| ui::view::draw(f, app))?;
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(msg) => if !apply(app, msg) { break },
                        None => break,
                    },
                    _ = ticker.tick() => app.tick_spinner(),
                }
                terminal.draw(|f| ui::view::draw(f, app))?;
            }
            break;
        }
        // while an action task runs, race its messages against key events so
        // the UI stays live (progress in the status bar, navigation works)
        if let Some(mut rx) = app.task_rx.take() {
            tokio::select! {
                ev = events.next() => {
                    app.task_rx = Some(rx);
                    match ev {
                        Some(Ok(ev)) => app.handle_event(ev),
                        Some(Err(_)) => {}
                        None => break,
                    }
                }
                _ = ticker.tick(), if app.busy => {
                    app.task_rx = Some(rx);
                    app.tick_spinner();
                }
                msg = rx.recv() => match msg {
                    Some(msg) => {
                        if apply(app, msg) {
                            app.task_rx = Some(rx);
                        }
                    }
                    None => {
                        // task died without reporting (bug); recover the UI
                        app.busy = false;
                        app.status = "error: action task exited unexpectedly".into();
                    }
                },
            }
        } else {
            // no task in flight, so `busy` should be false and the ticker arm
            // disabled — kept so the spinner can never freeze mid-animation
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(ev)) => app.handle_event(ev),
                    Some(Err(_)) => {}
                    None => break,
                },
                _ = ticker.tick(), if app.busy => app.tick_spinner(),
            }
        }
    }
    Ok(())
}

/// Apply one message from the in-flight task. Returns whether the task is
/// still running — `false` means `Done` landed and the receiver is spent.
fn apply(app: &mut ui::app::App, msg: ui::app::TaskMsg) -> bool {
    match msg {
        ui::app::TaskMsg::Status(s) => app.status = s,
        ui::app::TaskMsg::Done(done) => {
            app.on_task_done(done);
            return false;
        }
    }
    true
}

/// headless checkpoint: connect, sweep one window, print stacks
async fn cli_stacks() -> Result<()> {
    let cfg = config::load()?;
    for account in &cfg.accounts {
        println!("== {} ({}) ==", account.name, account.email);
        let password = config::get_password(&account.email)?;
        let mut client = imap_client::ImapClient::connect(account, &password).await?;
        let (sweep, outcome) = imap_client::sweep(&mut client, 0, |p| {
            eprintln!("  swept {} of {} · {} stacks", p.swept, p.bound, p.stacks);
        })
        .await;
        // a short window is still a window: print what landed, then say what
        // stopped it
        if let Err(e) = outcome {
            eprintln!("  sweep stopped: {e:#}");
        }
        let total = sweep.msgs.len();
        let stacks =
            stacks::build_stacks(sweep.msgs, stacks::GroupBy::Sender, stacks::SortBy::Count);
        println!(
            "{total} messages in {} of {} swept ({} in the mailbox), {} stacks\n",
            sweep.swept,
            sweep.bound,
            sweep.total,
            stacks.len()
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

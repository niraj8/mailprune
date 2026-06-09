mod config;
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
            let password =
                rpassword::prompt_password(format!("app password for {email}: "))?;
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
                    println!("  - use an app password (https://myaccount.google.com/apppasswords), not your normal password");
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

    let cfg = config::load()?;
    let mut app = ui::app::App::new(cfg.accounts);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app).await;
    ratatui::restore();

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
    if let Err(e) = app.load_active().await {
        app.status = format!("error: {e:#}");
    }

    let mut events = EventStream::new();
    loop {
        terminal.draw(|f| ui::view::draw(f, app))?;
        if app.should_quit {
            break;
        }
        match events.next().await {
            Some(Ok(ev)) => app.handle_event(ev).await,
            Some(Err(_)) => {}
            None => break,
        }
    }
    Ok(())
}

/// headless checkpoint: connect, fetch, print stacks
async fn cli_stacks() -> Result<()> {
    let cfg = config::load()?;
    for account in &cfg.accounts {
        println!("== {} ({}) ==", account.name, account.email);
        let password = config::get_password(&account.email)?;
        let mut client = imap_client::ImapClient::connect(account, &password).await?;
        let msgs = client.fetch_inbox().await?;
        let total = msgs.len();
        let stacks =
            stacks::build_stacks(msgs, stacks::GroupBy::Sender, stacks::SortBy::Count);
        println!("{total} messages, {} stacks\n", stacks.len());
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

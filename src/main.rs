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
            config::store_password(&email, password.trim())?;
            println!("stored in keychain (service \"mailstack\", account {email})");
            return Ok(());
        }
        Some("stacks") => return cli_stacks().await,
        Some("help") | Some("--help") | Some("-h") => {
            println!(
                "mailstack — email triage TUI\n\n  mailstack            run the TUI\n  mailstack auth <em>  store a Gmail app password in the keychain\n  mailstack stacks     print stacks to stdout (no TUI)\n\nconfig: ~/.config/mailstack/config.toml\n{}",
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
        let stacks = stacks::build_stacks(msgs);
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

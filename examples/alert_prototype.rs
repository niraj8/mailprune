//! PROTOTYPE — throwaway. Not part of the app; delete once the alert is decided.
//!
//! Question: what does the centered alert look like, given it is the one slot
//! for "the app is busy" (sweep) and "the app wants an answer" (confirm)?
//!
//!   cargo run --example alert_prototype
//!
//!   1-4  variant          s  sweeping     f  first sweep / widening
//!   q    quit             e  sweep failed
//!                         c  action confirm
//!                         n  no alert (baseline)

use crossterm::event::{self, Event, KeyCode};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::time::{Duration, Instant};

const BOUND: u32 = 5000;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(PartialEq, Clone, Copy)]
enum State {
    Sweeping,
    Failed,
    Confirm,
    None,
}

struct P {
    variant: u8,
    state: State,
    swept: u32,
    tick: usize,
    /// first sweep has nothing behind the alert; a widening `m` does
    first_sweep: bool,
}

fn main() -> std::io::Result<()> {
    let mut term = ratatui::init();
    let mut p = P {
        variant: 1,
        state: State::Sweeping,
        swept: 0,
        tick: 0,
        first_sweep: true,
    };
    let mut last = Instant::now();
    loop {
        term.draw(|f| draw(f, &p))?;
        if event::poll(Duration::from_millis(60))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char(c @ '1'..='4') => p.variant = c as u8 - b'0',
                    KeyCode::Char('s') => {
                        p.state = State::Sweeping;
                        p.swept = 0;
                    }
                    KeyCode::Char('e') => p.state = State::Failed,
                    KeyCode::Char('c') => p.state = State::Confirm,
                    KeyCode::Char('n') => p.state = State::None,
                    KeyCode::Char('f') => p.first_sweep = !p.first_sweep,
                    _ => {}
                }
            }
        }
        if last.elapsed() > Duration::from_millis(90) {
            last = Instant::now();
            p.tick += 1;
            if p.state == State::Sweeping {
                p.swept = (p.swept + 137).min(BOUND);
            }
        }
    }
    ratatui::restore();
    Ok(())
}

fn stacks_found(swept: u32) -> u32 {
    swept / 120
}

fn draw(f: &mut Frame, p: &P) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(f.area());

    // ---- the app behind the alert -------------------------------------
    let hide_list = p.state == State::Sweeping && p.first_sweep;
    draw_header(f, rows[0]);
    draw_panes(f, rows[1], hide_list);
    draw_footer(f, rows[2]);

    if p.state == State::None {
        return;
    }

    // variants 1, 2 and 4 dim what is behind; 3 does not
    if p.variant != 3 {
        let dim = if p.variant == 4 {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        let whole = f.area();
        f.buffer_mut().set_style(whole, dim);
    }

    match p.variant {
        1 => alert_plain(f, p),
        2 => alert_bar(f, p),
        3 => alert_band(f, p),
        _ => alert_loud(f, p),
    }
}

// ---- variants ---------------------------------------------------------

/// 1 — bordered box, one line of text, nothing else
fn alert_plain(f: &mut Frame, p: &P) {
    let (title, body, colour) = content(p);
    let area = centered(f.area(), 46, 3);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(body)
            .centered()
            .block(border(title, colour)),
        area,
    );
}

/// 2 — box with a progress bar under the counter
fn alert_bar(f: &mut Frame, p: &P) {
    let (title, body, colour) = content(p);
    let area = centered(f.area(), 46, 4);
    f.render_widget(Clear, area);
    f.render_widget(border(title, colour), area);
    let inner = area.inner(Margin::new(1, 1));
    let lines = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);
    f.render_widget(Paragraph::new(body).centered(), lines[0]);
    if p.state == State::Sweeping {
        f.render_widget(
            Paragraph::new(bar(p.swept, BOUND, lines[1].width as usize))
                .style(Style::new().fg(colour)),
            lines[1],
        );
    }
}

/// 3 — full-width reverse-video band, no border, nothing dimmed
fn alert_band(f: &mut Frame, p: &P) {
    let (title, body, colour) = content(p);
    let mut area = f.area();
    area.y = area.height / 2 - 1;
    area.height = 3;
    f.render_widget(Clear, area);
    let text = vec![
        Line::from(title.trim().to_string()).centered(),
        Line::from(body).centered(),
    ];
    f.render_widget(
        Paragraph::new(text).style(Style::new().bg(colour).fg(Color::Black).bold()),
        area,
    );
}

/// 4 — wide box, spinner, bar and a key hint; background greyed hard
fn alert_loud(f: &mut Frame, p: &P) {
    let (title, body, colour) = content(p);
    let area = centered(f.area(), 60, 6);
    f.render_widget(Clear, area);
    f.render_widget(border(title, colour), area);
    let inner = area.inner(Margin::new(2, 1));
    let lines = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let head = if p.state == State::Sweeping {
        format!("{} {}", SPINNER[p.tick % SPINNER.len()], body)
    } else {
        body
    };
    f.render_widget(
        Paragraph::new(head).centered().style(Style::new().bold()),
        lines[0],
    );
    if p.state == State::Sweeping {
        f.render_widget(
            Paragraph::new(bar(p.swept, BOUND, lines[2].width as usize))
                .style(Style::new().fg(colour)),
            lines[2],
        );
    }
    f.render_widget(
        Paragraph::new(hint(p))
            .centered()
            .style(Style::new().fg(Color::Gray)),
        lines[3],
    );
}

// ---- shared -----------------------------------------------------------

fn content(p: &P) -> (String, String, Color) {
    match p.state {
        State::Sweeping => (
            " sweeping ".into(),
            format!(
                "{} of {} · {} stacks",
                commas(p.swept),
                commas(BOUND),
                stacks_found(p.swept)
            ),
            Color::Cyan,
        ),
        State::Failed => (
            " sweep failed ".into(),
            "stopped at 2,400 of 5,000".into(),
            Color::Red,
        ),
        State::Confirm => (
            " confirm ".into(),
            "trash 400 messages from DoorDash (12 in view)?".into(),
            Color::Yellow,
        ),
        State::None => (String::new(), String::new(), Color::Reset),
    }
}

fn hint(p: &P) -> &'static str {
    match p.state {
        State::Sweeping => "q quit",
        State::Failed => "m retry · any key to continue",
        State::Confirm => "y yes · n no",
        State::None => "",
    }
}

fn border(title: String, colour: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(colour))
        .title(Span::styled(title, Style::new().fg(colour).bold()))
}

fn bar(done: u32, total: u32, width: usize) -> String {
    let filled = (done as usize * width) / total.max(1) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn commas(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---- fake app chrome --------------------------------------------------

const ROWS: [(&str, &str, &str); 6] = [
    ("214", "0%", "DoorDash (12 new)"),
    ("120", "2%", "Medium Daily Digest"),
    ("76", "31%", "LinkedIn"),
    ("31", "94%", "GitHub"),
    ("28", "7%", "Uber Receipts"),
    ("19", "63%", "Slack"),
];

fn draw_header(f: &mut Frame, area: Rect) {
    f.render_widget(
        Paragraph::new(" mailprune  personal  work            12 trashed · 40 archived")
            .style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

fn draw_panes(f: &mut Frame, area: Rect, hide_list: bool) {
    let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);
    let title = " stacks (41) · newest 5,000 of 137,482 msgs · by sender · sort read rate ";
    let body: Vec<Line> = if hide_list {
        vec![]
    } else {
        ROWS.iter()
            .map(|(n, rate, name)| {
                Line::from(format!("{n:>5} {rate:>5} U {name}"))
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(title)),
        cols[0],
    );
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" DoorDash <no-reply@doordash.com> "),
        cols[1],
    );
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(" personal: 873 of 137,482 messages in 42 stacks      m more  g group  s sort")
            .style(Style::new().fg(Color::DarkGray)),
        Line::from(" j/k move  Space mark  d trash  e archive  r read  u unsub  / filter  ? keys")
            .style(Style::new().fg(Color::DarkGray)),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

use chrono::{DateTime, Datelike, Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::app::{App, Mode};

/// attachment marker; two cells wide, so the empty slot is two spaces and the
/// subject column stays aligned
const CLIP: &str = "📎";
const CLIP_BLANK: &str = "  ";

pub fn draw(frame: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Min(3),    // panes
            Constraint::Length(1), // status
            Constraint::Length(1), // help
        ])
        .split(frame.area());

    draw_tabs(frame, app, outer[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(outer[1]);

    draw_stack_list(frame, app, panes[0]);
    draw_detail(frame, app, panes[1]);
    draw_status(frame, app, outer[2]);
    draw_help(frame, app, outer[3]);
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = vec![Span::styled(
        " mailprune ",
        Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
    )];
    for (i, acct) in app.accounts.iter().enumerate() {
        let style = if i == app.active {
            Style::default().fg(Color::Cyan).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(acct.cfg.name.clone(), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_stack_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let acct = app.account();
    let visible = app.visible_stacks();
    let marked = if acct.marked.is_empty() {
        String::new()
    } else {
        format!(" · {} marked", acct.marked.len())
    };
    let title = if app.filter.is_empty() {
        format!(
            " stacks ({}) · {} msgs · by {} · sort {}{} ",
            visible.len(),
            acct.total_messages(),
            app.group_by.label(),
            app.sort_by.label(),
            marked
        )
    } else {
        format!(
            " stacks ({}) · filter: {}{} ",
            visible.len(),
            app.filter,
            marked
        )
    };
    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let s = &acct.stacks[i];
            let is_marked = acct.marked.contains(&s.key);
            let mark = if is_marked { "▌" } else { " " };
            let count = format!("{:>4}", s.msgs.len());
            let badge = if s.can_unsubscribe { "U" } else { " " };
            let rate = s.read_rate();
            let rate_style = match rate {
                0..=10 => Style::default().fg(Color::Red),
                11..=40 => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::DarkGray),
            };
            let unread = if s.unread_count > 0 {
                format!(" ({} new)", s.unread_count)
            } else {
                String::new()
            };
            let name_style = if is_marked {
                Style::default().fg(Color::Cyan).bold()
            } else if s.unread_count > 0 {
                Style::default().bold()
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(mark, Style::default().fg(Color::Cyan).bold()),
                Span::styled(count, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(format!("{rate:>3}%"), rate_style),
                Span::raw(" "),
                Span::styled(badge, Style::default().fg(Color::Green).bold()),
                Span::raw(" "),
                Span::styled(truncate(&s.display_name, 22), name_style),
            ];
            if let Some(subject) = &s.subject {
                spans.push(Span::styled(
                    format!(" · {}", truncate(subject, 32)),
                    Style::default().fg(Color::Gray),
                ));
            }
            spans.push(Span::styled(unread, Style::default().fg(Color::Cyan)));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let highlight = if acct.expanded {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(highlight);
    let mut state = ListState::default();
    if !visible.is_empty() {
        state.select(Some(acct.selected.min(visible.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);

    // rows actually on screen become "seen" for the action log; the render
    // updated state.offset() to the real scroll position
    let rows = area.height.saturating_sub(2) as usize; // minus borders
    let on_screen: Vec<usize> = visible
        .iter()
        .skip(state.offset())
        .take(rows)
        .copied()
        .collect();
    app.record_seen(&on_screen);
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let acct = app.account();
    let Some(stack_idx) = app.selected_stack_idx() else {
        let p = Paragraph::new("no stacks — inbox zero 🎉")
            .block(Block::default().borders(Borders::ALL).title(" messages "));
        frame.render_widget(p, area);
        return;
    };
    let stack = &acct.stacks[stack_idx];
    let unsub = stack
        .unsubscribe_source()
        .and_then(crate::unsubscribe::pick_method)
        .map(|m| format!(" · unsub: {}", m.describe()))
        .unwrap_or_default();
    let title = format!(
        " {} <{}>{} ",
        truncate(&stack.display_name, 28),
        stack.key,
        unsub
    );
    let items: Vec<ListItem> = stack
        .msgs
        .iter()
        .map(|m| {
            let date = m.date.map(fmt_date).unwrap_or_else(|| "          ".into());
            // this month's mail is what you still have context on, so it reads
            // heavier than the archaeology below it
            let date_style = if m.date.is_some_and(in_current_month) {
                Style::default().fg(Color::DarkGray).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let dot = if m.unread { "●" } else { " " };
            let clip = if m.has_attachment { CLIP } else { CLIP_BLANK };
            let style = if m.unread {
                Style::default().bold()
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(date, date_style),
                Span::raw(" "),
                Span::styled(dot, Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(clip, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(m.subject.clone(), style),
            ]))
        })
        .collect();
    let highlight = if acct.expanded {
        Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(highlight);
    let mut state = ListState::default();
    if acct.expanded {
        state.select(Some(acct.msg_selected.min(stack.msgs.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// braille spinner, one frame per event-loop tick while busy
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.mode {
        Mode::Confirm(action) => Line::from(Span::styled(
            action.prompt(app.account()),
            Style::default().fg(Color::Black).bg(Color::Yellow).bold(),
        )),
        Mode::Filter => Line::from(Span::styled(
            format!("filter: {}▏", app.filter),
            Style::default().fg(Color::Cyan),
        )),
        Mode::Normal => {
            let mut spans = Vec::new();
            if app.busy {
                spans.push(Span::styled(
                    format!("{} ", SPINNER[app.spinner % SPINNER.len()]),
                    Style::default().fg(Color::Cyan).bold(),
                ));
            }
            spans.push(Span::styled(
                app.status.clone(),
                Style::default().fg(Color::Gray),
            ));
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let help = if app.account().expanded {
        " j/k move · Esc collapse · d trash · e archive · r read · u unsub · q quit"
    } else {
        " j/k · Enter expand · Space mark · a mark all · d trash · e archive · r read · u unsub · s group · o sort · / filter · Tab acct · R refresh · q quit"
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn fmt_date(d: DateTime<Utc>) -> String {
    let local = d.with_timezone(&Local);
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M     ").to_string()
    } else {
        local.format("%Y-%m-%d").to_string()
    }
}

/// same calendar month as today, in the viewer's local timezone
fn in_current_month(d: DateTime<Utc>) -> bool {
    let local = d.with_timezone(&Local);
    let now = Local::now();
    local.year() == now.year() && local.month() == now.month()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_log::ActionLog;
    use crate::config::AccountConfig;
    use crate::stacks::{GroupBy, MsgMeta, SortBy, build_stacks};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// the status row of a rendered frame, trailing blanks trimmed
    fn status_row(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row = buf.area.height - 2; // status sits above the help row
        (0..buf.area.width)
            .map(|x| buf[(x, row)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn test_app() -> App {
        let cfg = AccountConfig {
            name: "t".into(),
            email: "me@x.com".into(),
            imap_host: "imap".into(),
            smtp_host: "smtp".into(),
        };
        App::new(
            vec![cfg],
            ActionLog::at(
                std::env::temp_dir().join(format!("mailprune-view-{}.jsonl", std::process::id())),
            ),
        )
    }

    /// full rendered frame, wide enough that the detail pane fits a subject
    fn render(app: &mut App) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// the detail pane row holding `subject`, as (symbol-per-cell, style-per-cell).
    /// Cells, not bytes: a wide glyph occupies two of them and the subject
    /// column can only be compared across rows in cell units.
    fn detail_row(buf: &Buffer, subject: &str) -> (Vec<String>, Vec<Style>) {
        // the detail pane starts at 45% of a 100-column frame, plus its border
        let x0 = 46;
        for y in 0..buf.area.height {
            let cells: Vec<String> = (x0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            if cells.concat().contains(subject) {
                let styles = (x0..buf.area.width).map(|x| buf[(x, y)].style()).collect();
                return (cells, styles);
            }
        }
        panic!("no detail row for {subject:?} in\n{buf:?}");
    }

    /// cell index at which `subject` starts in a row
    fn subject_col(cells: &[String], subject: &str) -> usize {
        (0..cells.len())
            .find(|&i| cells[i..].concat().starts_with(subject))
            .expect("subject is in this row")
    }

    /// app with one stack whose messages are (subject, date, has_attachment)
    fn app_with_msgs(msgs: Vec<(&str, DateTime<Utc>, bool)>) -> App {
        let mut app = test_app();
        let msgs: Vec<MsgMeta> = msgs
            .into_iter()
            .map(|(subject, date, has_attachment)| MsgMeta {
                uid: 1,
                sender_email: "a@x.com".into(),
                sender_name: "A".into(),
                subject: subject.into(),
                date: Some(date),
                unread: false,
                has_attachment,
                list_unsubscribe: None,
                one_click: false,
            })
            .collect();
        app.accounts[0].stacks = build_stacks(msgs, GroupBy::Sender, SortBy::Count);
        app.accounts[0].loaded = true;
        app
    }

    #[test]
    fn attachment_marker_shows_only_for_messages_that_have_one() {
        let day = Utc::now() - chrono::Duration::days(40);
        let mut app = app_with_msgs(vec![("with", day, true), ("without", day, false)]);
        let buf = render(&mut app);

        let (with, _) = detail_row(&buf, "with");
        let (without, _) = detail_row(&buf, "without");
        assert!(
            with.iter().any(|c| c == CLIP),
            "expected a clip in {:?}",
            with.concat()
        );
        assert!(
            !without.iter().any(|c| c == CLIP),
            "unexpected clip in {:?}",
            without.concat()
        );
        // the marker is its own column: subjects still start at the same cell
        assert_eq!(
            subject_col(&with, "with"),
            subject_col(&without, "without"),
            "clip must not shift the subject column"
        );
    }

    #[test]
    fn dates_are_bold_only_for_the_current_month() {
        // mid-month, so the "this month" case can't straddle a month boundary
        let this_month = Local::now().with_day(15).unwrap().with_timezone(&Utc);
        let last_month = this_month - chrono::Duration::days(35);
        let mut app = app_with_msgs(vec![
            ("recent", this_month, false),
            ("old", last_month, false),
        ]);
        let buf = render(&mut app);

        let (_, recent) = detail_row(&buf, "recent");
        let (_, old) = detail_row(&buf, "old");
        // the date occupies the first 10 cells of each row
        assert!(
            recent[..10]
                .iter()
                .all(|s| s.add_modifier.contains(Modifier::BOLD)),
            "current-month date should be bold"
        );
        assert!(
            old[..10]
                .iter()
                .all(|s| !s.add_modifier.contains(Modifier::BOLD)),
            "older date should not be bold"
        );
    }

    #[test]
    fn busy_status_shows_a_spinner_that_advances() {
        let mut app = test_app();
        app.status = "fetching inbox…".into();

        assert_eq!(status_row(&mut app), "fetching inbox…", "idle: no spinner");

        app.busy = true;
        let first = status_row(&mut app);
        app.tick_spinner();
        let second = status_row(&mut app);

        assert!(first.ends_with("fetching inbox…"));
        assert!(SPINNER.contains(&&first[..first.len() - "fetching inbox…".len() - 1]));
        assert_ne!(first, second, "spinner frame advances with the tick");
    }
}

use chrono::{DateTime, Local, Utc};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use super::app::{App, Mode};

pub fn draw(frame: &mut Frame, app: &App) {
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

fn draw_stack_list(frame: &mut Frame, app: &App, area: Rect) {
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
    let title = format!(" {} <{}>{} ", truncate(&stack.display_name, 28), stack.key, unsub);
    let items: Vec<ListItem> = stack
        .msgs
        .iter()
        .map(|m| {
            let date = m
                .date
                .map(fmt_date)
                .unwrap_or_else(|| "          ".into());
            let dot = if m.unread { "●" } else { " " };
            let style = if m.unread {
                Style::default().bold()
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(date, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(dot, Style::default().fg(Color::Cyan)),
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

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (text, style) = match &app.mode {
        Mode::Confirm(action) => (
            action.prompt(app.account()),
            Style::default().fg(Color::Black).bg(Color::Yellow).bold(),
        ),
        Mode::Filter => (
            format!("filter: {}▏", app.filter),
            Style::default().fg(Color::Cyan),
        ),
        Mode::Normal => {
            let prefix = if app.busy { "⏳ " } else { "" };
            (
                format!("{prefix}{}", app.status),
                Style::default().fg(Color::Gray),
            )
        }
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

use chrono::{DateTime, Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use super::app::{App, Mode};

/// the layout needs 6 rows (tab + 3 panes + status + help) and enough columns
/// for a stack row; below this the Cassowary solver silently squeezes panes to
/// nothing, which reads as a broken TUI rather than a small one
const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;

pub fn draw(frame: &mut Frame, app: &mut App) {
    if frame.area().width < MIN_WIDTH || frame.area().height < MIN_HEIGHT {
        draw_too_small(frame);
        return;
    }

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

    if matches!(app.mode, Mode::Help) {
        draw_help_overlay(frame, frame.area());
    }
}

fn draw_too_small(frame: &mut Frame) {
    let area = frame.area();
    let lines = vec![
        Line::from(Span::styled(
            "terminal too small",
            Style::default().fg(Color::Yellow).bold(),
        )),
        Line::from(Span::styled(
            format!(
                "need {MIN_WIDTH}x{MIN_HEIGHT} — this one is {}x{}",
                area.width, area.height
            ),
            Style::default().fg(Color::Gray),
        )),
    ];
    let y = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
    let box_ = Rect {
        x: area.x,
        y,
        width: area.width,
        height: (lines.len() as u16).min(area.height),
    };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), box_);
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

/// a sender name never needs more than this, however wide the pane gets
const NAME_MAX: usize = 22;
/// below this a subject says nothing useful, so it is dropped instead
const SUBJECT_MIN: usize = 12;

/// the first candidate that fits `width`, else the last (shortest) one
fn first_that_fits(candidates: &[String], width: usize) -> String {
    candidates
        .iter()
        .find(|c| c.chars().count() <= width)
        .unwrap_or_else(|| candidates.last().expect("at least one candidate"))
        .clone()
}

fn draw_stack_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let acct = app.account();
    let visible = app.visible_stacks();
    // the marked count abbreviates before it disappears — it is the only
    // feedback that Space did anything
    let (marked, marked_short) = if acct.marked.is_empty() {
        (String::new(), String::new())
    } else {
        let n = acct.marked.len();
        (format!(" · {n} marked"), format!(" · {n}▌"))
    };
    let inner = area.width.saturating_sub(2) as usize;
    // group-by, sort and the marked count are the state `s`, `o` and Space
    // change, so they are the last things to go; the labels around them and
    // the message total drop first
    let title = if app.filter.is_empty() {
        let (n, msgs) = (visible.len(), acct.total_messages());
        let (group, sort) = (app.group_by.label(), app.sort_by.label());
        first_that_fits(
            &[
                format!(" stacks ({n}) · {msgs} msgs · by {group} · sort {sort}{marked} "),
                format!(" stacks ({n}) · by {group} · sort {sort}{marked} "),
                format!(" stacks ({n}) · {group} · {sort}{marked} "),
                format!(" {n} · {group} · {sort}{marked} "),
                format!(" {n} · {group} · {sort}{marked_short} "),
            ],
            inner,
        )
    } else {
        let n = visible.len();
        first_that_fits(
            &[
                format!(" stacks ({n}) · filter: {}{marked} ", app.filter),
                format!(" ({n}) /{}{marked} ", app.filter),
                format!(" ({n}) /{}{marked_short} ", app.filter),
            ],
            inner,
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
            // everything left of the name is fixed-width: mark, count, rate,
            // unsub badge and their separating spaces
            const FIXED: usize = 13;
            let free = inner
                .saturating_sub(FIXED)
                .saturating_sub(unread.chars().count());
            let name_budget = free.min(NAME_MAX);
            let mut spans = vec![
                Span::styled(mark, Style::default().fg(Color::Cyan).bold()),
                Span::styled(count, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(format!("{rate:>3}%"), rate_style),
                Span::raw(" "),
                Span::styled(badge, Style::default().fg(Color::Green).bold()),
                Span::raw(" "),
                Span::styled(truncate(&s.display_name, name_budget), name_style),
            ];
            // the subject is the first thing to go: a truncated sender is
            // worse than no subject at all
            let for_subject = free - name_budget;
            if let Some(subject) = &s.subject
                && for_subject >= SUBJECT_MIN + 3
            {
                spans.push(Span::styled(
                    format!(" · {}", truncate(subject, (for_subject - 3).min(32))),
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
            // recent mail is what you still have context on, so it reads
            // heavier than the archaeology below it
            let date_style = if m.date.is_some_and(is_recent) {
                Style::default().fg(Color::DarkGray).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let dot = if m.unread { "●" } else { " " };
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
        Mode::Normal | Mode::Help => {
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
    let status_width = line.width();
    frame.render_widget(Paragraph::new(line), area);

    // view/state keys ride the right end of the status row: they belong with
    // the state they change (the pane title says "by sender · sort read rate")
    // and it keeps the footer to one screenful of action keys
    if !matches!(app.mode, Mode::Normal) {
        return;
    }
    let view_keys: &[(&str, &str)] = &[("s", "group"), ("o", "sort"), ("Tab", "acct")];
    let free = (area.width as usize).saturating_sub(status_width + 2);
    let spans = hint_spans(view_keys, free);
    if spans.is_empty() {
        return;
    }
    let width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right = Rect {
        x: area.x + area.width - width as u16 - 1,
        y: area.y,
        width: width as u16,
        height: 1,
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), right);
}

/// "k label  " — the cost of one rendered hint, including its separator
fn hint_width((k, desc): (&str, &str)) -> usize {
    k.chars().count() + desc.chars().count() + 3
}

/// as many hints as fit in `max` columns, in priority order. drops whole
/// hints off the tail rather than truncating one mid-word.
fn hint_spans<'a>(hints: &[(&'a str, &'a str)], max: usize) -> Vec<Span<'a>> {
    let key = Style::default().fg(Color::Cyan);
    let label = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    let mut used = 0;
    for &h in hints {
        if used + hint_width(h) > max {
            break;
        }
        used += hint_width(h);
        spans.push(Span::styled(h.0, key));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(h.1, label));
        spans.push(Span::raw("  "));
    }
    spans.pop(); // trailing separator
    spans
}

/// keys that change what happens to mail. the ones that only change the
/// *view* ride along on the status row instead — see `draw_status`.
fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let hints: &[(&str, &str)] = if app.account().expanded {
        &[
            ("j/k", "move"),
            ("Esc", "back"),
            ("d", "trash"),
            ("e", "archive"),
            ("r", "read"),
            ("u", "unsub"),
        ]
    } else {
        &[
            ("j/k", "move"),
            ("↵", "open"),
            ("Space", "mark"),
            ("d", "trash"),
            ("e", "archive"),
            ("u", "unsub"),
            ("/", "filter"),
        ]
    };
    const HELP: (&str, &str) = ("?", "keys");

    let mut spans = vec![Span::raw(" ")];
    // reserve the tail for `? keys`: whatever else gets dropped, the way to
    // find it back must not be
    let budget = (area.width as usize).saturating_sub(1 + hint_width(HELP));
    spans.extend(hint_spans(hints, budget));
    spans.push(Span::raw("  "));
    spans.extend(hint_spans(&[HELP], usize::MAX));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// every binding, grouped, shown over a dimmed frame. any key dismisses.
fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    const SECTIONS: [(&str, &[(&str, &str)]); 4] = [
        (
            "move",
            &[
                ("j / k, ↓ / ↑", "next / previous"),
                ("g / G", "top / bottom"),
                ("Enter", "expand / collapse stack"),
                ("Esc", "collapse, else clear marks / filter"),
                ("Tab", "next account"),
            ],
        ),
        (
            "select",
            &[
                ("Space", "mark stack, advance"),
                ("a", "mark / unmark everything in view"),
            ],
        ),
        (
            "act on selection",
            &[
                ("d", "trash"),
                ("e", "archive"),
                ("r", "mark read"),
                ("u", "unsubscribe"),
            ],
        ),
        (
            "view",
            &[
                ("s", "group by sender / subject"),
                ("o", "cycle sort"),
                ("/", "filter"),
                ("R", "refresh"),
                ("q", "quit"),
            ],
        ),
    ];

    let mut lines: Vec<Line> = Vec::new();
    for (title, rows) in SECTIONS {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            Style::default().fg(Color::Yellow).bold(),
        )));
        for (k, desc) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!(" {k:>14}  "), Style::default().fg(Color::Cyan)),
                Span::styled(*desc, Style::default().fg(Color::Gray)),
            ]));
        }
    }

    let width = 54.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" keys · any key to close "),
        ),
        popup,
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

const RECENT_DAYS: i64 = 30;

/// received within the last 30 days. A date in the future (clock skew on the
/// sending side) counts as recent rather than ancient.
fn is_recent(d: DateTime<Utc>) -> bool {
    Utc::now().signed_duration_since(d) < chrono::Duration::days(RECENT_DAYS)
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

    /// the status row of a rendered frame, trailing blanks trimmed
    fn status_row(app: &mut App) -> String {
        let frame = render(app, 80, MIN_HEIGHT);
        // status sits above the help row
        frame.lines().nth_back(1).unwrap().trim_end().to_string()
    }

    /// the status row with the right-aligned view hints stripped off
    fn status_text(app: &mut App) -> String {
        let row = status_row(app);
        let cut = row.find("  s group").unwrap_or(row.len());
        row[..cut].trim_end().to_string()
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

    /// the style of each cell in the detail-pane row holding `subject`
    fn detail_row_styles(app: &mut App, subject: &str) -> Vec<Style> {
        let mut terminal = Terminal::new(TestBackend::new(100, MIN_HEIGHT)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // the detail pane starts at 45% of a 100-column frame, plus its border
        let x0 = 46;
        for y in 0..buf.area.height {
            let text: String = (x0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if text.contains(subject) {
                return (x0..buf.area.width).map(|x| buf[(x, y)].style()).collect();
            }
        }
        panic!("no detail row for {subject:?} in\n{buf:?}");
    }

    /// app holding one stack, its messages given as (subject, date)
    fn app_with_msgs(msgs: Vec<(&str, DateTime<Utc>)>) -> App {
        let mut app = test_app();
        let msgs: Vec<MsgMeta> = msgs
            .into_iter()
            .map(|(subject, date)| MsgMeta {
                uid: 1,
                sender_email: "a@x.com".into(),
                sender_name: "A".into(),
                subject: subject.into(),
                date: Some(date),
                unread: false,
                list_unsubscribe: None,
                one_click: false,
            })
            .collect();
        app.accounts[0].stacks = build_stacks(msgs, GroupBy::Sender, SortBy::Count);
        app.accounts[0].loaded = true;
        app
    }

    /// the whole frame as one string, rows joined by newline
    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn footer(app: &mut App, w: u16) -> String {
        render(app, w, MIN_HEIGHT)
            .lines()
            .last()
            .unwrap()
            .trim_end()
            .to_string()
    }

    /// the stack-list pane's rows, borders stripped, at the given size
    fn stack_pane(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let inner_w = (w as usize * 45 / 100) - 2;
        render(app, w, h)
            .lines()
            .skip(1) // tab bar
            .take(h as usize - 3) // minus tab bar, status, help
            .map(|l| l.chars().skip(1).take(inner_w).collect::<String>())
            .collect()
    }

    fn msg(uid: u32, email: &str, name: &str, subject: &str) -> MsgMeta {
        MsgMeta {
            uid,
            sender_email: email.into(),
            sender_name: name.into(),
            subject: subject.into(),
            date: None,
            unread: false,
            list_unsubscribe: None,
            one_click: false,
        }
    }

    fn app_with(msgs: Vec<MsgMeta>, group_by: GroupBy) -> App {
        let mut app = test_app();
        app.group_by = group_by;
        app.account_mut().stacks = build_stacks(msgs, group_by, SortBy::Count);
        app.account_mut().loaded = true;
        app
    }

    #[test]
    fn dates_are_bold_only_within_the_last_30_days() {
        let now = Utc::now();
        // a day either side of the cutoff, so the boundary itself is covered
        let inside = now - chrono::Duration::days(RECENT_DAYS - 1);
        let outside = now - chrono::Duration::days(RECENT_DAYS + 1);
        let mut app = app_with_msgs(vec![("recent", inside), ("old", outside)]);

        let recent = detail_row_styles(&mut app, "recent");
        let old = detail_row_styles(&mut app, "old");
        // the date occupies the first 10 cells of each row
        assert!(
            recent[..10]
                .iter()
                .all(|s| s.add_modifier.contains(Modifier::BOLD)),
            "date inside the 30-day window should be bold"
        );
        assert!(
            old[..10]
                .iter()
                .all(|s| !s.add_modifier.contains(Modifier::BOLD)),
            "date outside the 30-day window should not be bold"
        );
    }

    #[test]
    fn a_terminal_under_the_minimum_says_so_instead_of_squeezing_panes() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);

        for (w, h) in [(79, 24), (80, 23), (40, 10)] {
            let frame = render(&mut app, w, h);
            assert!(
                frame.contains("80x24"),
                "{w}x{h} should ask for a resize, got:\n{frame}"
            );
            assert!(!frame.contains("stacks ("), "{w}x{h} still drew the panes");
        }

        let frame = render(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(frame.contains("stacks ("), "80x24 is usable, not gated");
        assert!(!frame.contains("80x24"), "no resize message at the minimum");
    }

    #[test]
    fn sender_names_survive_the_80_col_stack_pane_uncut() {
        // 21 chars — the widest name the 34-column inner pane can hold
        let name = "Newsletter Weekly Dig";
        let mut app = app_with(vec![msg(1, "n@x.com", name, "hello")], GroupBy::Sender);

        let rows = stack_pane(&mut app, MIN_WIDTH, MIN_HEIGHT);
        let row = rows
            .iter()
            .find(|r| r.contains("Newsl"))
            .expect("row drawn");
        assert!(row.contains(name), "name was cut: {row:?}");
        assert!(!row.contains('…'), "nothing should be elided: {row:?}");
    }

    #[test]
    fn the_subject_span_is_dropped_at_80_cols_and_returns_when_wide() {
        let msgs = vec![msg(1, "n@x.com", "Deals", "Weekend deals near you")];
        let mut app = app_with(msgs, GroupBy::SenderSubject);

        let narrow = stack_pane(&mut app, MIN_WIDTH, MIN_HEIGHT).join("\n");
        assert!(narrow.contains("Deals"), "sender still drawn: {narrow}");
        assert!(
            !narrow.contains("Weekend"),
            "no room for a subject at 80 cols: {narrow}"
        );

        let wide = stack_pane(&mut app, 160, MIN_HEIGHT).join("\n");
        assert!(
            wide.contains("Weekend deals near you"),
            "the subject earns its place when there is room: {wide}"
        );
    }

    #[test]
    fn the_pane_title_keeps_the_state_the_user_is_changing_at_80_cols() {
        let mut app = app_with(
            vec![
                msg(1, "a@x.com", "Alice", "hi"),
                msg(2, "b@x.com", "Bob", "yo"),
            ],
            GroupBy::Sender,
        );
        app.sort_by = SortBy::ReadRate;
        app.account_mut().marked.insert("a@x.com".into());

        let title = render(&mut app, MIN_WIDTH, MIN_HEIGHT)
            .lines()
            .nth(1)
            .unwrap()
            .to_string();
        // s, o and Space each change one of these — all three must stay legible
        assert!(title.contains("sender"), "group-by lost: {title:?}");
        assert!(title.contains("read rate"), "sort lost: {title:?}");
        assert!(title.contains('1'), "marked count lost: {title:?}");
        let inner: String = title.chars().skip(1).take(34).collect();
        assert!(
            !inner.contains('┐') || inner.ends_with('┐'),
            "title overran the pane: {title:?}"
        );

        // given room, the labels and the message total come back
        let wide = render(&mut app, 200, MIN_HEIGHT)
            .lines()
            .nth(1)
            .unwrap()
            .to_string();
        for part in ["msgs", "by sender", "sort read rate", "1 marked"] {
            assert!(
                wide.contains(part),
                "{part:?} missing at 200 cols: {wide:?}"
            );
        }
    }

    #[test]
    fn an_80_col_terminal_shows_every_hint_across_the_two_chrome_rows() {
        let mut app = test_app();

        let f = footer(&mut app, 80);
        for hint in ["j/k move", "↵ open", "Space mark", "d trash", "u unsub"] {
            assert!(f.contains(hint), "{hint:?} missing from footer {f:?}");
        }
        assert!(f.ends_with("? keys"));

        let status = status_row(&mut app);
        for hint in ["s group", "o sort", "Tab acct"] {
            assert!(
                status.contains(hint),
                "{hint:?} missing from status {status:?}"
            );
        }
    }

    #[test]
    fn the_footer_never_overflows_and_never_loses_the_way_to_the_help() {
        let mut app = test_app();
        // below MIN_WIDTH the resize gate owns the screen, so 80 is the floor
        for width in [MIN_WIDTH, 90, 120, 200] {
            let f = footer(&mut app, width);
            assert!(
                f.chars().count() <= width as usize,
                "footer overflows {width} cols: {f:?}"
            );
            // truncation happens between hints, never inside one
            assert!(f.ends_with("? keys"), "at {width}: {f:?}");
        }
    }

    #[test]
    fn hints_are_dropped_whole_when_the_budget_runs_out() {
        let hints = [("j/k", "move"), ("d", "trash"), ("u", "unsub")];
        let text = |max| {
            hint_spans(&hints, max)
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };

        assert_eq!(text(usize::MAX), "j/k move  d trash  u unsub");
        assert_eq!(
            text(hint_width(hints[0]) + hint_width(hints[1])),
            "j/k move  d trash"
        );
        assert_eq!(text(hint_width(hints[0])), "j/k move");
        assert_eq!(text(0), "", "no room means no hints, not half a hint");
    }

    #[test]
    fn view_hints_yield_the_status_row_to_a_prompt() {
        let mut app = test_app();
        assert!(status_row(&mut app).contains("s group"));

        app.mode = Mode::Filter;
        app.filter = "doordash".into();
        let row = status_row(&mut app);
        assert!(row.starts_with("filter: doordash"));
        assert!(!row.contains("s group"), "prompt owns the row: {row:?}");
    }

    #[test]
    fn question_mark_opens_an_overlay_with_the_bindings_the_footer_drops() {
        let mut app = test_app();
        assert!(!render(&mut app, 80, 24).contains("unsubscribe"));

        app.mode = Mode::Help;
        let frame = render(&mut app, 80, 24);
        assert!(frame.contains("unsubscribe"), "overlay lists every binding");
        assert!(frame.contains("any key to close"));
    }

    #[test]
    fn busy_status_shows_a_spinner_that_advances() {
        let mut app = test_app();
        app.status = "fetching inbox…".into();

        assert_eq!(status_text(&mut app), "fetching inbox…", "idle: no spinner");

        app.busy = true;
        let first = status_text(&mut app);
        app.tick_spinner();
        let second = status_text(&mut app);

        assert!(first.ends_with("fetching inbox…"));
        assert!(SPINNER.contains(&&first[..first.len() - "fetching inbox…".len() - 1]));
        assert_ne!(first, second, "spinner frame advances with the tick");
    }
}

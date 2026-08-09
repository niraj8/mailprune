use chrono::{DateTime, Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::imap_client::WINDOW;

use super::app::{Alert, App, Mode};

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

    // An alert answers every key itself, so the rows that advertise the other
    // keys come down for as long as it is up — otherwise the UI offers keys it
    // is about to swallow, which is what ADR 0001 rules out. The alert's own
    // hint line names what still responds.
    let alert = alert_view(app);

    draw_stack_list(frame, app, panes[0]);
    draw_detail(frame, app, panes[1]);
    draw_status(frame, app, outer[2], alert.is_some());
    draw_help(frame, app, outer[3], alert.is_some());

    // the alert owns the screen while it is up, so nothing else overlays with
    // it — a sweep refuses `?`, and a confirm is answered before anything else
    if let Some(alert) = alert {
        draw_alert(frame, app, &alert);
    } else if matches!(app.mode, Mode::Help) {
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
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // the session's running totals ride the free end of this row: the pane
    // title below is already at capacity, and this puts them next to the
    // message counts they relate to
    let free = (area.width as usize).saturating_sub(used + 2);
    let Some(counters) = session_counters(&app.stats, free) else {
        return;
    };
    let width = counters.chars().count() as u16;
    let right = Rect {
        x: area.x + area.width - width - 1,
        y: area.y,
        width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            counters,
            Style::default().fg(Color::DarkGray),
        ))),
        right,
    );
}

/// What this session has actually cleared, in `max` columns or fewer, or None
/// when nothing has been done yet — the row stays empty until it has something
/// to say. Abbreviates to the action keys before it gives up.
fn session_counters(stats: &super::app::SessionStats, max: usize) -> Option<String> {
    let parts: [(usize, &str, &str); 4] = [
        (stats.trashed, "trashed", "d"),
        (stats.archived, "archived", "e"),
        (stats.marked_read, "read", "r"),
        (stats.unsubscribed, "unsubbed", "u"),
    ];
    let join = |short: bool| -> String {
        parts
            .iter()
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, long, abbrev)| {
                if short {
                    format!("{n}{abbrev}")
                } else {
                    format!("{n} {long}")
                }
            })
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let full = join(false);
    if full.is_empty() {
        return None;
    }
    if full.chars().count() <= max {
        return Some(full);
    }
    let short = join(true);
    (short.chars().count() <= max).then_some(short)
}

/// 137482 -> "137,482" — a six-figure mailbox total is unreadable without it
pub fn commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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
        let n = visible.len();
        // loaded of mailbox-wide, because the loaded set is a recency window
        // while every count in it is the sender's true total. Both numbers
        // fall as you triage: the denominator shrinking is the mailbox
        // actually getting smaller.
        let (loaded, total) = (commas(acct.loaded_messages()), commas(acct.inbox_total()));
        let (group, sort) = (app.group_by.label(), app.sort_by.label());
        first_that_fits(
            &[
                format!(
                    " stacks ({n}) · {loaded} of {total} msgs · by {group} · sort {sort}{marked} "
                ),
                format!(" stacks ({n}) · {loaded}/{total} · by {group} · sort {sort}{marked} "),
                format!(" stacks ({n}) · {loaded}/{total} · {group} · {sort}{marked} "),
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
            // a refused fan-out leaves only the discovery sample, so the count
            // under-reports and trashing the stack under-clears. The marker is
            // the warning; it shares the count's width rather than costing a
            // column of its own.
            let count = if acct.is_partial(s) {
                format!("{:>4}", format!("~{}", s.msgs.len()))
            } else {
                format!("{:>4}", s.msgs.len())
            };
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
    let highlight = Style::default()
        .bg(Color::Cyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let block = Block::default().borders(Borders::ALL).title(title);
    if visible.is_empty() {
        // this is the pane that drains as you triage, so this is where it has
        // to say whether there is more behind it
        frame.render_widget(
            Paragraph::new(empty_state(app))
                .style(Style::default().fg(Color::Gray))
                .wrap(Wrap { trim: true })
                .block(block),
            area,
        );
        return;
    }
    let list = List::new(items).block(block).highlight_style(highlight);
    let mut state = ListState::default();
    state.select(Some(acct.selected.min(visible.len() - 1)));
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

/// why the stack pane is empty — the one place the UI can tell "cleared the
/// batch" from "cleared the inbox", and both are worth saying
fn empty_state(app: &App) -> String {
    let acct = app.account();
    if app.loading {
        "loading…".into()
    } else if !acct.loaded {
        "nothing loaded — press R to try again".into()
    } else if !app.filter.is_empty() {
        "nothing matches the filter — Esc to clear it".into()
    } else if acct.exhausted() {
        "inbox zero — nothing left to triage".into()
    } else {
        format!(
            "all clear — press m to sweep {} more messages",
            commas(WINDOW)
        )
    }
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let acct = app.account();
    let Some(stack_idx) = app.selected_stack_idx() else {
        // the stack pane carries the explanation; this one just stays out of
        // the way rather than repeating it
        frame.render_widget(
            Block::default().borders(Borders::ALL).title(" messages "),
            area,
        );
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
    // no cursor of its own: this pane follows the stack selection, and every
    // action works on the whole stack
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

/// braille spinner, one frame per event-loop tick while busy
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// the alert's frame: 60 columns because the first sweep draws it over a blank
/// screen, where a smaller box reads as a crash rather than as progress, and
/// six rows for a border, a headline, a bar and a hint (ADR 0004)
const ALERT_W: u16 = 60;
const ALERT_H: u16 = 6;

/// what the alert's three states have in common — the same box, filled in
struct AlertView {
    /// names the state; takes `colour` along with the border
    title: &'static str,
    colour: Color,
    headline: String,
    /// The state is one the user is waiting on, so the headline is spun. This
    /// is not `bar.is_some()`: a sweep that has not reported its first chunk
    /// yet has nothing to fill a bar with, and that is exactly the moment — an
    /// alert alone on a blank screen — that has to read as progress rather than
    /// as a crash (ADR 0004).
    spins: bool,
    /// `(done, of)` when there is progress to show. It rides *alongside* the
    /// spinner rather than replacing it: the bar carries progress, the spinner
    /// carries liveness, and a bar that has not moved in two seconds is
    /// ambiguous where a stopped spinner is not (ADR 0004).
    bar: Option<(usize, usize)>,
    /// the keys that work right now, and only those
    hint: &'static str,
}

/// What is in the one centered slot, or nothing. The confirm's copy lives in
/// `Mode::Confirm` rather than in `Alert`, so the two are merged here; they
/// cannot both be up, because a sweep refuses the keys that open a confirm.
fn alert_view(app: &App) -> Option<AlertView> {
    if let Mode::Confirm(action) = &app.mode {
        return Some(AlertView {
            title: " confirm ",
            colour: Color::Yellow,
            headline: action.prompt(app.account()),
            // the app is waiting on the user here, not the other way round
            spins: false,
            bar: None,
            hint: "y yes · n no",
        });
    }
    match app.alert.as_ref()? {
        Alert::Sweeping { starting, progress } => Some(AlertView {
            title: " sweeping ",
            colour: Color::Cyan,
            headline: progress.map_or_else(
                || starting.clone(),
                |p| {
                    format!(
                        "{} of {} · {} stacks",
                        commas(p.swept),
                        commas(p.bound),
                        p.stacks
                    )
                },
            ),
            spins: true,
            bar: progress.map(|p| (p.swept, p.bound)),
            hint: "q quit",
        }),
        Alert::Failed(headline) => Some(AlertView {
            title: " sweep failed ",
            colour: Color::Red,
            headline: headline.clone(),
            spins: false,
            bar: None,
            hint: "m retry · any key to continue",
        }),
    }
}

/// the foreground everything behind the alert is repainted with. Overwriting
/// rather than the `DIM` modifier, because terminals are free to ignore `DIM`.
const MUTED: Color = Color::DarkGray;

fn draw_alert(frame: &mut Frame, app: &App, alert: &AlertView) {
    // Under NO_COLOR the box's border and its `Clear` carry the whole job of
    // separating the alert from the app behind it — which is the reason the
    // box is large. #5 replaces both of these with the theme module.
    let coloured = |c: Color| if app.no_color { Color::Reset } else { c };
    let colour = coloured(alert.colour);
    if !app.no_color {
        let whole = frame.area();
        frame
            .buffer_mut()
            .set_style(whole, Style::default().fg(MUTED));
    }

    let area = centered(frame.area(), ALERT_W, ALERT_H);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colour))
            .title(Span::styled(
                alert.title,
                Style::default().fg(colour).bold(),
            )),
        area,
    );

    // headline, blank, bar, hint — the blank and the bar row hold their places
    // in the states that have no bar, so the box is one shape in every state
    let inner = area.inner(ratatui::layout::Margin::new(2, 1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 4])
        .split(inner);

    let width = inner.width as usize;
    let headline = if alert.spins {
        // the spinner costs two columns of the headline's budget
        format!(
            "{} {}",
            SPINNER[app.spinner % SPINNER.len()],
            truncate(&alert.headline, width.saturating_sub(2))
        )
    } else {
        truncate(&alert.headline, width)
    };
    frame.render_widget(
        Paragraph::new(headline)
            .alignment(Alignment::Center)
            .style(Style::default().bold()),
        rows[0],
    );
    if let Some((done, of)) = alert.bar {
        frame.render_widget(
            Paragraph::new(progress_bar(done, of, width)).style(Style::default().fg(colour)),
            rows[2],
        );
    }
    frame.render_widget(
        Paragraph::new(alert.hint)
            .alignment(Alignment::Center)
            .style(Style::default().fg(coloured(Color::Gray))),
        rows[3],
    );
}

fn progress_bar(done: usize, of: usize, width: usize) -> String {
    let filled = (done * width) / of.max(1);
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// a `w`x`h` box in the middle of `area`, shrunk to fit if it has to be
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, alert_up: bool) {
    // the sweep's spinner and the confirm both moved to the alert (ADR 0004);
    // what is left is the idle text and, on the right, the view-state keys
    let line = match &app.mode {
        Mode::Filter => Line::from(Span::styled(
            format!("filter: {}▏", app.filter),
            Style::default().fg(Color::Cyan),
        )),
        Mode::Normal | Mode::Confirm(_) | Mode::Help => {
            let mut spans = Vec::new();
            // An in-flight `d`/`e`/`r`/`u` is the one wait the alert does not
            // cover — ADR 0004 gives it three states, and none of them is an
            // action mid-flight. Without this the row would say "trashing 400
            // messages…" with nothing on screen moving behind it.
            if app.busy && !alert_up {
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
    // an alert refuses all four, and its hint line names the ones that do
    // respond — offering them here would be the UI saying two things (ADR 0001)
    if app.loading || alert_up {
        return;
    }
    // group before sort: it is the coarser control, so it is the one worth
    // keeping when a narrow terminal drops the tail
    let view_keys: &[(&str, &str)] = &[
        ("m", "more"),
        ("g", "group"),
        ("s", "sort"),
        ("Tab", "acct"),
    ];
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
///
/// A sweep refuses every one of these, and so does an alert of any kind, so
/// the row goes blank for its length rather than offer keys that do nothing
/// (ADR 0001). The alert's hint line names the ones that still respond.
fn draw_help(frame: &mut Frame, app: &App, area: Rect, alert_up: bool) {
    if app.loading || alert_up {
        return;
    }
    const HINTS: &[(&str, &str)] = &[
        ("j/k", "move"),
        ("Space", "mark"),
        ("d", "trash"),
        ("e", "archive"),
        ("r", "read"),
        ("u", "unsub"),
        ("/", "filter"),
    ];
    const HELP: (&str, &str) = ("?", "keys");

    let mut spans = vec![Span::raw(" ")];
    // reserve the tail for `? keys`: whatever else gets dropped, the way to
    // find it back must not be
    let budget = (area.width as usize).saturating_sub(1 + hint_width(HELP));
    spans.extend(hint_spans(HINTS, budget));
    spans.push(Span::raw("  "));
    spans.extend(hint_spans(&[HELP], usize::MAX));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// every binding, grouped, shown over a dimmed frame. any key dismisses.
///
/// `SECTIONS` is `const`, so the window size in the `m` row is a literal. This
/// makes it a build error rather than a silent drift if the constant moves.
fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    const _: () = assert!(
        WINDOW == 5000,
        "the `m` help row says 5,000; the README (#32) still says 40 senders"
    );
    const SECTIONS: [(&str, &[(&str, &str)]); 4] = [
        (
            "move",
            &[
                ("j / k, ↓ / ↑", "next / previous"),
                ("Home / End", "top / bottom (G = End)"),
                ("Esc", "clear marks, else clear filter"),
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
                ("m", "sweep 5,000 more messages"),
                ("g", "group by sender / subject"),
                ("s", "re-sort everything loaded"),
                ("/", "filter"),
                ("R", "reload from scratch"),
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
    use crate::ui::app::PendingAction;
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
        let cut = row.find("  m more").unwrap_or(row.len());
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
        let swept = msgs.len();
        app.account_mut().stacks = build_stacks(msgs, group_by, SortBy::Count);
        app.account_mut().total = swept;
        app.account_mut().back = swept;
        app.account_mut().reached_end = true;
        app.account_mut().loaded = true;
        app
    }

    /// the tab row, trailing blanks trimmed
    fn tab_row(app: &mut App, w: u16) -> String {
        render(app, w, MIN_HEIGHT)
            .lines()
            .next()
            .unwrap()
            .trim_end()
            .to_string()
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
        for hint in ["j/k move", "Space mark", "d trash", "r read", "u unsub"] {
            assert!(f.contains(hint), "{hint:?} missing from footer {f:?}");
        }
        assert!(f.ends_with("? keys"));

        let status = status_row(&mut app);
        for hint in ["m more", "g group", "s sort", "Tab acct"] {
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
        assert!(status_row(&mut app).contains("g group"));

        app.mode = Mode::Filter;
        app.filter = "doordash".into();
        let row = status_row(&mut app);
        assert!(row.starts_with("filter: doordash"));
        assert!(!row.contains("g group"), "prompt owns the row: {row:?}");
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

    /// the loaded set is a recency window, so a title reporting only what is
    /// loaded says "412" whether the mailbox holds 412 messages or 137,482
    #[test]
    fn the_pane_title_reports_the_mailbox_total_not_the_loaded_count() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.account_mut().total = 137_482;
        app.account_mut().back = 5_000;
        app.account_mut().reached_end = false;

        let title = render(&mut app, 200, MIN_HEIGHT)
            .lines()
            .nth(1)
            .unwrap()
            .to_string();
        assert!(
            title.contains("1 of 137,482 msgs"),
            "loaded-of-total lost: {title:?}"
        );
    }

    #[test]
    fn a_refused_senders_count_is_marked_short_rather_than_read_as_the_truth() {
        let mut app = app_with(
            vec![
                msg(1, "a@x.com", "Alice", "hi"),
                msg(2, "b@x.com", "Bob", "yo"),
            ],
            GroupBy::Sender,
        );
        app.account_mut().partial_senders.insert("a@x.com".into());

        let rows = stack_pane(&mut app, MIN_WIDTH, MIN_HEIGHT).join("\n");
        let alice = rows.lines().find(|r| r.contains("Alice")).unwrap();
        let bob = rows.lines().find(|r| r.contains("Bob")).unwrap();
        assert!(alice.contains("~1"), "no short-count marker: {alice:?}");
        assert!(!bob.contains('~'), "the others are unmarked: {bob:?}");
    }

    /// clearing the batch and clearing the inbox look identical otherwise, and
    /// the explanation belongs in the pane that actually drained
    #[test]
    fn the_emptied_stack_pane_says_whether_there_is_more_to_load() {
        let mut app = app_with(vec![], GroupBy::Sender);
        app.account_mut().total = 3;
        app.account_mut().reached_end = false;
        let pane = stack_pane(&mut app, MIN_WIDTH, MIN_HEIGHT).join(" ");
        assert!(pane.contains("press m to sweep"), "{pane:?}");

        app.account_mut().reached_end = true;
        let pane = stack_pane(&mut app, MIN_WIDTH, MIN_HEIGHT).join(" ");
        assert!(pane.contains("inbox zero"), "{pane:?}");
        assert!(!pane.contains("press m to sweep"));

        // a load that never landed is not an empty inbox
        app.account_mut().loaded = false;
        let pane = stack_pane(&mut app, MIN_WIDTH, MIN_HEIGHT).join(" ");
        assert!(pane.contains("press R"), "{pane:?}");
        app.loading = true;
        let pane = stack_pane(&mut app, MIN_WIDTH, MIN_HEIGHT).join(" ");
        assert!(pane.contains("loading"), "{pane:?}");
    }

    /// ADR 0001: a sweep refuses every key but `q` and ctrl-c. Leaving the
    /// hint rows up would have the UI offering keys it is about to swallow —
    /// the status line is where the ones that still respond get named.
    #[test]
    fn a_running_sweep_takes_down_the_hints_for_the_keys_it_refuses() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        assert!(footer(&mut app, 80).contains("trash"));
        assert!(status_row(&mut app).contains("m more"));

        app.loading = true;
        assert_eq!(footer(&mut app, 80), "", "no action keys during a sweep");
        assert!(
            !status_row(&mut app).contains("m more"),
            "nor the view keys: {}",
            status_row(&mut app)
        );
    }

    #[test]
    fn session_counters_stay_hidden_until_there_is_something_to_show() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        assert_eq!(tab_row(&mut app, 80), " mailprune   t");

        app.stats.trashed = 12;
        app.stats.archived = 40;
        app.stats.unsubscribed = 3;
        let row = tab_row(&mut app, 80);
        assert!(
            row.ends_with("12 trashed · 40 archived · 3 unsubbed"),
            "{row:?}"
        );
        assert!(row.starts_with(" mailprune "), "the tabs keep their place");
    }

    #[test]
    fn session_counters_abbreviate_before_they_disappear() {
        use super::super::app::SessionStats;
        let stats = SessionStats {
            trashed: 12,
            archived: 40,
            marked_read: 0,
            unsubscribed: 3,
        };
        assert_eq!(
            session_counters(&stats, 80).unwrap(),
            "12 trashed · 40 archived · 3 unsubbed",
            "zero-valued counters are omitted"
        );
        assert_eq!(session_counters(&stats, 20).unwrap(), "12d · 40e · 3u");
        assert_eq!(session_counters(&stats, 5), None, "no room means nothing");
        assert_eq!(session_counters(&SessionStats::default(), 80), None);
    }

    #[test]
    fn six_figure_totals_are_grouped() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(412), "412");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(137_482), "137,482");
    }

    /// the frame's rendered buffer at this size
    fn buffer(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// every box-drawing corner in a rendered frame
    fn corners(app: &mut App, w: u16, h: u16) -> Vec<(u16, u16)> {
        let buf = buffer(app, w, h);
        let mut found = Vec::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if matches!(buf[(x, y)].symbol(), "┌" | "┘") {
                    found.push((x, y));
                }
            }
        }
        found
    }

    /// Where the alert sits, found by diffing against the same frame with the
    /// alert taken down — the corners it adds are the box. Nothing here reads
    /// the layout constants, so the geometry assertions are not circular.
    fn alert_rect(app: &mut App, w: u16, h: u16) -> Rect {
        let with = corners(app, w, h);
        let mode = std::mem::replace(&mut app.mode, Mode::Normal);
        let alert = app.alert.take();
        let without = corners(app, w, h);
        app.mode = mode;
        app.alert = alert;

        let added: Vec<(u16, u16)> = with.into_iter().filter(|c| !without.contains(c)).collect();
        let tl = *added
            .iter()
            .min_by_key(|(x, y)| (*y, *x))
            .expect("the alert drew no box");
        let br = *added
            .iter()
            .max_by_key(|(x, y)| (*y, *x))
            .expect("the alert drew no box");
        Rect {
            x: tl.0,
            y: tl.1,
            width: br.0 - tl.0 + 1,
            height: br.1 - tl.1 + 1,
        }
    }

    /// the alert box's own rows, joined — nothing of the app behind it
    fn alert_at(app: &mut App, w: u16, h: u16) -> String {
        let r = alert_rect(app, w, h);
        let buf = buffer(app, w, h);
        (r.y..r.y + r.height)
            .map(|y| {
                (r.x..r.x + r.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn alert(app: &mut App) -> String {
        alert_at(app, 100, 30)
    }

    fn sweeping(swept: usize, stacks: usize) -> Alert {
        Alert::Sweeping {
            starting: "connecting to me@x.com…".into(),
            progress: Some(crate::imap_client::SweepProgress {
                swept,
                bound: 5_000,
                stacks,
            }),
        }
    }

    /// ADR 0004: one slot, one size, one position — the thing the user learns
    /// is where to look, and that must not depend on which state came up.
    #[test]
    fn the_three_alert_states_share_one_frame_in_one_position() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);

        /// one row of ADR 0004's table, plus the state that produces it
        struct Case {
            title: &'static str,
            headline: &'static str,
            hint: &'static str,
            set: fn(&mut App),
        }
        let states = [
            Case {
                title: "sweeping",
                headline: "3,000 of 5,000 · 41 stacks",
                hint: "q quit",
                set: |app| {
                    app.loading = true;
                    app.mode = Mode::Normal;
                    app.alert = Some(sweeping(3_000, 41));
                },
            },
            Case {
                title: "sweep failed",
                headline: "stopped at 2,400 of 5,000",
                hint: "m retry · any key to continue",
                set: |app| {
                    app.loading = false;
                    app.mode = Mode::Normal;
                    app.alert = Some(Alert::Failed("stopped at 2,400 of 5,000".into()));
                },
            },
            Case {
                title: "confirm",
                headline: "trash 1 message from Alice?",
                hint: "y yes · n no",
                set: |app| {
                    app.loading = false;
                    app.alert = None;
                    app.mode = Mode::Confirm(PendingAction::Trash {
                        stack_idxs: vec![0],
                    });
                },
            },
        ];

        let mut geometry = Vec::new();
        for Case {
            title,
            headline,
            hint,
            set,
        } in states
        {
            set(&mut app);
            let frame = alert(&mut app);
            assert!(frame.contains(title), "no title {title:?} in\n{frame}");
            assert!(frame.contains(headline), "no headline in\n{frame}");
            assert!(frame.contains(hint), "no hint in\n{frame}");
            geometry.push(alert_rect(&mut app, 100, 30));
        }
        assert_eq!(geometry[0], geometry[1], "sweeping vs failed moved the box");
        assert_eq!(geometry[0], geometry[2], "the confirm moved the box");
        assert_eq!(geometry[0].width, ALERT_W);
        assert_eq!(geometry[0].height, ALERT_H);
    }

    /// the bar only rides along when there is progress behind it; a static bar
    /// on a confirm would read as a stalled sweep
    #[test]
    fn only_the_sweeping_alert_carries_a_progress_bar() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.loading = true;
        app.alert = Some(sweeping(2_500, 20));
        let sweeping_box = alert(&mut app);
        assert!(sweeping_box.contains('█'), "no bar:\n{sweeping_box}");
        assert!(sweeping_box.contains('░'), "no remainder:\n{sweeping_box}");

        app.loading = false;
        app.alert = Some(Alert::Failed("stopped at 2,400 of 5,000".into()));
        let failed = alert(&mut app);
        assert!(
            !failed.contains('█'),
            "a failure has no progress:\n{failed}"
        );
    }

    /// the bar carries progress; the spinner carries liveness. A bar that has
    /// not moved for two seconds is ambiguous in a way a stopped spinner isn't.
    #[test]
    fn the_spinner_keeps_turning_next_to_the_bar() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.loading = true;
        app.alert = Some(sweeping(3_000, 41));

        let first = alert(&mut app);
        app.tick_spinner();
        let second = alert(&mut app);
        assert_ne!(first, second, "the spinner frame did not advance");
        assert!(
            SPINNER.iter().any(|s| first.contains(s)),
            "no spinner beside the bar:\n{first}"
        );
    }

    /// The moment with nothing to put in the bar is the moment the spinner is
    /// carrying the whole box, so it cannot be keyed off the bar: a sweep that
    /// has not reported its first chunk would sit there wholly static, over an
    /// empty screen, which is the "reads as a crash" ADR 0004 rules out.
    #[test]
    fn the_alert_spins_before_the_first_chunk_gives_it_a_bar() {
        let mut app = app_with(vec![], GroupBy::Sender);
        app.loading = true;
        app.alert = Some(Alert::Sweeping {
            starting: "connecting to me@x.com…".into(),
            progress: None,
        });

        let first = alert(&mut app);
        app.tick_spinner();
        assert!(!first.contains('█'), "no bar yet:\n{first}");
        assert!(
            SPINNER.iter().any(|s| first.contains(s)),
            "a barless sweep must still move:\n{first}"
        );
        assert_ne!(first, alert(&mut app), "the frame did not advance");
    }

    /// the states the user is answering are not states the user is waiting on
    #[test]
    fn the_alert_holds_still_when_it_is_waiting_on_the_user() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        for state in ["failed", "confirm"] {
            if state == "failed" {
                app.alert = Some(Alert::Failed("stopped at 2,400 of 5,000".into()));
            } else {
                app.alert = None;
                app.mode = Mode::Confirm(PendingAction::Trash {
                    stack_idxs: vec![0],
                });
            }
            let before = alert(&mut app);
            app.tick_spinner();
            assert_eq!(before, alert(&mut app), "{state} should not spin");
            assert!(
                !SPINNER.iter().any(|s| before.contains(s)),
                "{state} has no spinner:\n{before}"
            );
        }
    }

    /// An in-flight `d`/`e`/`r`/`u` is the one wait with no alert behind it —
    /// ADR 0004 gives the box three states and none of them is an action
    /// mid-flight, so the status row keeps a spinner for exactly that case.
    #[test]
    fn an_action_in_flight_keeps_its_spinner_on_the_status_row() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.status = "trashing 400 messages…".into();
        assert_eq!(status_text(&mut app), "trashing 400 messages…");

        app.busy = true;
        let spun = status_text(&mut app);
        assert!(spun.ends_with("trashing 400 messages…"), "{spun:?}");
        assert!(
            SPINNER.iter().any(|s| spun.starts_with(s)),
            "nothing on screen moves during a trash: {spun:?}"
        );
        app.tick_spinner();
        assert_ne!(spun, status_text(&mut app), "the frame advances");
    }

    /// ADR 0004: the box is 60 columns because the first sweep draws it over a
    /// blank screen, where a small box reads as a crash rather than as progress
    #[test]
    fn the_first_sweep_draws_the_alert_at_full_size_over_an_empty_stack_pane() {
        let mut app = app_with(vec![], GroupBy::Sender);
        app.accounts[0].loaded = false;
        app.loading = true;
        app.alert = Some(Alert::Sweeping {
            starting: "connecting to me@x.com…".into(),
            progress: None,
        });

        let rect = alert_rect(&mut app, 100, 30);
        assert_eq!((rect.width, rect.height), (ALERT_W, ALERT_H), "box shrank");
        let frame = alert(&mut app);
        assert!(
            frame.contains("connecting to me@x.com…"),
            "before the first chunk the alert still says what it is doing:\n{frame}"
        );
    }

    #[test]
    fn the_alert_still_fits_at_80x24() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.loading = true;
        app.alert = Some(sweeping(3_000, 41));

        let rect = alert_rect(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert_eq!(rect.height, ALERT_H, "the box lost rows at 80x24");
        assert!(
            rect.x + rect.width <= MIN_WIDTH && rect.y + rect.height <= MIN_HEIGHT,
            "the box ran off an 80x24 screen: {rect:?}"
        );
        let frame = alert_at(&mut app, MIN_WIDTH, MIN_HEIGHT);
        assert!(frame.contains("3,000 of 5,000 · 41 stacks"), "{frame}");
        assert!(frame.contains("q quit"), "{frame}");
    }

    /// the style of every cell outside `r`
    fn outside(app: &mut App, r: Rect) -> Vec<Style> {
        let buf = buffer(app, 100, 30);
        let mut styles = Vec::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let inside = x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
                if !inside {
                    styles.push(buf[(x, y)].style());
                }
            }
        }
        styles
    }

    /// the same cells with nothing in the alert slot — the baseline the
    /// repaint is measured against
    fn undisturbed(app: &mut App, r: Rect) -> Vec<Style> {
        let alert = app.alert.take();
        let styles = outside(app, r);
        app.alert = alert;
        styles
    }

    /// ADR 0004: the foreground behind the alert is overwritten, not `DIM`ed —
    /// terminals are free to ignore `DIM`
    #[test]
    fn the_screen_behind_the_alert_is_greyed_by_colour_not_by_the_dim_bit() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.loading = true;
        app.alert = Some(sweeping(3_000, 41));
        let r = alert_rect(&mut app, 100, 30);

        let behind = outside(&mut app, r);
        assert!(
            behind.iter().all(|s| s.fg == Some(MUTED)),
            "something behind the alert kept its own colour"
        );
        assert!(
            behind
                .iter()
                .all(|s| !s.add_modifier.contains(Modifier::DIM)),
            "DIM is not the mechanism"
        );
        assert_ne!(behind, undisturbed(&mut app, r), "nothing was repainted");
    }

    /// Under NO_COLOR the border and the `Clear` carry the whole job, which is
    /// the reason the box is large. #5 takes this over with the theme module.
    #[test]
    fn no_color_drops_the_dimming_and_the_alerts_colour() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.no_color = true;
        app.loading = true;
        app.alert = Some(sweeping(3_000, 41));
        let r = alert_rect(&mut app, 100, 30);

        assert_eq!(
            outside(&mut app, r),
            undisturbed(&mut app, r),
            "NO_COLOR must leave what is behind the alert exactly as it was"
        );

        let frame = alert(&mut app);
        assert!(
            frame.contains("sweeping"),
            "the box is still there:\n{frame}"
        );
        assert!(frame.contains("3,000 of 5,000 · 41 stacks"), "{frame}");

        // and the box itself carries no colour of its own
        let buf = buffer(&mut app, 100, 30);
        let border: Vec<Style> = (r.x..r.x + r.width)
            .map(|x| buf[(x, r.y)].style())
            .collect();
        assert!(
            border
                .iter()
                .all(|s| s.fg.is_none_or(|c| c == Color::Reset)),
            "the border kept a colour under NO_COLOR: {border:?}"
        );
    }

    /// An alert answers the next key itself, whichever it is. Leaving the hint
    /// rows up under it would have the UI offering keys it is about to swallow
    /// — the same thing ADR 0001 takes them down for during a sweep.
    #[test]
    fn an_alert_takes_down_the_hint_rows_for_the_keys_it_swallows() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        assert!(footer(&mut app, 80).contains("trash"));
        assert!(status_row(&mut app).contains("m more"));

        // a failed sweep is not `loading`, so only the alert can take them down
        app.alert = Some(Alert::Failed("stopped at 2,400 of 5,000".into()));
        assert_eq!(footer(&mut app, 80), "", "no action keys under the alert");
        assert!(
            !status_row(&mut app).contains("m more"),
            "nor the view keys"
        );

        app.alert = None;
        app.mode = Mode::Confirm(PendingAction::Trash {
            stack_idxs: vec![0],
        });
        assert_eq!(footer(&mut app, 80), "", "nor under a confirm");
    }

    /// ADR 0004: the spinner and the confirm both leave the status row. What
    /// stays is the idle text and the view-state keys.
    #[test]
    fn the_status_row_keeps_only_its_idle_text() {
        let mut app = app_with(vec![msg(1, "a@x.com", "Alice", "hi")], GroupBy::Sender);
        app.status = "me@x.com: 1 of 1 messages in 1 stacks".into();
        assert_eq!(
            status_text(&mut app),
            "me@x.com: 1 of 1 messages in 1 stacks"
        );

        // a sweep's spinner is the alert's; the row keeps only its own text
        app.loading = true;
        app.alert = Some(sweeping(3_000, 41));
        assert_eq!(
            status_text(&mut app),
            "me@x.com: 1 of 1 messages in 1 stacks"
        );

        app.loading = false;
        app.alert = None;
        app.mode = Mode::Confirm(PendingAction::Trash {
            stack_idxs: vec![0],
        });
        let row = status_row(&mut app);
        assert!(!row.contains("[y/n]"), "the confirm moved too: {row:?}");
        assert!(!row.contains("g group"), "a prompt still owns the row");
    }
}

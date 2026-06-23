//! Rendering: a title bar, the scrollable workspace/window list, and a footer
//! that doubles as the status line and prompt input.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Mode, Row, Win};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Min(1),    // list
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_title(f, chunks[0], app);
    draw_list(f, chunks[1], app);
    draw_footer(f, chunks[2], app);

    if matches!(app.mode, Mode::Help) {
        draw_help(f, f.area());
    }
    if let Mode::ConfirmKill { label, .. } = &app.mode {
        draw_confirm(f, f.area(), label);
    }
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let conn = if app.connected {
        Span::styled(" ● ", Style::default().fg(Color::Green))
    } else {
        Span::styled(" ○ ", Style::default().fg(Color::Red))
    };
    let line = Line::from(vec![
        Span::styled(
            " shoestring-tasks ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        conn,
        Span::raw(if app.connected {
            "live"
        } else {
            "no WM on socket"
        }),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_list(f: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app.rows.iter().map(row_item).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn row_item(row: &Row) -> ListItem<'static> {
    match row {
        Row::Workspace {
            index,
            name,
            focused,
            count,
        } => {
            let title = if name.is_empty() {
                format!("Workspace {index}")
            } else {
                format!("Workspace {index} · {name}")
            };
            let style = if *focused {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD)
            };
            let marker = if *focused { "▌" } else { "│" };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} {title} "), style),
                Span::styled(format!("({count})"), Style::default().fg(Color::DarkGray)),
            ]))
        }
        Row::Minimized { count } => ListItem::new(Line::from(Span::styled(
            format!("│ Minimized ({count})"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))),
        Row::Window(w) => ListItem::new(window_line(w)),
    }
}

fn window_line(w: &Win) -> Line<'static> {
    let mut spans = vec![Span::raw("    ")];
    spans.push(Span::styled(flags(w), Style::default().fg(Color::Green)));

    let title = if w.title.is_empty() {
        "(untitled)".to_string()
    } else {
        w.title.clone()
    };
    let title_style = if w.focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else if w.minimized {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    spans.push(Span::styled(title, title_style));

    if !w.app_id.is_empty() {
        spans.push(Span::styled(
            format!("  {}", w.app_id),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// A fixed-width flag column so titles line up: focus, minimized, maximized,
/// sticky, always-on-top.
fn flags(w: &Win) -> String {
    let mut s = String::new();
    s.push(if w.focused { '*' } else { ' ' });
    s.push(if w.minimized {
        'm'
    } else if w.maximized {
        '□'
    } else {
        ' '
    });
    s.push(if w.sticky { 'S' } else { ' ' });
    s.push(if w.always_on_top { '^' } else { ' ' });
    s.push(' ');
    s
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let line = match &app.mode {
        Mode::Rename { buffer, .. } => Line::from(vec![
            Span::styled("rename: ", Style::default().fg(Color::Cyan)),
            Span::raw(buffer.clone()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]),
        Mode::MoveTo { buffer, .. } => Line::from(vec![
            Span::styled("move to workspace: ", Style::default().fg(Color::Cyan)),
            Span::raw(buffer.clone()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]),
        _ => Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Gray),
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_confirm(f: &mut Frame, area: Rect, label: &str) {
    let popup = centered(area, 60, 5);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Force-kill ")
        .style(Style::default().fg(Color::Red));
    let text = vec![
        Line::from(format!("SIGKILL the process owning {label}?")),
        Line::from(""),
        Line::from(Span::styled(
            "y = kill   ·   any other key = cancel",
            Style::default().fg(Color::Gray),
        )),
    ];
    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
        popup,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let popup = centered(area, 64, 22);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Keys — any key closes ")
        .style(Style::default().fg(Color::Cyan));
    let rows = [
        ("j / k, ↓ / ↑", "move selection"),
        ("g / G", "first / last window"),
        ("Enter", "focus window (switches workspace)"),
        ("x", "close window (polite)"),
        ("X", "force-kill window (confirm)"),
        ("r", "rename window (empty clears override)"),
        ("m / M", "toggle minimize / maximize"),
        ("t / a", "toggle sticky / always-on-top"),
        ("R / L", "raise / lower in stacking order"),
        ("s", "screenshot window (needs capture gate)"),
        ("1-9, 0", "move to workspace N (0 = 10)"),
        ("w / >", "move to workspace (prompt, 1-16)"),
        ("z", "show/hide empty workspaces"),
        ("F5", "refresh now"),
        ("? ", "this help"),
        ("q / Esc / Ctrl-C", "quit"),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (k, v) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<18}"), Style::default().fg(Color::Yellow)),
            Span::raw(v),
        ]));
    }
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// A centered rectangle `w`×`h` (clamped to `area`).
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    Rect::new(x, y, w, h)
}

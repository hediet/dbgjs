use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::app::{App, OutlineRow, Tab, Tone};

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let [summary, tabs, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(8),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    render_summary(frame, app, summary);
    render_tabs(frame, app, tabs);
    render_body(frame, app, body);
    render_footer(frame, app, footer);
}

fn render_summary(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (context_index, context_count) = app.context_position();
    let (connected, targets, debugger_targets) = app.context().map_or((0, 0, 0), |context| {
        (
            context
                .connections
                .iter()
                .filter(|connection| {
                    matches!(
                        connection.status,
                        cdp_client::service_api::ConnectionStatus::Connected { .. }
                    )
                })
                .count(),
            context.target_forest.len(),
            context
                .target_forest
                .iter()
                .filter(|target| {
                    target.attachment == cdp_client::service_api::TargetAttachmentState::Debugger
                })
                .count(),
        )
    });
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", app.context_name()),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[{context_index}/{context_count}]  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("● {connected} connected  "),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("{targets} targets · {debugger_targets} attached"),
            Style::default().fg(Color::Gray),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn render_tabs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let titles = Tab::ALL
        .iter()
        .map(|tab| Line::from(format!(" {} ", tab.title())))
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(app.tab().index())
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
            .divider(" "),
        area,
    );
}

fn render_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let areas = if area.width >= 92 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area)
    };
    render_outline(frame, app, areas[0]);
    render_inspector(frame, app, areas[1]);
}

fn render_outline(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app.rows();
    let items = rows
        .iter()
        .map(|row| ListItem::new(outline_line(row, area.width.saturating_sub(4) as usize)))
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(app.selected_index());
    let title = if app.tab() == Tab::Sources {
        format!(" {} · {} ", app.tab().title(), app.source_kind_label())
    } else {
        format!(" {} ", app.tab().title())
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(31, 59, 86))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn outline_line(row: &OutlineRow, width: usize) -> Line<'static> {
    let marker = if row.expandable {
        if row.expanded { "▾" } else { "▸" }
    } else {
        " "
    };
    let indent = "  ".repeat(row.depth);
    let prefix = format!("{indent}{marker} {}", row.label);
    let suffix = if row.detail.is_empty() {
        row.state.clone()
    } else {
        format!("{}  {}", row.state, row.detail)
    };
    let gap = width.saturating_sub(prefix.chars().count() + suffix.chars().count());
    Line::from(vec![
        Span::styled(prefix, tone_style(row.tone)),
        Span::raw(" ".repeat(gap.max(1))),
        Span::styled(suffix, tone_style(row.tone)),
    ])
}

fn render_inspector(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let inspector = app.inspector();
    let lines = inspector
        .lines
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .title(format!(" {} ", inspector.title))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let message = app.message().map_or_else(
        || {
            if app.is_loading(app.tab()) {
                let label = if app.tab() == Tab::Sources {
                    format!("{} sources", app.source_kind_label())
                } else {
                    app.tab().title().to_lowercase()
                };
                Line::from(Span::styled(
                    format!("Loading {label}…"),
                    Style::default().fg(Color::Yellow),
                ))
            } else {
                Line::from(Span::styled(
                    app.footer_hint(),
                    Style::default().fg(Color::DarkGray),
                ))
            }
        },
        |(tone, message)| Line::from(Span::styled(message, tone_style(tone))),
    );
    let text = Text::from(vec![
        Line::from(vec![
            key("Tab"),
            Span::raw(" switch  "),
            key("j/k"),
            Span::raw(" move  "),
            key("h/l"),
            Span::raw(" collapse/expand  "),
            key("Space"),
            Span::raw(" toggle  "),
            key("m"),
            Span::raw(" sources  "),
            key("[/]"),
            Span::raw(" context  "),
            key("r"),
            Span::raw(" refresh  "),
            key("q"),
            Span::raw(" quit"),
        ]),
        message,
    ]);
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn key(value: &'static str) -> Span<'static> {
    Span::styled(
        value,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Normal => Style::default().fg(Color::Gray),
        Tone::Muted => Style::default().fg(Color::DarkGray),
        Tone::Good => Style::default().fg(Color::Green),
        Tone::Warning => Style::default().fg(Color::Yellow),
        Tone::Error => Style::default().fg(Color::Red),
    }
}

#[cfg(test)]
mod tests {
    use cdp_client::context_identity::ContextKind;
    use cdp_client::service_api::{
        ConnectionConfiguration, ConnectionSnapshot, ConnectionStatus, ContextSnapshot,
        ContextSummary, SourceFormattingSettings,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn renders_second_row_tabs_and_connection_toggle() {
        let context = ContextSnapshot {
            agent_instance_id: "agent".to_owned(),
            id: "ctx".to_owned(),
            display_name: "Shop".to_owned(),
            revision: 1,
            resource_revision: 1,
            connections: vec![ConnectionSnapshot {
                id: "browser".to_owned(),
                configuration: ConnectionConfiguration::DirectCdp {
                    endpoint: "ws://example".to_owned(),
                },
                generation: 1,
                status: ConnectionStatus::Disconnected,
                targets: Vec::new(),
            }],
            target_forest: Vec::new(),
            breakpoints: Vec::new(),
            source_formatting: SourceFormattingSettings::default(),
        };
        let mut app = App::new(
            vec![ContextSummary {
                agent_instance_id: "agent".to_owned(),
                id: "ctx".to_owned(),
                kind: ContextKind::Named,
                path_distance: None,
                path_ancestor: None,
                display_name: "Shop".to_owned(),
                revision: 1,
                connection_count: 1,
                breakpoint_count: 0,
            }],
            0,
            context,
        );
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Processes"));
        assert!(screen.contains("Connections"));
        assert!(screen.contains("Targets"));
        assert!(screen.contains("[ ] browser"));
        assert!(screen.contains("Space toggles connect/disconnect"));
        assert!(screen.contains("d or Delete removes this connection"));

        app.set_tab(Tab::Processes);
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let stable_tabs = buffer_row(terminal.backend().buffer(), 2);
        assert!(app.begin_loading(Tab::Processes));
        terminal.draw(|frame| render(frame, &app)).unwrap();
        assert_eq!(buffer_row(terminal.backend().buffer(), 2), stable_tabs);
        let loading_screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(loading_screen.contains("Loading processes…"));

        app.finish_loading(Tab::Processes);
        app.set_tab(Tab::Sources);
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let source_screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(source_screen.contains("Sources · source maps"));
        assert!(source_screen.contains("m cycle projection"));
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }
}

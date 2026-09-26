use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::app::{App, OutlineRow, Section, Tab, Tone};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
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
                        dbgjs::service_api::ConnectionStatus::Connected { .. }
                    )
                })
                .count(),
            context.target_forest.len(),
            context
                .target_forest
                .iter()
                .filter(|target| {
                    target.attachment == dbgjs::service_api::TargetAttachmentState::Debugger
                })
                .count(),
        )
    });
    let paused = app.paused_count();
    let issues = app.issue_count();
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
            format!("! {issues}  "),
            if issues == 0 {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Red)
            },
        ),
        Span::styled(
            format!("⏸ {paused}  "),
            if paused == 0 {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Yellow)
            },
        ),
        Span::styled(
            format!("Scope: {}  ", app.scope_label()),
            Style::default().fg(Color::Cyan),
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

fn render_body(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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
    render_sidebar(frame, app, areas[0]);
    render_detail(frame, app, areas[1]);
}

fn render_sidebar(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Block::default()
        .title(format!(" {} ", app.tab().title()))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = app.sections().to_vec();
    let expanded_count = sections
        .iter()
        .filter(|section| !app.is_section_collapsed(**section))
        .count();
    let available_body = usize::from(inner.height).saturating_sub(sections.len());
    let mut body_remainder = if expanded_count == 0 {
        0
    } else {
        available_body % expanded_count
    };
    let body_base = if expanded_count == 0 {
        0
    } else {
        available_body / expanded_count
    };
    let mut y = inner.y;
    for section in sections {
        let header_area = Rect::new(inner.x, y, inner.width, 1);
        render_section_header(frame, app, section, header_area);
        y = y.saturating_add(1);
        if app.is_section_collapsed(section) {
            app.set_section_viewport(section, 0);
            continue;
        }
        let extra = usize::from(body_remainder > 0);
        body_remainder = body_remainder.saturating_sub(extra);
        let body_height = (body_base + extra).min(usize::from(u16::MAX)) as u16;
        let body_area = Rect::new(inner.x, y, inner.width, body_height);
        render_section_body(frame, app, section, body_area);
        y = y.saturating_add(body_height);
    }
}

fn render_section_header(frame: &mut Frame<'_>, app: &App, section: Section, area: Rect) {
    let collapsed = app.is_section_collapsed(section);
    let marker = if collapsed { "›" } else { "⌄" };
    let status = app.section_header_status(section);
    let section_title = if section == Section::Sources {
        format!("{} · {}", section.title(), app.source_kind_label())
    } else {
        section.title().to_owned()
    };
    let title = format!("{marker} {section_title}");
    let gap = usize::from(area.width)
        .saturating_sub(title.chars().count() + status.chars().count())
        .max(1);
    let style = if app.is_header_selected(section) {
        Style::default()
            .bg(Color::Rgb(31, 59, 86))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(Color::Rgb(37, 37, 38))
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Line::from(format!("{title}{}{status}", " ".repeat(gap)))).style(style),
        area,
    );
}

fn render_section_body(frame: &mut Frame<'_>, app: &mut App, section: Section, area: Rect) {
    app.set_section_viewport(section, usize::from(area.height));
    let rows = app.rows(section);
    if rows.is_empty() {
        let message = if app.is_loading(section) {
            "TUI is querying the debugger service…".to_owned()
        } else if matches!(
            section,
            Section::Contexts
                | Section::Processes
                | Section::Connections
                | Section::Sources
                | Section::Captures
        ) && !app.section_is_queried(section)
        {
            "TUI has not received a result yet.".to_owned()
        } else if section == Section::Attention {
            "No paused, failed, or ownership-conflict items.".to_owned()
        } else {
            format!(
                "No {} reported for this scope.",
                section.title().to_lowercase()
            )
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    let offset = app
        .section_scroll(section)
        .min(rows.len().saturating_sub(1));
    let end = offset
        .saturating_add(usize::from(area.height))
        .min(rows.len());
    let items = rows[offset..end]
        .iter()
        .map(|row| ListItem::new(outline_line(row, area.width.saturating_sub(1) as usize)))
        .collect::<Vec<_>>();
    let selected = app.selected_index(section).and_then(|selected| {
        (offset..end)
            .contains(&selected)
            .then_some(selected - offset)
    });
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items).highlight_style(
            Style::default()
                .bg(Color::Rgb(31, 59, 86))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        area,
        &mut state,
    );
}

fn outline_line(row: &OutlineRow, width: usize) -> Line<'static> {
    let marker = if row.expandable {
        if row.expanded { "⌄" } else { "›" }
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

fn render_detail(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if app.tab() == Tab::Debug
        && let Some(source) = app.source_content().cloned()
    {
        render_source(frame, app, &source, area);
        return;
    }
    render_inspector(frame, app, area);
}

fn render_source(
    frame: &mut Frame<'_>,
    app: &mut App,
    source: &dbgjs::service_api::SourceContentSnapshot,
    area: Rect,
) {
    app.set_source_viewport(usize::from(area.height.saturating_sub(2)));
    let focused = app.document_focused();
    let selected_line = app.source_line();
    let source_scroll = app.source_scroll();
    let source_viewport = usize::from(area.height.saturating_sub(2));
    let lines = source
        .content
        .lines()
        .skip(source_scroll)
        .take(source_viewport)
        .enumerate()
        .map(|(index, text)| {
            let line = source.start_line + source_scroll as u32 + index as u32;
            let breakpoint = if app.has_breakpoint(&source.path, line) {
                "●"
            } else {
                " "
            };
            let style = if focused && line == selected_line {
                Style::default().bg(Color::Rgb(31, 59, 86)).fg(Color::White)
            } else {
                Style::default()
            };
            Line::styled(format!("{breakpoint} {line:>5}  {text}"), style)
        })
        .collect::<Vec<_>>();
    let focus = if focused { " · focused" } else { "" };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .title(format!(
                    " {} · {}{focus} ",
                    source.path,
                    app.source_kind_label()
                ))
                .borders(Borders::ALL),
        ),
        area,
    );
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
            let loading = app
                .sections()
                .iter()
                .filter(|section| app.is_loading(**section))
                .map(|section| section.title().to_lowercase())
                .collect::<Vec<_>>();
            if !loading.is_empty() {
                Line::from(Span::styled(
                    format!("TUI querying {}…", loading.join(", ")),
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
            Span::raw(" parent/child  "),
            key("Space"),
            Span::raw(" collapse/expand  "),
            key("c"),
            Span::raw(" connect  "),
            key("a"),
            Span::raw(" attach  "),
            key("m"),
            Span::raw(" projection  "),
            key("b"),
            Span::raw(" breakpoint  "),
            key("[/]"),
            Span::raw(" context  "),
            key("r"),
            Span::raw(" query  "),
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
    use dbgjs::context_identity::ContextKind;
    use dbgjs::service_api::{
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

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

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
        assert!(screen.contains("Runtime"));
        assert!(screen.contains("Scope: context runtime"));
        assert!(screen.contains("TUI: expand to query"));
        assert!(screen.contains("› Processes"));
        assert!(screen.contains("› Connections"));
        assert!(screen.contains("› Targets"));
        assert!(!screen.contains("> Processes"));

        let mut narrow_terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        narrow_terminal
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let narrow_summary = buffer_row(narrow_terminal.backend().buffer(), 0);
        assert!(narrow_summary.contains("! 0"));
        assert!(narrow_summary.contains("⏸ 0"));

        app.toggle_expanded();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let stable_tabs = buffer_row(terminal.backend().buffer(), 2);
        assert!(app.begin_loading(Section::Processes));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(buffer_row(terminal.backend().buffer(), 2), stable_tabs);
        let loading_screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(loading_screen.contains("TUI querying processes…"));

        app.finish_loading(Section::Processes);
        app.set_tab(Tab::Debug);
        app.move_selection(1);
        app.toggle_expanded();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let source_screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(source_screen.contains("⌄ Sources · source maps"));
        assert!(source_screen.contains("m projection"));
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }
}

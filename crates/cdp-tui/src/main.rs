mod app;
mod service;
mod ui;

use std::io::{self, stdout};
use std::time::Duration;

use app::{App, OutlineItem, Section, Tab};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use service::{Bootstrap, Data, ServiceController, ServiceEvent};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("jsdbg-tui: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let arguments = Arguments::parse(std::env::args().skip(1))?;
    if arguments.help {
        println!("{}", usage());
        return Ok(());
    }
    let bootstrap = Bootstrap::connect(arguments.context.as_deref()).await?;
    let mut terminal = TerminalSession::open().map_err(|error| error.to_string())?;
    let (service_events, mut service_event_rx) = mpsc::channel(64);
    let mut service = ServiceController::new(bootstrap.client, service_events, bootstrap.cwd);
    service.observe_context_after(bootstrap.context.id.clone(), bootstrap.context.revision);
    let mut app = App::new(
        bootstrap.contexts,
        bootstrap.context_index,
        bootstrap.context,
    );
    let mut input = EventStream::new();
    let mut refresh = tokio::time::interval(Duration::from_secs(2));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed_targets = Vec::new();
    let mut selected_source = None;
    request_active_data(&mut app, &mut service);

    loop {
        terminal
            .terminal
            .draw(|frame| ui::render(frame, &mut app))
            .map_err(|error| error.to_string())?;

        tokio::select! {
            event = input.next() => {
                let Some(event) = event else {
                    break;
                };
                let event = event.map_err(|error| error.to_string())?;
                if let Event::Key(key) = event
                    && key.kind != KeyEventKind::Release
                    && handle_key(key, &mut app, &mut service)
                {
                    break;
                }
            }
            Some(event) = service_event_rx.recv() => {
                handle_service_event(event, &mut app, &service);
            }
            _ = refresh.tick() => {
                request_active_data(&mut app, &mut service);
            }
        }

        let next_targets = app.observed_targets();
        if next_targets != observed_targets {
            observed_targets = next_targets.clone();
            service.set_targets(next_targets);
        }
        let next_source = app.selected_source().map(|path| {
            let revision = app.source_revision_token(&path);
            (
                app.context_id().to_owned(),
                path,
                app.source_kind(),
                revision,
            )
        });
        if next_source != selected_source {
            selected_source = next_source.clone();
            app.set_source_content(None);
            service.set_source(
                next_source
                    .map(|(context_id, path, source_kind, _)| (context_id, path, source_kind)),
            );
        }
    }
    Ok(())
}

fn handle_key(key: KeyEvent, app: &mut App, service: &mut ServiceController) -> bool {
    match key.code {
        KeyCode::Esc if app.document_focused() => app.leave_document(),
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => app.next_tab(-1),
        KeyCode::Tab => app.next_tab(1),
        KeyCode::BackTab => app.next_tab(-1),
        KeyCode::Char(character @ '1'..='2') => {
            app.set_tab(Tab::ALL[(character as u8 - b'1') as usize]);
        }
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Home | KeyCode::Char('g') => app.select_edge(false),
        KeyCode::End | KeyCode::Char('G') => app.select_edge(true),
        KeyCode::Left | KeyCode::Char('h') => app.navigate_left(),
        KeyCode::Right | KeyCode::Char('l') => app.navigate_right(),
        KeyCode::Enter => {
            if let Some(context_id) = app.selected_context_id().map(str::to_owned)
                && let Some(context_id) = app.select_context_id(&context_id)
            {
                service.switch_context(context_id);
            } else {
                app.open_selected();
            }
        }
        KeyCode::Char(' ')
            if app.selected_section() == Section::Processes
                && matches!(
                    app.selected_row().map(|row| row.item),
                    Some(OutlineItem::Process { .. } | OutlineItem::ProcessWindow { .. })
                ) =>
        {
            perform_connection_configuration_toggle(app, service);
        }
        KeyCode::Char(' ') => app.toggle_expanded(),
        KeyCode::Char('c') => perform_connect(app, service),
        KeyCode::Char('a') if app.selected_section() == Section::Targets => {
            perform_target_attachment(app, service, false);
        }
        KeyCode::Char('f') if app.selected_section() == Section::Targets => {
            perform_target_attachment(app, service, true);
        }
        KeyCode::Char('d') | KeyCode::Delete if app.selected_section() == Section::Connections => {
            perform_delete_connection(app, service);
        }
        KeyCode::Char('m') if app.selected_section() == Section::Sources => {
            app.toggle_source_kind()
        }
        KeyCode::Char('b') if app.document_focused() => perform_breakpoint_toggle(app, service),
        KeyCode::Char('[') => {
            let context_id = app.select_context(-1);
            service.switch_context(context_id);
        }
        KeyCode::Char(']') => {
            let context_id = app.select_context(1);
            service.switch_context(context_id);
        }
        KeyCode::Char('r') => app.refresh_active_data(),
        _ => {}
    }
    request_active_data(app, service);
    false
}

fn perform_target_attachment(app: &mut App, service: &ServiceController, force: bool) {
    match app.target_action_for_selected(force) {
        Ok(action) => {
            app.begin_action(&action);
            service.perform(action);
        }
        Err(error) => app.set_error(error),
    }
}

fn perform_connect(app: &mut App, service: &ServiceController) {
    match app.connect_action_for_selected() {
        Ok(action) => {
            app.begin_action(&action);
            service.perform(action);
        }
        Err(error) => app.set_error(error),
    }
}

fn perform_connection_configuration_toggle(app: &mut App, service: &ServiceController) {
    match app.toggle_connection_configuration_action_for_selected() {
        Ok(action) => {
            app.begin_action(&action);
            service.perform(action);
        }
        Err(error) => app.set_error(error),
    }
}

fn perform_breakpoint_toggle(app: &mut App, service: &ServiceController) {
    match app.breakpoint_action_for_source_line() {
        Ok(action) => {
            app.begin_action(&action);
            service.perform(action);
        }
        Err(error) => app.set_error(error),
    }
}

fn perform_delete_connection(app: &mut App, service: &ServiceController) {
    match app.delete_connection_action_for_selected() {
        Ok(action) => {
            app.begin_action(&action);
            service.perform(action);
        }
        Err(error) => app.set_error(error),
    }
}

fn request_active_data(app: &mut App, service: &mut ServiceController) {
    let mut sections = app
        .sections()
        .iter()
        .copied()
        .filter(|section| !app.is_section_collapsed(*section))
        .collect::<Vec<_>>();
    if app.tab() == Tab::Runtime
        && !app.is_section_collapsed(Section::Connections)
        && !sections.contains(&Section::Processes)
    {
        sections.push(Section::Processes);
    }
    for section in sections {
        if !matches!(
            section,
            Section::Contexts
                | Section::Processes
                | Section::Connections
                | Section::Sources
                | Section::Captures
        ) || !app.needs_load(section)
            || !app.begin_loading(section)
        {
            continue;
        }
        service.load(
            section,
            app.context_id().to_owned(),
            app.source_kind(),
            app.demanded_process_roots(),
        );
    }
}

fn handle_service_event(event: ServiceEvent, app: &mut App, service: &ServiceController) {
    match event {
        ServiceEvent::Context(snapshot) => app.apply_context(snapshot),
        ServiceEvent::ContextError(error) => app.set_error(error),
        ServiceEvent::TargetDetail {
            generation,
            target,
            detail,
        } => {
            if generation == service.target_generation() {
                if detail.is_some() {
                    app.set_target_detail(detail);
                } else {
                    app.remove_target_detail(&target.connection_id, &target.target_id);
                }
            }
        }
        ServiceEvent::DataLoaded {
            section,
            context_id,
            generation,
            result,
        } => {
            if context_id != app.context_id() || generation != service.load_generation(section) {
                return;
            }
            app.finish_loading(section);
            match result {
                Ok(Data::Contexts(contexts)) => app.set_contexts(contexts),
                Ok(Data::Processes(processes)) => app.set_processes(processes),
                Ok(Data::Resources(resources)) => app.set_resources(resources),
                Ok(Data::Sources(sources)) => app.set_sources(sources),
                Ok(Data::Captures(captures)) => app.set_captures(captures),
                Err(error) => app.set_error(error),
            }
        }
        ServiceEvent::SourceLoaded {
            context_id,
            path,
            generation,
            result,
        } => {
            if context_id != app.context_id()
                || generation != service.source_generation()
                || app.selected_source().as_deref() != Some(path.as_str())
            {
                return;
            }
            match result {
                Ok(content) => app.set_source_content(Some(content)),
                Err(error) => app.set_error(error),
            }
        }
        ServiceEvent::ActionFinished(result) => app.finish_action(result),
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    fn open() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let terminal = match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(stdout(), LeaveAlternateScreen);
                return Err(error);
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Default)]
struct Arguments {
    context: Option<String>,
    help: bool,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut result = Self::default();
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--context" => {
                    result.context = Some(
                        arguments
                            .next()
                            .ok_or_else(|| "--context requires a value".to_owned())?,
                    );
                }
                "--help" | "-h" | "help" => result.help = true,
                _ => return Err(format!("unknown argument '{argument}'\n\n{}", usage())),
            }
        }
        Ok(result)
    }
}

fn usage() -> &'static str {
    "usage: jsdbg-tui [--context <path|:id>]

keys:
  Tab / Shift+Tab, 1-2  switch Runtime / Debug
  j/k, Up/Down          move selection
  h/l, Left/Right       move to parent / remembered child
  Space                 collapse/expand
  c                     configure/connect/disconnect access path
  a                     attach/detach target
  f                     force target attachment
  d / Delete            remove an inactive connection
  m                     cycle source maps/formatted/no projection
  Enter                 expand/collapse or open a source
  b                     toggle breakpoint at the selected source line
  [ / ]                 switch context
  r                     query visible TUI data again
  q                     quit"
}

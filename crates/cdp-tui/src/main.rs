mod app;
mod service;
mod ui;

use std::io::{self, stdout};
use std::time::Duration;

use app::{App, Tab};
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
    let mut service = ServiceController::new(bootstrap.client, service_events);
    service.observe_context_after(bootstrap.context.id.clone(), bootstrap.context.revision);
    let mut app = App::new(
        bootstrap.contexts,
        bootstrap.context_index,
        bootstrap.context,
    );
    let mut input = EventStream::new();
    let mut refresh = tokio::time::interval(Duration::from_secs(2));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed_target = None;
    request_active_data(&mut app, &mut service);

    loop {
        terminal
            .terminal
            .draw(|frame| ui::render(frame, &app))
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

        let selected_target = app.selected_target();
        if selected_target != observed_target {
            observed_target = selected_target.clone();
            service.set_target(selected_target);
        }
    }
    Ok(())
}

fn handle_key(key: KeyEvent, app: &mut App, service: &mut ServiceController) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => app.next_tab(-1),
        KeyCode::Tab => app.next_tab(1),
        KeyCode::BackTab => app.next_tab(-1),
        KeyCode::Char(character @ '1'..='6') => {
            app.set_tab(Tab::ALL[(character as u8 - b'1') as usize]);
        }
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Home | KeyCode::Char('g') => app.select_edge(false),
        KeyCode::End | KeyCode::Char('G') => app.select_edge(true),
        KeyCode::Left | KeyCode::Char('h') => app.set_expanded(false),
        KeyCode::Right | KeyCode::Char('l') => app.set_expanded(true),
        KeyCode::Enter => app.toggle_expanded(),
        KeyCode::Char(' ') => perform_selected(app, service, false),
        KeyCode::Char('f') if app.tab() == Tab::Targets => {
            perform_selected(app, service, true);
        }
        KeyCode::Char('d') | KeyCode::Delete if app.tab() == Tab::Connections => {
            perform_delete_connection(app, service);
        }
        KeyCode::Char('m') if app.tab() == Tab::Sources => app.toggle_source_kind(),
        KeyCode::Char('[') => {
            let context_id = app.select_context(-1);
            service.switch_context(context_id);
        }
        KeyCode::Char(']') => {
            let context_id = app.select_context(1);
            service.switch_context(context_id);
        }
        KeyCode::Char('r') => request_active_data(app, service),
        _ => {}
    }
    request_active_data(app, service);
    false
}

fn perform_selected(app: &mut App, service: &ServiceController, force: bool) {
    match app.action_for_selected(force) {
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
    let tab = app.tab();
    if !matches!(tab, Tab::Processes | Tab::Sources | Tab::Captures) || !app.begin_loading(tab) {
        return;
    }
    service.load(tab, app.context_id().to_owned(), app.source_kind());
}

fn handle_service_event(event: ServiceEvent, app: &mut App, service: &ServiceController) {
    match event {
        ServiceEvent::Context(snapshot) => app.apply_context(snapshot),
        ServiceEvent::ContextError(error) => app.set_error(error),
        ServiceEvent::TargetDetail { generation, detail } => {
            if generation == service.target_generation() {
                app.set_target_detail(detail);
            }
        }
        ServiceEvent::DataLoaded {
            tab,
            context_id,
            generation,
            result,
        } => {
            if context_id != app.context_id() || generation != service.load_generation(tab) {
                return;
            }
            app.finish_loading(tab);
            match result {
                Ok(Data::Processes(processes)) => app.set_processes(processes),
                Ok(Data::Sources(sources)) => app.set_sources(sources),
                Ok(Data::Captures(captures)) => app.set_captures(captures),
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
  Tab / Shift+Tab, 1-6  switch tabs
  j/k, Up/Down          move selection
  h/l, Left/Right       collapse/expand
  Space                 attach process or toggle connection/target
  f                     force target attachment
  d / Delete            remove an inactive connection
  m                     cycle source maps/formatted/no projection
  [ / ]                 switch context
  r                     refresh active view
  q                     quit"
}

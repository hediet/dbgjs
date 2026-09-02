use std::collections::{BTreeMap, BTreeSet};

use cdp_client::service_api::{
    BreakpointApplicationStatus, BreakpointStatus, CaptureKind, CaptureSnapshot,
    ConnectionConfiguration, ConnectionSnapshot, ConnectionStatus, ContextSnapshot, ContextSummary,
    ProcessSnapshot, ProcessTreeSnapshot, SourceTreeKind, SourceTreeSnapshot,
    TargetAttachmentState, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetNodeSnapshot,
    UncompactedSourceRevisionSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tab {
    Processes,
    Connections,
    Targets,
    Sources,
    Breakpoints,
    Captures,
}

impl Tab {
    pub const ALL: [Self; 6] = [
        Self::Processes,
        Self::Connections,
        Self::Targets,
        Self::Sources,
        Self::Breakpoints,
        Self::Captures,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Processes => "Processes",
            Self::Connections => "Connections",
            Self::Targets => "Targets",
            Self::Sources => "Sources",
            Self::Breakpoints => "Breakpoints",
            Self::Captures => "Captures",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .expect("every tab is in Tab::ALL")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetRef {
    pub context_id: String,
    pub connection_id: String,
    pub connection_generation: u64,
    pub target_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRef {
    pub context_id: String,
    pub outline_key: String,
    pub root_process_id: u32,
    pub process_id: u32,
    pub role: cdp_client::service_api::ProcessRole,
    pub debug_target_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    AttachProcess {
        process: ProcessRef,
    },
    SetConnection {
        context_id: String,
        connection_id: String,
        connected: bool,
    },
    DeleteConnection {
        context_id: String,
        connection_id: String,
        expected_revision: u64,
    },
    SetTargetAttachment {
        target: TargetRef,
        attached: bool,
        force: bool,
    },
}

impl UiAction {
    pub fn key(&self) -> String {
        match self {
            Self::AttachProcess { process } => process.outline_key.clone(),
            Self::SetConnection { connection_id, .. } => {
                format!("connection:{connection_id}")
            }
            Self::DeleteConnection { connection_id, .. } => {
                format!("connection:{connection_id}")
            }
            Self::SetTargetAttachment { target, .. } => {
                format!("target:{}:{}", target.connection_id, target.target_id)
            }
        }
    }

    pub fn pending_message(&self) -> String {
        match self {
            Self::AttachProcess { process } => {
                format!("Attaching process {}", process.process_id)
            }
            Self::SetConnection {
                connection_id,
                connected,
                ..
            } => format!(
                "{} connection {connection_id}",
                if *connected {
                    "Connecting"
                } else {
                    "Disconnecting"
                }
            ),
            Self::DeleteConnection { connection_id, .. } => {
                format!("Removing connection {connection_id}")
            }
            Self::SetTargetAttachment {
                target, attached, ..
            } => format!(
                "{} target {}/{}",
                if *attached { "Attaching" } else { "Detaching" },
                target.connection_id,
                target.target_id
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutlineItem {
    ProcessTree {
        root_pid: u32,
    },
    Process {
        root_pid: u32,
        process_id: u32,
    },
    Connection {
        connection_id: String,
    },
    TargetGroup {
        connection_id: String,
    },
    Target {
        connection_id: String,
        target_id: String,
    },
    Source {
        path: String,
    },
    SourceFolder {
        path: String,
    },
    Breakpoint {
        breakpoint_id: String,
    },
    BreakpointApplication {
        breakpoint_id: String,
        connection_id: String,
        target_id: String,
        script_id: String,
    },
    Capture {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Good,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct OutlineRow {
    pub key: String,
    pub depth: usize,
    pub label: String,
    pub state: String,
    pub detail: String,
    pub item: OutlineItem,
    pub expandable: bool,
    pub expanded: bool,
    pub tone: Tone,
}

#[derive(Clone, Debug)]
pub struct Inspector {
    pub title: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct App {
    contexts: Vec<ContextSummary>,
    context_index: usize,
    context: Option<ContextSnapshot>,
    tab: Tab,
    selection: [usize; 6],
    expanded: BTreeSet<String>,
    processes: Vec<ProcessTreeSnapshot>,
    sources: Option<SourceTreeSnapshot>,
    source_kind: SourceTreeKind,
    initialized_source_kinds: BTreeSet<SourceTreeKind>,
    captures: Vec<CaptureSnapshot>,
    target_detail: Option<TargetDebuggerSnapshot>,
    loading_tabs: BTreeSet<Tab>,
    pending_action: Option<String>,
    message: Option<(Tone, String)>,
}

impl App {
    pub fn new(
        contexts: Vec<ContextSummary>,
        context_index: usize,
        context: ContextSnapshot,
    ) -> Self {
        let mut app = Self {
            contexts,
            context_index,
            context: Some(context),
            tab: Tab::Connections,
            selection: [0; 6],
            expanded: BTreeSet::new(),
            processes: Vec::new(),
            sources: None,
            source_kind: SourceTreeKind::SourceMapped,
            initialized_source_kinds: BTreeSet::new(),
            captures: Vec::new(),
            target_detail: None,
            loading_tabs: BTreeSet::new(),
            pending_action: None,
            message: None,
        };
        app.expand_defaults();
        app
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn context(&self) -> Option<&ContextSnapshot> {
        self.context.as_ref()
    }

    pub fn context_id(&self) -> &str {
        &self.contexts[self.context_index].id
    }

    pub fn context_name(&self) -> &str {
        &self.contexts[self.context_index].display_name
    }

    pub fn context_position(&self) -> (usize, usize) {
        (self.context_index + 1, self.contexts.len())
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.clamp_selection();
    }

    pub fn next_tab(&mut self, delta: isize) {
        let count = Tab::ALL.len() as isize;
        let index = (self.tab.index() as isize + delta).rem_euclid(count) as usize;
        self.set_tab(Tab::ALL[index]);
    }

    pub fn select_context(&mut self, delta: isize) -> String {
        let count = self.contexts.len() as isize;
        self.context_index = (self.context_index as isize + delta).rem_euclid(count) as usize;
        self.context = None;
        self.sources = None;
        self.initialized_source_kinds.clear();
        self.captures.clear();
        self.target_detail = None;
        self.selection = [0; 6];
        self.expanded.clear();
        self.loading_tabs.clear();
        self.message = Some((
            Tone::Muted,
            format!("Loading context {}", self.context_id()),
        ));
        self.context_id().to_owned()
    }

    pub fn apply_context(&mut self, snapshot: ContextSnapshot) {
        if snapshot.id != self.context_id() {
            return;
        }
        if self
            .context
            .as_ref()
            .is_some_and(|current| current.revision > snapshot.revision)
        {
            return;
        }
        self.context = Some(snapshot);
        self.expand_defaults();
        self.clamp_selection();
    }

    pub fn set_processes(&mut self, processes: Vec<ProcessTreeSnapshot>) {
        self.processes = processes;
        self.expand_defaults();
        self.clamp_selection();
    }

    pub fn set_sources(&mut self, sources: SourceTreeSnapshot) {
        if sources.kind != self.source_kind {
            return;
        }
        if self.initialized_source_kinds.insert(sources.kind) {
            let hierarchy = SourceFolder::from_snapshot(&sources);
            hierarchy.collect_folder_keys(sources.kind, &mut Vec::new(), &mut self.expanded);
        }
        self.sources = Some(sources);
        self.clamp_selection();
    }

    pub fn source_kind(&self) -> SourceTreeKind {
        self.source_kind
    }

    pub fn source_kind_label(&self) -> &'static str {
        source_kind_label(self.source_kind)
    }

    pub fn toggle_source_kind(&mut self) {
        self.source_kind = match self.source_kind {
            SourceTreeKind::SourceMapped => SourceTreeKind::Formatted,
            SourceTreeKind::Formatted => SourceTreeKind::Loaded,
            SourceTreeKind::Loaded | SourceTreeKind::Resolved => SourceTreeKind::SourceMapped,
        };
        self.sources = None;
        self.loading_tabs.remove(&Tab::Sources);
        self.selection[Tab::Sources.index()] = 0;
    }

    pub fn set_captures(&mut self, captures: Vec<CaptureSnapshot>) {
        self.captures = captures;
        self.clamp_selection();
    }

    pub fn set_target_detail(&mut self, detail: Option<TargetDebuggerSnapshot>) {
        if let Some(detail) = &detail
            && detail.context_id != self.context_id()
        {
            return;
        }
        self.target_detail = detail;
    }

    pub fn begin_loading(&mut self, tab: Tab) -> bool {
        let started = self.loading_tabs.insert(tab);
        if started && self.pending_action.is_none() {
            self.message = None;
        }
        started
    }

    pub fn finish_loading(&mut self, tab: Tab) {
        self.loading_tabs.remove(&tab);
    }

    pub fn is_loading(&self, tab: Tab) -> bool {
        self.loading_tabs.contains(&tab)
    }

    pub fn begin_action(&mut self, action: &UiAction) {
        self.pending_action = Some(action.key());
        self.message = Some((Tone::Warning, action.pending_message()));
    }

    pub fn finish_action(&mut self, result: Result<(String, ContextSnapshot), String>) {
        self.pending_action = None;
        match result {
            Ok((message, snapshot)) => {
                self.apply_context(snapshot);
                self.message = Some((Tone::Good, message));
            }
            Err(error) => self.message = Some((Tone::Error, error)),
        }
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.message = Some((Tone::Error, error.into()));
    }

    pub fn message(&self) -> Option<(Tone, &str)> {
        self.message
            .as_ref()
            .map(|(tone, message)| (*tone, message.as_str()))
    }

    pub fn rows(&self) -> Vec<OutlineRow> {
        match self.tab {
            Tab::Processes => self.process_rows(),
            Tab::Connections => self.connection_rows(),
            Tab::Targets => self.target_rows(),
            Tab::Sources => self.source_rows(),
            Tab::Breakpoints => self.breakpoint_rows(),
            Tab::Captures => self.capture_rows(),
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        (!self.rows().is_empty()).then_some(self.selection[self.tab.index()])
    }

    pub fn selected_row(&self) -> Option<OutlineRow> {
        self.rows()
            .into_iter()
            .nth(self.selection[self.tab.index()])
    }

    pub fn move_selection(&mut self, delta: isize) {
        let row_count = self.rows().len();
        if row_count == 0 {
            return;
        }
        let selection = &mut self.selection[self.tab.index()];
        *selection =
            (*selection as isize + delta).clamp(0, row_count.saturating_sub(1) as isize) as usize;
    }

    pub fn select_edge(&mut self, last: bool) {
        let row_count = self.rows().len();
        self.selection[self.tab.index()] = if last { row_count.saturating_sub(1) } else { 0 };
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        let Some(row) = self.selected_row().filter(|row| row.expandable) else {
            return;
        };
        if expanded {
            self.expanded.insert(row.key);
        } else {
            self.expanded.remove(&row.key);
        }
        self.clamp_selection();
    }

    pub fn toggle_expanded(&mut self) {
        let Some(row) = self.selected_row().filter(|row| row.expandable) else {
            return;
        };
        if !self.expanded.remove(&row.key) {
            self.expanded.insert(row.key);
        }
        self.clamp_selection();
    }

    pub fn action_for_selected(&self, force: bool) -> Result<UiAction, String> {
        if self.pending_action.is_some() {
            return Err("another lifecycle operation is still pending".to_owned());
        }
        let row = self
            .selected_row()
            .ok_or_else(|| "nothing is selected".to_owned())?;
        match row.item {
            OutlineItem::ProcessTree { root_pid } => {
                self.attach_process_action(root_pid, root_pid, row.key)
            }
            OutlineItem::Process {
                root_pid,
                process_id,
            } => self.attach_process_action(root_pid, process_id, row.key),
            OutlineItem::Connection { connection_id } => {
                let connection = self.connection(&connection_id)?;
                let connected = match connection.status {
                    ConnectionStatus::Disconnected | ConnectionStatus::Failed { .. } => true,
                    ConnectionStatus::Connected { .. } => false,
                    ConnectionStatus::Connecting => {
                        return Err("connection is already connecting".to_owned());
                    }
                    ConnectionStatus::Disconnecting => {
                        return Err("connection is already disconnecting".to_owned());
                    }
                };
                Ok(UiAction::SetConnection {
                    context_id: self.context_id().to_owned(),
                    connection_id,
                    connected,
                })
            }
            OutlineItem::Target {
                connection_id,
                target_id,
            } => {
                let target = self.target(&connection_id, &target_id)?;
                match target.attachment {
                    TargetAttachmentState::Debugger => Ok(UiAction::SetTargetAttachment {
                        target: TargetRef {
                            context_id: self.context_id().to_owned(),
                            connection_id,
                            connection_generation: target.connection_generation,
                            target_id,
                        },
                        attached: false,
                        force: false,
                    }),
                    TargetAttachmentState::Detached => Ok(UiAction::SetTargetAttachment {
                        target: TargetRef {
                            context_id: self.context_id().to_owned(),
                            connection_id,
                            connection_generation: target.connection_generation,
                            target_id,
                        },
                        attached: true,
                        force: false,
                    }),
                    TargetAttachmentState::External if force => Ok(UiAction::SetTargetAttachment {
                        target: TargetRef {
                            context_id: self.context_id().to_owned(),
                            connection_id,
                            connection_generation: target.connection_generation,
                            target_id,
                        },
                        attached: true,
                        force: true,
                    }),
                    TargetAttachmentState::External => Err(
                        "target has an external debugger; press f to steal attachment".to_owned(),
                    ),
                }
            }
            _ => Err("Space attaches processes or toggles connections and targets".to_owned()),
        }
    }

    pub fn delete_connection_action_for_selected(&self) -> Result<UiAction, String> {
        if self.pending_action.is_some() {
            return Err("another lifecycle operation is still pending".to_owned());
        }
        let OutlineItem::Connection { connection_id } = self
            .selected_row()
            .ok_or_else(|| "nothing is selected".to_owned())?
            .item
        else {
            return Err("select a connection to remove".to_owned());
        };
        let connection = self.connection(&connection_id)?;
        match connection.status {
            ConnectionStatus::Disconnected | ConnectionStatus::Failed { .. } => {}
            ConnectionStatus::Connected { .. }
            | ConnectionStatus::Connecting
            | ConnectionStatus::Disconnecting => {
                return Err(format!(
                    "disconnect connection {connection_id} before removing it"
                ));
            }
        }
        let expected_revision = self
            .context
            .as_ref()
            .ok_or_else(|| "context is still loading".to_owned())?
            .revision;
        Ok(UiAction::DeleteConnection {
            context_id: self.context_id().to_owned(),
            connection_id,
            expected_revision,
        })
    }

    pub fn selected_target(&self) -> Option<TargetRef> {
        let OutlineItem::Target {
            connection_id,
            target_id,
        } = self.selected_row()?.item
        else {
            return None;
        };
        let target = self.target(&connection_id, &target_id).ok()?;
        (target.attachment == TargetAttachmentState::Debugger).then(|| TargetRef {
            context_id: self.context_id().to_owned(),
            connection_id,
            connection_generation: target.connection_generation,
            target_id,
        })
    }

    pub fn inspector(&self) -> Inspector {
        let Some(row) = self.selected_row() else {
            return Inspector {
                title: self.tab.title().to_owned(),
                lines: vec!["No items.".to_owned()],
            };
        };
        match row.item {
            OutlineItem::ProcessTree { root_pid } => self.process_tree_inspector(root_pid),
            OutlineItem::Process {
                root_pid,
                process_id,
            } => self.process_inspector(root_pid, process_id),
            OutlineItem::Connection { connection_id } => self.connection_inspector(&connection_id),
            OutlineItem::TargetGroup { connection_id } => self.connection_inspector(&connection_id),
            OutlineItem::Target {
                connection_id,
                target_id,
            } => self.target_inspector(&connection_id, &target_id),
            OutlineItem::Source { path } => self.source_inspector(&path),
            OutlineItem::SourceFolder { path } => self.source_folder_inspector(&path),
            OutlineItem::Breakpoint { breakpoint_id } => self.breakpoint_inspector(&breakpoint_id),
            OutlineItem::BreakpointApplication {
                breakpoint_id,
                connection_id,
                target_id,
                script_id,
            } => self.breakpoint_application_inspector(
                &breakpoint_id,
                &connection_id,
                &target_id,
                &script_id,
            ),
            OutlineItem::Capture { name } => self.capture_inspector(&name),
        }
    }

    pub fn footer_hint(&self) -> &'static str {
        match self.tab {
            Tab::Connections => "Space connect/disconnect  d/Delete remove inactive",
            Tab::Targets => "Space attach/detach  f force attach",
            Tab::Processes => "Space attach process  r refresh process inventory",
            Tab::Sources => "m cycle projection  r refresh sources",
            Tab::Breakpoints => "Breakpoint state is live from the context",
            Tab::Captures => "r refresh capture catalog",
        }
    }

    fn process_rows(&self) -> Vec<OutlineRow> {
        let mut rows = Vec::new();
        for tree in &self.processes {
            let key = format!("process-tree:{}", tree.root_process_id);
            let expanded = self.expanded.contains(&key);
            let pending = self.pending_action.as_deref() == Some(&key);
            rows.push(OutlineRow {
                key: key.clone(),
                depth: 0,
                label: format!(
                    "{} process tree {}",
                    if pending { "[~]" } else { "   " },
                    tree.root_process_id
                ),
                state: format!("{} processes", tree.processes.len()),
                detail: format!("{:?}", tree.root_kind).to_lowercase(),
                item: OutlineItem::ProcessTree {
                    root_pid: tree.root_process_id,
                },
                expandable: !tree.processes.is_empty(),
                expanded,
                tone: Tone::Normal,
            });
            if !expanded {
                continue;
            }
            let depths = process_depths(tree);
            for process in &tree.processes {
                let key = format!("process:{}:{}", tree.root_process_id, process.process_id);
                let attach = if self.pending_action.as_deref() == Some(&key) {
                    "[~]"
                } else if process.attachable {
                    "[ ]"
                } else {
                    "   "
                };
                rows.push(OutlineRow {
                    key,
                    depth: depths.get(&process.process_id).copied().unwrap_or(0) + 1,
                    label: format!("{attach} {}", process_label(process)),
                    state: process_role_name(process),
                    detail: format!("pid {}", process.process_id),
                    item: OutlineItem::Process {
                        root_pid: tree.root_process_id,
                        process_id: process.process_id,
                    },
                    expandable: false,
                    expanded: false,
                    tone: if process.attachable {
                        Tone::Good
                    } else {
                        Tone::Muted
                    },
                });
            }
        }
        rows
    }

    fn connection_rows(&self) -> Vec<OutlineRow> {
        let Some(context) = &self.context else {
            return Vec::new();
        };
        let mut connections = context.connections.iter().collect::<Vec<_>>();
        connections.sort_by(|left, right| left.id.cmp(&right.id));
        connections
            .into_iter()
            .map(|connection| {
                let (toggle, state, tone) = connection_state(&connection.status);
                let key = format!("connection:{}", connection.id);
                let toggle = if self.pending_action.as_deref() == Some(&key) {
                    "[~]"
                } else {
                    toggle
                };
                OutlineRow {
                    key,
                    depth: 0,
                    label: format!("{toggle} {}", connection.id),
                    state: state.to_owned(),
                    detail: format!(
                        "{} · {} targets",
                        connection_kind(&connection.configuration),
                        connection.targets.len()
                    ),
                    item: OutlineItem::Connection {
                        connection_id: connection.id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone,
                }
            })
            .collect()
    }

    fn target_rows(&self) -> Vec<OutlineRow> {
        let Some(context) = &self.context else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut connections = context.connections.iter().collect::<Vec<_>>();
        connections.sort_by(|left, right| left.id.cmp(&right.id));
        for connection in connections {
            let group_key = format!("target-group:{}", connection.id);
            let expanded = self.expanded.contains(&group_key);
            rows.push(OutlineRow {
                key: group_key,
                depth: 0,
                label: connection.id.clone(),
                state: connection_status_name(&connection.status).to_owned(),
                detail: format!("{} targets", connection.targets.len()),
                item: OutlineItem::TargetGroup {
                    connection_id: connection.id.clone(),
                },
                expandable: !connection.targets.is_empty(),
                expanded,
                tone: connection_state(&connection.status).2,
            });
            if !expanded {
                continue;
            }
            let targets = context
                .target_forest
                .iter()
                .filter(|target| target.connection_id == connection.id)
                .collect::<Vec<_>>();
            let depths = target_depths(&targets);
            for target in targets {
                let key = format!(
                    "target:{}:{}",
                    target.connection_id, target.target.target_id
                );
                let toggle = if self.pending_action.as_deref() == Some(&key) {
                    "[~]"
                } else {
                    attachment_toggle(target.attachment)
                };
                let selected_detail = self.target_detail.as_ref().filter(|detail| {
                    detail.connection_id == target.connection_id
                        && detail.connection_generation == target.connection_generation
                        && detail.target_id == target.target.target_id
                });
                let state = selected_detail
                    .map(|detail| target_phase(&detail.phase))
                    .unwrap_or_else(|| attachment_name(target.attachment).to_owned());
                rows.push(OutlineRow {
                    key,
                    depth: depths
                        .get(target.target.target_id.as_str())
                        .copied()
                        .unwrap_or(0)
                        + 1,
                    label: format!("{toggle} {}", target_label(target)),
                    state,
                    detail: target.target.target_type.clone(),
                    item: OutlineItem::Target {
                        connection_id: target.connection_id.clone(),
                        target_id: target.target.target_id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone: match target.attachment {
                        TargetAttachmentState::Debugger => Tone::Good,
                        TargetAttachmentState::External => Tone::Warning,
                        TargetAttachmentState::Detached => Tone::Muted,
                    },
                });
            }
        }
        rows
    }

    fn source_rows(&self) -> Vec<OutlineRow> {
        let Some(sources) = &self.sources else {
            return Vec::new();
        };
        let hierarchy = SourceFolder::from_snapshot(sources);
        let mut rows = Vec::new();
        hierarchy.append_rows(self, sources, sources.kind, &mut Vec::new(), 0, &mut rows);
        rows
    }

    fn breakpoint_rows(&self) -> Vec<OutlineRow> {
        let Some(context) = &self.context else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut breakpoints = context.breakpoints.iter().collect::<Vec<_>>();
        breakpoints.sort_by(|left, right| left.id.cmp(&right.id));
        for breakpoint in breakpoints {
            let key = format!("breakpoint:{}", breakpoint.id);
            let expanded = self.expanded.contains(&key);
            let (state, tone) = breakpoint_state(&breakpoint.status);
            rows.push(OutlineRow {
                key,
                depth: 0,
                label: breakpoint.id.clone(),
                state: state.to_owned(),
                detail: format!(
                    "{}:{}:{}",
                    breakpoint.source_path, breakpoint.line, breakpoint.column
                ),
                item: OutlineItem::Breakpoint {
                    breakpoint_id: breakpoint.id.clone(),
                },
                expandable: !breakpoint.applications.is_empty(),
                expanded,
                tone,
            });
            if !expanded {
                continue;
            }
            for application in &breakpoint.applications {
                let (state, tone) = breakpoint_application_state(&application.status);
                rows.push(OutlineRow {
                    key: format!(
                        "breakpoint-app:{}:{}:{}:{}",
                        breakpoint.id,
                        application.connection_id,
                        application.target_id,
                        application.script_id
                    ),
                    depth: 1,
                    label: format!("{}/{}", application.connection_id, application.target_id),
                    state: state.to_owned(),
                    detail: format!(
                        "{}:{}:{}",
                        application.script_url,
                        application.generated_line,
                        application.generated_column
                    ),
                    item: OutlineItem::BreakpointApplication {
                        breakpoint_id: breakpoint.id.clone(),
                        connection_id: application.connection_id.clone(),
                        target_id: application.target_id.clone(),
                        script_id: application.script_id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone,
                });
            }
        }
        rows
    }

    fn capture_rows(&self) -> Vec<OutlineRow> {
        let mut captures = self.captures.iter().collect::<Vec<_>>();
        captures.sort_by(|left, right| left.name.cmp(&right.name));
        captures
            .into_iter()
            .map(|capture| OutlineRow {
                key: format!("capture:{}", capture.name),
                depth: 0,
                label: capture.name.clone(),
                state: capture_kind(capture.kind).to_owned(),
                detail: format!("{}/{}", capture.connection_id, capture.target_id),
                item: OutlineItem::Capture {
                    name: capture.name.clone(),
                },
                expandable: false,
                expanded: false,
                tone: Tone::Normal,
            })
            .collect()
    }

    fn expand_defaults(&mut self) {
        for tree in &self.processes {
            self.expanded
                .insert(format!("process-tree:{}", tree.root_process_id));
        }
        if let Some(context) = &self.context {
            for connection in &context.connections {
                self.expanded
                    .insert(format!("target-group:{}", connection.id));
            }
            for breakpoint in &context.breakpoints {
                if !breakpoint.applications.is_empty() {
                    self.expanded
                        .insert(format!("breakpoint:{}", breakpoint.id));
                }
            }
        }
    }

    fn clamp_selection(&mut self) {
        let row_count = self.rows().len();
        let selection = &mut self.selection[self.tab.index()];
        *selection = (*selection).min(row_count.saturating_sub(1));
    }

    fn connection(&self, connection_id: &str) -> Result<&ConnectionSnapshot, String> {
        self.context
            .as_ref()
            .and_then(|context| {
                context
                    .connections
                    .iter()
                    .find(|connection| connection.id == connection_id)
            })
            .ok_or_else(|| format!("connection '{connection_id}' is no longer available"))
    }

    fn process(&self, root_pid: u32, process_id: u32) -> Result<&ProcessSnapshot, String> {
        self.processes
            .iter()
            .find(|tree| tree.root_process_id == root_pid)
            .and_then(|tree| {
                tree.processes
                    .iter()
                    .find(|process| process.process_id == process_id)
            })
            .ok_or_else(|| format!("process '{process_id}' is no longer available"))
    }

    fn attach_process_action(
        &self,
        root_pid: u32,
        process_id: u32,
        outline_key: String,
    ) -> Result<UiAction, String> {
        let process = self.process(root_pid, process_id)?;
        if !process.attachable {
            return Err(format!("process {process_id} is not attachable"));
        }
        Ok(UiAction::AttachProcess {
            process: ProcessRef {
                context_id: self.context_id().to_owned(),
                outline_key,
                root_process_id: root_pid,
                process_id,
                role: process.role.clone(),
                debug_target_id: process.debug_target_id.clone(),
            },
        })
    }

    fn target(&self, connection_id: &str, target_id: &str) -> Result<&TargetNodeSnapshot, String> {
        self.context
            .as_ref()
            .and_then(|context| {
                context.target_forest.iter().find(|target| {
                    target.connection_id == connection_id && target.target.target_id == target_id
                })
            })
            .ok_or_else(|| format!("target '{connection_id}/{target_id}' is no longer available"))
    }

    fn process_tree_inspector(&self, root_pid: u32) -> Inspector {
        let Some(tree) = self
            .processes
            .iter()
            .find(|tree| tree.root_process_id == root_pid)
        else {
            return unavailable_inspector("Process tree");
        };
        Inspector {
            title: format!("Process tree {root_pid}"),
            lines: vec![
                format!("Kind: {:?}", tree.root_kind),
                format!("Processes: {}", tree.processes.len()),
                format!("Targets: {}", tree.targets.len()),
                format!(
                    "Runtime metadata: {}",
                    yes_no(tree.runtime_metadata_available)
                ),
                format!(
                    "Discovery error: {}",
                    tree.target_discovery_error.as_deref().unwrap_or("none")
                ),
            ],
        }
    }

    fn process_inspector(&self, root_pid: u32, process_id: u32) -> Inspector {
        let Some(process) = self
            .processes
            .iter()
            .find(|tree| tree.root_process_id == root_pid)
            .and_then(|tree| {
                tree.processes
                    .iter()
                    .find(|process| process.process_id == process_id)
            })
        else {
            return unavailable_inspector("Process");
        };
        Inspector {
            title: process_label(process),
            lines: vec![
                format!("PID: {}", process.process_id),
                format!(
                    "Parent PID: {}",
                    process
                        .parent_process_id
                        .map_or_else(|| "none".to_owned(), |pid| pid.to_string())
                ),
                format!("Role: {:?}", process.role),
                format!("Attachable: {}", yes_no(process.attachable)),
                format!("Command: {}", process.command_line),
                if process.attachable {
                    "Space attaches this process.".to_owned()
                } else {
                    "This process does not expose a debugger target.".to_owned()
                },
            ],
        }
    }

    fn connection_inspector(&self, connection_id: &str) -> Inspector {
        let Ok(connection) = self.connection(connection_id) else {
            return unavailable_inspector("Connection");
        };
        let mut lines = vec![
            format!("Status: {}", connection_status_name(&connection.status)),
            format!("Kind: {}", connection_kind(&connection.configuration)),
            format!("Generation: {}", connection.generation),
            format!("Targets: {}", connection.targets.len()),
        ];
        lines.extend(connection_configuration_lines(&connection.configuration));
        lines.push(String::new());
        lines.push("Space toggles connect/disconnect.".to_owned());
        lines.push(match connection.status {
            ConnectionStatus::Disconnected | ConnectionStatus::Failed { .. } => {
                "d or Delete removes this connection.".to_owned()
            }
            ConnectionStatus::Connected { .. }
            | ConnectionStatus::Connecting
            | ConnectionStatus::Disconnecting => {
                "Disconnect this connection before removing it.".to_owned()
            }
        });
        Inspector {
            title: format!("Connection {connection_id}"),
            lines,
        }
    }

    fn target_inspector(&self, connection_id: &str, target_id: &str) -> Inspector {
        let Ok(target) = self.target(connection_id, target_id) else {
            return unavailable_inspector("Target");
        };
        let mut lines = vec![
            format!("Attachment: {}", attachment_name(target.attachment)),
            format!("Type: {}", target.target.target_type),
            format!("ID: {}", target.target.target_id),
            format!("Generation: {}", target.connection_generation),
            format!("URL: {}", display_or(&target.target.url, "(none)")),
            format!(
                "Parent: {}",
                target.parent_target_id.as_deref().unwrap_or("none")
            ),
        ];
        if let Some(detail) = self.target_detail.as_ref().filter(|detail| {
            detail.connection_id == connection_id
                && detail.connection_generation == target.connection_generation
                && detail.target_id == target_id
        }) {
            lines.push(String::new());
            lines.push(format!("Phase: {}", target_phase(&detail.phase)));
            lines.push(format!("Scripts: {}", detail.scripts.len()));
            lines.push(format!("Breakpoints: {}", detail.breakpoints.len()));
            if let Some(pause) = &detail.pause {
                lines.push(format!("Pause: {} · epoch {}", pause.reason, pause.epoch));
                for frame in pause.frames.iter().take(8) {
                    lines.push(format!(
                        "  {}  {}:{}:{}",
                        display_or(&frame.function_name, "(anonymous)"),
                        frame.raw.source_url,
                        frame.raw.line,
                        frame.raw.column
                    ));
                }
            }
        }
        lines.push(String::new());
        lines.push(match target.attachment {
            TargetAttachmentState::Debugger => "Space detaches this target.".to_owned(),
            TargetAttachmentState::Detached => "Space attaches this target.".to_owned(),
            TargetAttachmentState::External => {
                "An external debugger owns this target; press f to steal.".to_owned()
            }
        });
        Inspector {
            title: format!("Target {connection_id}/{target_id}"),
            lines,
        }
    }

    fn source_inspector(&self, path: &str) -> Inspector {
        let sources = self
            .sources
            .as_ref()
            .map(|tree| {
                tree.sources
                    .iter()
                    .filter(|source| source.uri == path)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let lines = (!sources.is_empty()).then(|| {
            let mut lines = vec![
                format!("Projection: {}", self.source_kind_label()),
                format!("URI: {path}"),
                format!("Snapshots: {}", sources.len()),
            ];
            lines.extend(
                sources
                    .iter()
                    .map(|source| format!("Revision: {}", source_revision(&source.revision))),
            );
            lines
        });
        let lines = lines.map_or_else(
            || vec!["Source is no longer available.".to_owned()],
            |lines| lines,
        );
        Inspector {
            title: "Source".to_owned(),
            lines,
        }
    }

    fn source_folder_inspector(&self, path: &str) -> Inspector {
        Inspector {
            title: "Source folder".to_owned(),
            lines: vec![
                format!("Projection: {}", self.source_kind_label()),
                format!("Path: {path}"),
                "Use h/l or Left/Right to collapse or expand.".to_owned(),
            ],
        }
    }

    fn breakpoint_inspector(&self, breakpoint_id: &str) -> Inspector {
        let Some(breakpoint) = self.context.as_ref().and_then(|context| {
            context
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == breakpoint_id)
        }) else {
            return unavailable_inspector("Breakpoint");
        };
        Inspector {
            title: format!("Breakpoint {}", breakpoint.id),
            lines: vec![
                format!(
                    "Location: {}:{}:{}",
                    breakpoint.source_path, breakpoint.line, breakpoint.column
                ),
                format!("Status: {}", breakpoint_state(&breakpoint.status).0),
                format!("Enabled: {}", yes_no(breakpoint.enabled)),
                format!(
                    "Condition: {}",
                    breakpoint.condition.as_deref().unwrap_or("none")
                ),
                format!(
                    "Target selector: {}",
                    breakpoint.target_selector.as_deref().unwrap_or("all")
                ),
                format!("Applications: {}", breakpoint.applications.len()),
            ],
        }
    }

    fn breakpoint_application_inspector(
        &self,
        breakpoint_id: &str,
        connection_id: &str,
        target_id: &str,
        script_id: &str,
    ) -> Inspector {
        let application = self
            .context
            .as_ref()
            .and_then(|context| {
                context
                    .breakpoints
                    .iter()
                    .find(|breakpoint| breakpoint.id == breakpoint_id)
            })
            .and_then(|breakpoint| {
                breakpoint.applications.iter().find(|application| {
                    application.connection_id == connection_id
                        && application.target_id == target_id
                        && application.script_id == script_id
                })
            });
        let Some(application) = application else {
            return unavailable_inspector("Breakpoint application");
        };
        Inspector {
            title: format!("Breakpoint {breakpoint_id} application"),
            lines: vec![
                format!("Target: {connection_id}/{target_id}"),
                format!("Script: {}", application.script_url),
                format!("Script ID: {}", application.script_id),
                format!(
                    "Generated location: {}:{}",
                    application.generated_line, application.generated_column
                ),
                format!(
                    "Status: {}",
                    breakpoint_application_state(&application.status).0
                ),
            ],
        }
    }

    fn capture_inspector(&self, name: &str) -> Inspector {
        let Some(capture) = self.captures.iter().find(|capture| capture.name == name) else {
            return unavailable_inspector("Capture");
        };
        Inspector {
            title: format!("Capture {}", capture.name),
            lines: vec![
                format!("Kind: {}", capture_kind(capture.kind)),
                format!("Target: {}/{}", capture.connection_id, capture.target_id),
                format!("Generation: {}", capture.connection_generation),
                format!("Storage ID: {}", capture.storage_id),
            ],
        }
    }
}

fn process_depths(tree: &ProcessTreeSnapshot) -> BTreeMap<u32, usize> {
    let parents = tree
        .processes
        .iter()
        .map(|process| (process.process_id, process.parent_process_id))
        .collect::<BTreeMap<_, _>>();
    tree.processes
        .iter()
        .map(|process| {
            let mut depth = 0;
            let mut current = process.parent_process_id;
            let mut visited = BTreeSet::new();
            while let Some(parent) = current.filter(|parent| visited.insert(*parent)) {
                if !parents.contains_key(&parent) {
                    break;
                }
                depth += 1;
                current = parents[&parent];
            }
            (process.process_id, depth)
        })
        .collect()
}

fn target_depths(targets: &[&TargetNodeSnapshot]) -> BTreeMap<String, usize> {
    let parents = targets
        .iter()
        .map(|target| {
            (
                target.target.target_id.as_str(),
                target.parent_target_id.as_deref(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    targets
        .iter()
        .map(|target| {
            let mut depth = 0;
            let mut current = target.parent_target_id.as_deref();
            let mut visited = BTreeSet::new();
            while let Some(parent) = current.filter(|parent| visited.insert(*parent)) {
                if !parents.contains_key(parent) {
                    break;
                }
                depth += 1;
                current = parents[parent];
            }
            (target.target.target_id.clone(), depth)
        })
        .collect()
}

fn process_label(process: &ProcessSnapshot) -> String {
    process
        .display_name
        .as_deref()
        .or(process.window_title.as_deref())
        .unwrap_or(&process.name)
        .to_owned()
}

fn process_role_name(process: &ProcessSnapshot) -> String {
    format!("{:?}", process.role).to_lowercase()
}

fn target_label(target: &TargetNodeSnapshot) -> String {
    display_or(&target.target.title, &target.target.target_id).to_owned()
}

fn connection_state(status: &ConnectionStatus) -> (&'static str, &'static str, Tone) {
    match status {
        ConnectionStatus::Disconnected => ("[ ]", "disconnected", Tone::Muted),
        ConnectionStatus::Connecting => ("[-]", "connecting", Tone::Warning),
        ConnectionStatus::Disconnecting => ("[-]", "disconnecting", Tone::Warning),
        ConnectionStatus::Connected { .. } => ("[x]", "connected", Tone::Good),
        ConnectionStatus::Failed { .. } => ("[ ]", "failed", Tone::Error),
    }
}

fn connection_status_name(status: &ConnectionStatus) -> &'static str {
    connection_state(status).1
}

fn connection_kind(configuration: &ConnectionConfiguration) -> &'static str {
    match configuration {
        ConnectionConfiguration::DirectCdp { .. } => "direct CDP",
        ConnectionConfiguration::NodeInspector { .. } => "Node inspector",
        ConnectionConfiguration::Process { .. } => "process",
        ConnectionConfiguration::ProcessTree { .. } => "process tree",
        ConnectionConfiguration::Playwright { .. } => "Playwright",
        ConnectionConfiguration::Chrome { .. } => "Chrome",
        ConnectionConfiguration::Node { .. } => "Node",
        ConnectionConfiguration::Stdio { .. } => "stdio",
    }
}

fn connection_configuration_lines(configuration: &ConnectionConfiguration) -> Vec<String> {
    match configuration {
        ConnectionConfiguration::DirectCdp { endpoint }
        | ConnectionConfiguration::NodeInspector { endpoint } => {
            vec![format!("Endpoint: {endpoint}")]
        }
        ConnectionConfiguration::Process { process_id } => vec![format!("PID: {process_id}")],
        ConnectionConfiguration::ProcessTree { root_pid } => {
            vec![format!("Root PID: {root_pid}")]
        }
        ConnectionConfiguration::Playwright {
            url,
            channel,
            headless,
            ..
        } => vec![
            format!("URL: {url}"),
            format!("Channel: {channel:?}"),
            format!("Headless: {}", yes_no(*headless)),
        ],
        ConnectionConfiguration::Chrome {
            url,
            executable,
            headless,
            ..
        } => vec![
            format!("URL: {url}"),
            format!("Executable: {executable}"),
            format!("Headless: {}", yes_no(*headless)),
        ],
        ConnectionConfiguration::Node { program, cwd, .. } => {
            vec![format!("Program: {program}"), format!("CWD: {cwd}")]
        }
        ConnectionConfiguration::Stdio { command, cwd, .. } => {
            vec![format!("Command: {command}"), format!("CWD: {cwd}")]
        }
    }
}

fn attachment_toggle(state: TargetAttachmentState) -> &'static str {
    match state {
        TargetAttachmentState::Detached => "[ ]",
        TargetAttachmentState::External => "[!]",
        TargetAttachmentState::Debugger => "[x]",
    }
}

fn attachment_name(state: TargetAttachmentState) -> &'static str {
    match state {
        TargetAttachmentState::Detached => "detached",
        TargetAttachmentState::External => "external",
        TargetAttachmentState::Debugger => "attached",
    }
}

fn target_phase(phase: &TargetDebuggerPhase) -> String {
    match phase {
        TargetDebuggerPhase::Running => "running".to_owned(),
        TargetDebuggerPhase::Paused { epoch } => format!("PAUSED · epoch {epoch}"),
        TargetDebuggerPhase::Resuming { epoch } => format!("resuming · epoch {epoch}"),
        TargetDebuggerPhase::Failed { .. } => "failed".to_owned(),
    }
}

#[derive(Default)]
struct SourceFolder {
    folders: BTreeMap<String, SourceFolder>,
    files: BTreeMap<String, BTreeMap<String, usize>>,
}

impl SourceFolder {
    fn from_snapshot(snapshot: &SourceTreeSnapshot) -> Self {
        let mut root = Self::default();
        for source in &snapshot.sources {
            let mut components = source_tree_components(&source.uri);
            let file_name = components.pop().unwrap_or_else(|| source.uri.clone());
            let mut folder = &mut root;
            for component in components {
                folder = folder.folders.entry(component).or_default();
            }
            *folder
                .files
                .entry(file_name)
                .or_default()
                .entry(source.uri.clone())
                .or_default() += 1;
        }
        root
    }

    fn collect_folder_keys(
        &self,
        kind: SourceTreeKind,
        path: &mut Vec<String>,
        expanded: &mut BTreeSet<String>,
    ) {
        for (name, folder) in &self.folders {
            path.push(name.clone());
            expanded.insert(source_folder_key(kind, path));
            folder.collect_folder_keys(kind, path, expanded);
            path.pop();
        }
    }

    fn append_rows(
        &self,
        app: &App,
        snapshot: &SourceTreeSnapshot,
        kind: SourceTreeKind,
        path: &mut Vec<String>,
        depth: usize,
        rows: &mut Vec<OutlineRow>,
    ) {
        for (name, folder) in &self.folders {
            path.push(name.clone());
            let key = source_folder_key(kind, path);
            let expanded = app.expanded.contains(&key);
            let (source_count, snapshot_count) = folder.metrics();
            rows.push(OutlineRow {
                key,
                depth,
                label: name.clone(),
                state: format!(
                    "{source_count} source{}",
                    if source_count == 1 { "" } else { "s" }
                ),
                detail: (snapshot_count > source_count)
                    .then(|| format!("{snapshot_count} snapshots"))
                    .unwrap_or_default(),
                item: OutlineItem::SourceFolder {
                    path: path.join("/"),
                },
                expandable: true,
                expanded,
                tone: Tone::Normal,
            });
            if expanded {
                folder.append_rows(app, snapshot, kind, path, depth + 1, rows);
            }
            path.pop();
        }
        for (name, sources) in &self.files {
            for (uri, snapshot_count) in sources {
                let state = if *snapshot_count > 1 {
                    format!("{snapshot_count} snapshots")
                } else {
                    snapshot
                        .sources
                        .iter()
                        .find(|source| source.uri == *uri)
                        .map(|source| source_revision_name(&source.revision).to_owned())
                        .unwrap_or_default()
                };
                rows.push(OutlineRow {
                    key: format!("source:{uri}"),
                    depth,
                    label: name.clone(),
                    state,
                    detail: String::new(),
                    item: OutlineItem::Source { path: uri.clone() },
                    expandable: false,
                    expanded: false,
                    tone: Tone::Normal,
                });
            }
        }
    }

    fn metrics(&self) -> (usize, usize) {
        let mut source_count = self.files.values().map(BTreeMap::len).sum();
        let mut snapshot_count = self
            .files
            .values()
            .flat_map(BTreeMap::values)
            .sum::<usize>();
        for folder in self.folders.values() {
            let (folder_sources, folder_snapshots) = folder.metrics();
            source_count += folder_sources;
            snapshot_count += folder_snapshots;
        }
        (source_count, snapshot_count)
    }
}

fn source_tree_components(uri: &str) -> Vec<String> {
    let Ok(url) = url::Url::parse(uri) else {
        return uri
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect();
    };
    if url.cannot_be_a_base() {
        return vec![uri.to_owned()];
    }
    let root = url[..url::Position::BeforePath].to_owned();
    let mut components = vec![root];
    components.extend(
        url.path_segments()
            .into_iter()
            .flatten()
            .filter(|component| !component.is_empty())
            .map(str::to_owned),
    );
    if let Some(last) = components.last_mut() {
        if let Some(query) = url.query() {
            last.push('?');
            last.push_str(query);
        }
        if let Some(fragment) = url.fragment() {
            last.push('#');
            last.push_str(fragment);
        }
    }
    components
}

fn source_folder_key(kind: SourceTreeKind, path: &[String]) -> String {
    format!("source-folder:{}:{}", source_kind_key(kind), path.join("/"))
}

fn source_kind_key(kind: SourceTreeKind) -> &'static str {
    match kind {
        SourceTreeKind::Loaded => "loaded",
        SourceTreeKind::SourceMapped => "source-mapped",
        SourceTreeKind::Formatted => "formatted",
        SourceTreeKind::Resolved => "resolved",
    }
}

fn source_kind_label(kind: SourceTreeKind) -> &'static str {
    match kind {
        SourceTreeKind::Loaded => "no projection",
        SourceTreeKind::SourceMapped => "source maps",
        SourceTreeKind::Formatted => "formatted",
        SourceTreeKind::Resolved => "resolved",
    }
}

fn source_revision_name(revision: &UncompactedSourceRevisionSnapshot) -> &'static str {
    match revision {
        UncompactedSourceRevisionSnapshot::Content { .. } => "content",
        UncompactedSourceRevisionSnapshot::Version { .. } => "version",
    }
}

fn source_revision(revision: &UncompactedSourceRevisionSnapshot) -> String {
    match revision {
        UncompactedSourceRevisionSnapshot::Content { hash } => format!("content {hash}"),
        UncompactedSourceRevisionSnapshot::Version { namespace, value } => {
            format!("{namespace}:{value}")
        }
    }
}

fn breakpoint_state(status: &BreakpointStatus) -> (&'static str, Tone) {
    match status {
        BreakpointStatus::Unconfirmed => ("unconfirmed", Tone::Muted),
        BreakpointStatus::Disabled => ("disabled", Tone::Muted),
        BreakpointStatus::Pending => ("pending", Tone::Warning),
        BreakpointStatus::PartiallyBound { .. } => ("partial", Tone::Warning),
        BreakpointStatus::Bound { .. } => ("bound", Tone::Good),
        BreakpointStatus::Failed { .. } => ("failed", Tone::Error),
    }
}

fn breakpoint_application_state(status: &BreakpointApplicationStatus) -> (&'static str, Tone) {
    match status {
        BreakpointApplicationStatus::Installing => ("installing", Tone::Warning),
        BreakpointApplicationStatus::Installed { .. } => ("installed", Tone::Good),
        BreakpointApplicationStatus::Removing => ("removing", Tone::Warning),
        BreakpointApplicationStatus::Failed { .. } => ("failed", Tone::Error),
    }
}

fn capture_kind(kind: CaptureKind) -> &'static str {
    match kind {
        CaptureKind::Coverage => "coverage",
        CaptureKind::CpuProfile => "CPU profile",
        CaptureKind::HeapSnapshot => "heap snapshot",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn display_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

fn unavailable_inspector(title: &str) -> Inspector {
    Inspector {
        title: title.to_owned(),
        lines: vec!["The selected item is no longer available.".to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdp_client::service_api::{
        ConnectionConfiguration, ProcessRole, ProcessRootKind, SourceFormattingSettings,
        TargetSnapshot,
    };

    #[test]
    fn connection_space_toggles_lifecycle_direction() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        assert_eq!(
            app.action_for_selected(false).unwrap(),
            UiAction::SetConnection {
                context_id: "ctx".to_owned(),
                connection_id: "browser".to_owned(),
                connected: true,
            }
        );

        app.context.as_mut().unwrap().connections[0].status = ConnectionStatus::Connected {
            product: "Chrome".to_owned(),
            protocol_version: "1.3".to_owned(),
        };
        assert!(matches!(
            app.action_for_selected(false).unwrap(),
            UiAction::SetConnection {
                connected: false,
                ..
            }
        ));
    }

    #[test]
    fn connection_delete_requires_an_inactive_connection() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        assert_eq!(
            app.delete_connection_action_for_selected().unwrap(),
            UiAction::DeleteConnection {
                context_id: "ctx".to_owned(),
                connection_id: "browser".to_owned(),
                expected_revision: 1,
            }
        );

        app.context.as_mut().unwrap().connections[0].status = ConnectionStatus::Connected {
            product: "Chrome".to_owned(),
            protocol_version: "1.3".to_owned(),
        };
        assert!(
            app.delete_connection_action_for_selected()
                .unwrap_err()
                .contains("disconnect connection browser")
        );
    }

    #[test]
    fn target_toggle_is_safe_for_external_owners() {
        let mut app = test_app(
            ConnectionStatus::Connected {
                product: "Chrome".to_owned(),
                protocol_version: "1.3".to_owned(),
            },
            TargetAttachmentState::External,
        );
        app.set_tab(Tab::Targets);
        app.move_selection(1);

        assert!(
            app.action_for_selected(false)
                .unwrap_err()
                .contains("press f")
        );
        assert!(matches!(
            app.action_for_selected(true).unwrap(),
            UiAction::SetTargetAttachment {
                attached: true,
                force: true,
                ..
            }
        ));
    }

    #[test]
    fn process_space_attaches_tree_root_and_process_rows() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_processes(vec![ProcessTreeSnapshot {
            root_process_id: 100,
            root_kind: ProcessRootKind::Vscode,
            processes: vec![ProcessSnapshot {
                process_id: 100,
                parent_process_id: None,
                attachable: true,
                debug_target_id: Some("$node-root".to_owned()),
                name: "Code.exe".to_owned(),
                command_line: "Code.exe".to_owned(),
                creation_date: "1".to_owned(),
                role: ProcessRole::VscodeMain,
                display_name: None,
                window_id: None,
                window_title: None,
                cpu_percent: None,
                memory_bytes: None,
                agent_sessions: Vec::new(),
            }],
            runtime_metadata_available: true,
            targets: Vec::new(),
            target_discovery_error: None,
        }]);
        app.set_tab(Tab::Processes);

        assert!(matches!(
            app.action_for_selected(false).unwrap(),
            UiAction::AttachProcess {
                process: ProcessRef {
                    root_process_id: 100,
                    process_id: 100,
                    ..
                }
            }
        ));
        app.move_selection(1);
        assert!(matches!(
            app.action_for_selected(false).unwrap(),
            UiAction::AttachProcess {
                process: ProcessRef {
                    root_process_id: 100,
                    process_id: 100,
                    ..
                }
            }
        ));
    }

    #[test]
    fn switching_context_clears_in_flight_tab_loads() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.contexts.push(ContextSummary {
            id: "other".to_owned(),
            display_name: "Other".to_owned(),
            ..app.contexts[0].clone()
        });
        assert!(app.begin_loading(Tab::Sources));

        app.select_context(1);

        assert!(!app.is_loading(Tab::Sources));
        assert!(app.initialized_source_kinds.is_empty());
    }

    #[test]
    fn starting_a_load_clears_stale_messages_but_keeps_pending_actions() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_error("old error");
        assert!(app.begin_loading(Tab::Sources));
        assert!(app.message().is_none());
        app.finish_loading(Tab::Sources);

        let action = UiAction::SetConnection {
            context_id: "ctx".to_owned(),
            connection_id: "browser".to_owned(),
            connected: true,
        };
        app.begin_action(&action);
        assert!(app.begin_loading(Tab::Sources));
        assert_eq!(
            app.message().map(|(_, message)| message),
            Some("Connecting connection browser")
        );
    }

    #[test]
    fn sources_default_to_source_maps_and_cycle_all_projection_policies() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_tab(Tab::Sources);

        assert_eq!(app.source_kind(), SourceTreeKind::SourceMapped);
        assert_eq!(app.source_kind_label(), "source maps");

        app.toggle_source_kind();
        assert_eq!(app.source_kind(), SourceTreeKind::Formatted);
        assert_eq!(app.source_kind_label(), "formatted");
        assert!(app.rows().is_empty());

        app.toggle_source_kind();
        assert_eq!(app.source_kind(), SourceTreeKind::Loaded);
        assert_eq!(app.source_kind_label(), "no projection");
        assert!(app.rows().is_empty());

        app.toggle_source_kind();
        assert_eq!(app.source_kind(), SourceTreeKind::SourceMapped);
    }

    #[test]
    fn source_rows_form_an_expandable_uri_folder_hierarchy() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_tab(Tab::Sources);
        let source = |id, uri: &str| cdp_client::service_api::UncompactedSourceNodeSnapshot {
            id,
            uri: uri.to_owned(),
            revision: UncompactedSourceRevisionSnapshot::Version {
                namespace: "test".to_owned(),
                value: id.to_string(),
            },
        };
        app.set_sources(SourceTreeSnapshot {
            kind: SourceTreeKind::SourceMapped,
            sources: vec![
                source(1, "https://example.test/src/a.ts"),
                source(2, "https://example.test/src/nested/b.ts"),
                source(3, "file:///workspace/main.ts"),
            ],
        });

        let rows = app.rows();
        assert!(rows.iter().any(|row| {
            matches!(
                &row.item,
                OutlineItem::SourceFolder { path }
                    if path == "https://example.test/src/nested"
            )
        }));
        assert!(rows.iter().any(|row| {
            row.label == "a.ts"
                && row.depth == 2
                && matches!(&row.item, OutlineItem::Source { path } if path.ends_with("/a.ts"))
        }));
        assert!(rows.iter().any(|row| {
            row.label == "b.ts"
                && row.depth == 3
                && matches!(&row.item, OutlineItem::Source { path } if path.ends_with("/b.ts"))
        }));

        app.selection[Tab::Sources.index()] = rows
            .iter()
            .position(|row| {
                matches!(
                    &row.item,
                    OutlineItem::SourceFolder { path }
                        if path == "https://example.test/src"
                )
            })
            .unwrap();
        app.set_expanded(false);
        assert!(!app.rows().iter().any(|row| row.label == "a.ts"));
    }

    #[test]
    fn selected_target_identity_includes_connection_generation() {
        let mut app = test_app(
            ConnectionStatus::Connected {
                product: "Chrome".to_owned(),
                protocol_version: "1.3".to_owned(),
            },
            TargetAttachmentState::Debugger,
        );
        app.set_tab(Tab::Targets);
        app.move_selection(1);
        let previous = app.selected_target().unwrap();

        app.context.as_mut().unwrap().target_forest[0].connection_generation += 1;

        assert_ne!(app.selected_target().unwrap(), previous);
    }

    fn test_app(connection_status: ConnectionStatus, attachment: TargetAttachmentState) -> App {
        let target = TargetSnapshot {
            target_id: "page".to_owned(),
            target_type: "page".to_owned(),
            title: "Page".to_owned(),
            url: "https://example.test".to_owned(),
            attached: attachment != TargetAttachmentState::Detached,
            parent_id: None,
            opener_id: None,
            browser_context_id: None,
            subtype: None,
        };
        let context = ContextSnapshot {
            agent_instance_id: "agent".to_owned(),
            id: "ctx".to_owned(),
            display_name: "Context".to_owned(),
            revision: 1,
            resource_revision: 1,
            connections: vec![ConnectionSnapshot {
                id: "browser".to_owned(),
                configuration: ConnectionConfiguration::DirectCdp {
                    endpoint: "ws://example".to_owned(),
                },
                generation: 1,
                status: connection_status,
                targets: vec![target.clone()],
            }],
            target_forest: vec![TargetNodeSnapshot {
                connection_id: "browser".to_owned(),
                connection_generation: 1,
                target,
                parent_target_id: None,
                attachment,
            }],
            breakpoints: Vec::new(),
            source_formatting: SourceFormattingSettings::default(),
        };
        App::new(
            vec![ContextSummary {
                agent_instance_id: "agent".to_owned(),
                id: "ctx".to_owned(),
                kind: cdp_client::context_identity::ContextKind::Named,
                path_distance: None,
                path_ancestor: None,
                display_name: "Context".to_owned(),
                revision: 1,
                connection_count: 1,
                breakpoint_count: 0,
            }],
            0,
            context,
        )
    }
}

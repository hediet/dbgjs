use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use dbgjs::api::service_api::{
    BreakpointApplicationStatus, BreakpointStatus, CaptureKind, CaptureSnapshot,
    ConnectionConfiguration, ConnectionSnapshot, ConnectionStatus, ContextSnapshot, ContextSummary,
    FrameProjectionSnapshot, ProcessSnapshot, ProcessTreeSnapshot, ResourceGraphSnapshot,
    ResourceSnapshot, SourceContentSnapshot, SourceTreeKind, SourceTreeSnapshot,
    TargetAttachmentState, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetNodeSnapshot,
    UncompactedSourceRevisionSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tab {
    Runtime,
    Debug,
}

impl Tab {
    pub const ALL: [Self; 2] = [Self::Runtime, Self::Debug];

    pub fn title(self) -> &'static str {
        match self {
            Self::Runtime => "Runtime",
            Self::Debug => "Debug",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .expect("every tab is in Tab::ALL")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Contexts,
    Processes,
    Connections,
    Targets,
    Attention,
    Sources,
    Breakpoints,
    Captures,
    CallStacks,
}

impl Section {
    pub const COUNT: usize = 9;
    pub const RUNTIME: [Self; 4] = [
        Self::Contexts,
        Self::Processes,
        Self::Connections,
        Self::Targets,
    ];
    pub const DEBUG: [Self; 5] = [
        Self::Attention,
        Self::Sources,
        Self::Breakpoints,
        Self::Captures,
        Self::CallStacks,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Contexts => "Contexts",
            Self::Processes => "Processes",
            Self::Connections => "Connections",
            Self::Targets => "Targets",
            Self::Attention => "Attention",
            Self::Sources => "Sources",
            Self::Breakpoints => "Breakpoints",
            Self::Captures => "Captures",
            Self::CallStacks => "Call Stacks",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::Contexts => 0,
            Self::Processes => 1,
            Self::Connections => 2,
            Self::Targets => 3,
            Self::Sources => 4,
            Self::Breakpoints => 5,
            Self::Captures => 6,
            Self::CallStacks => 7,
            Self::Attention => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidebarFocus {
    section: Section,
    row: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FocusPath {
    section: Section,
    node_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetRef {
    pub reference: dbgjs::api::service_api::TargetRef,
    pub connection_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionPathRef {
    pub context_id: String,
    pub connection_id: String,
    pub outline_key: String,
    pub root_process_id: u32,
    pub process_id: u32,
    pub role: dbgjs::api::service_api::ProcessRole,
    pub debug_target_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    ConfigureConnectionPath {
        path: ConnectionPathRef,
    },
    SetConnectionPathConfigured {
        path: ConnectionPathRef,
        configured: bool,
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
    PutBreakpoint {
        context_id: String,
        breakpoint_id: String,
        source_path: String,
        line: u32,
    },
    DeleteBreakpoint {
        context_id: String,
        breakpoint_id: String,
        expected_revision: u64,
    },
}

impl UiAction {
    pub fn key(&self) -> String {
        match self {
            Self::ConfigureConnectionPath { path }
            | Self::SetConnectionPathConfigured { path, .. } => path.outline_key.clone(),
            Self::SetConnection { connection_id, .. } => {
                format!("connection:{connection_id}")
            }
            Self::DeleteConnection { connection_id, .. } => {
                format!("connection:{connection_id}")
            }
            Self::SetTargetAttachment { target, .. } => {
                format!(
                    "target:{}:{}",
                    target.reference.connection.connection_id, target.reference.target_id
                )
            }
            Self::PutBreakpoint {
                source_path, line, ..
            } => format!("source-line:{source_path}:{line}"),
            Self::DeleteBreakpoint { breakpoint_id, .. } => {
                format!("breakpoint:{breakpoint_id}")
            }
        }
    }

    pub fn pending_message(&self) -> String {
        match self {
            Self::ConfigureConnectionPath { path } => {
                format!("Connecting {}", path.connection_id)
            }
            Self::SetConnectionPathConfigured { path, configured } => {
                format!(
                    "{} connection {}",
                    if *configured { "Adding" } else { "Removing" },
                    path.connection_id
                )
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
                target.reference.connection.connection_id,
                target.reference.target_id
            ),
            Self::PutBreakpoint {
                source_path, line, ..
            } => format!("Setting breakpoint at {source_path}:{line}"),
            Self::DeleteBreakpoint { breakpoint_id, .. } => {
                format!("Removing breakpoint {breakpoint_id}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutlineItem {
    Context {
        context_id: String,
    },
    ProcessTree {
        root_pid: u32,
    },
    Process {
        root_pid: u32,
        process_id: u32,
    },
    ProcessWindow {
        root_pid: u32,
        window_id: u32,
        process_id: u32,
    },
    AgentSession {
        process_id: u32,
        session_id: String,
    },
    ProcessTarget {
        root_pid: u32,
        target_id: String,
    },
    ConnectionSet {
        configured: bool,
    },
    ConnectionCandidate {
        root_pid: u32,
        process_id: u32,
    },
    Connection {
        connection_id: String,
    },
    TargetSet {
        attached: bool,
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
    Resource {
        resource_id: String,
    },
    CallStack {
        connection_id: String,
        target_id: String,
    },
    StackFrame {
        connection_id: String,
        target_id: String,
        frame_index: u32,
    },
    AttentionConnection {
        connection_id: String,
    },
    AttentionTarget {
        connection_id: String,
        target_id: String,
    },
    AttentionBreakpoint {
        breakpoint_id: String,
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
    focus: [SidebarFocus; 2],
    collapsed_sections: BTreeSet<Section>,
    section_scroll: [usize; Section::COUNT],
    section_viewport: [usize; Section::COUNT],
    expanded: BTreeSet<String>,
    last_visited_child: BTreeMap<String, String>,
    processes: Vec<ProcessTreeSnapshot>,
    resources: Option<ResourceGraphSnapshot>,
    sources: Option<SourceTreeSnapshot>,
    source_hierarchy: Option<SourceFolder>,
    source_rows: Vec<OutlineRow>,
    source_revisions: BTreeMap<String, Vec<UncompactedSourceRevisionSnapshot>>,
    source_content: Option<SourceContentSnapshot>,
    source_line: u32,
    source_scroll: usize,
    source_viewport: usize,
    document_focused: bool,
    source_kind: SourceTreeKind,
    initialized_source_kinds: BTreeSet<SourceTreeKind>,
    captures: Vec<CaptureSnapshot>,
    target_details: BTreeMap<(String, String), TargetDebuggerSnapshot>,
    loading_sections: BTreeSet<Section>,
    queried_sections: BTreeSet<Section>,
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
            tab: Tab::Runtime,
            focus: [
                SidebarFocus {
                    section: Section::Contexts,
                    row: Some(context_index),
                },
                SidebarFocus {
                    section: Section::Attention,
                    row: None,
                },
            ],
            collapsed_sections: Section::RUNTIME
                .into_iter()
                .chain(Section::DEBUG)
                .filter(|section| *section != Section::Contexts)
                .collect(),
            section_scroll: [0; Section::COUNT],
            section_viewport: [0; Section::COUNT],
            expanded: BTreeSet::new(),
            last_visited_child: BTreeMap::new(),
            processes: Vec::new(),
            resources: None,
            sources: None,
            source_hierarchy: None,
            source_rows: Vec::new(),
            source_revisions: BTreeMap::new(),
            source_content: None,
            source_line: 1,
            source_scroll: 0,
            source_viewport: 0,
            document_focused: false,
            source_kind: SourceTreeKind::SourceMapped,
            initialized_source_kinds: BTreeSet::new(),
            captures: Vec::new(),
            target_details: BTreeMap::new(),
            loading_sections: BTreeSet::new(),
            queried_sections: BTreeSet::from([Section::Contexts]),
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
        self.document_focused = false;
        if tab == Tab::Runtime {
            self.target_details.clear();
        }
        self.clamp_focus();
    }

    pub fn next_tab(&mut self, delta: isize) {
        let count = Tab::ALL.len() as isize;
        let index = (self.tab.index() as isize + delta).rem_euclid(count) as usize;
        self.set_tab(Tab::ALL[index]);
    }

    pub fn select_context(&mut self, delta: isize) -> String {
        let count = self.contexts.len() as isize;
        let context_index = (self.context_index as isize + delta).rem_euclid(count) as usize;
        self.activate_context(context_index)
    }

    pub fn select_context_id(&mut self, context_id: &str) -> Option<String> {
        let context_index = self
            .contexts
            .iter()
            .position(|context| context.id == context_id)?;
        (context_index != self.context_index).then(|| self.activate_context(context_index))
    }

    fn activate_context(&mut self, context_index: usize) -> String {
        self.context_index = context_index;
        self.context = None;
        self.resources = None;
        self.sources = None;
        self.source_hierarchy = None;
        self.source_rows.clear();
        self.source_revisions.clear();
        self.source_content = None;
        self.initialized_source_kinds.clear();
        self.captures.clear();
        self.target_details.clear();
        self.focus = [
            SidebarFocus {
                section: Section::Contexts,
                row: Some(context_index),
            },
            SidebarFocus {
                section: Section::Attention,
                row: None,
            },
        ];
        self.section_scroll = [0; Section::COUNT];
        self.source_scroll = 0;
        self.document_focused = false;
        self.expanded.clear();
        self.last_visited_child.clear();
        self.loading_sections.clear();
        self.queried_sections
            .retain(|section| *section == Section::Contexts);
        self.message = Some((
            Tone::Muted,
            format!("Querying context {}", self.context_id()),
        ));
        self.context_id().to_owned()
    }

    pub fn set_contexts(&mut self, contexts: Vec<ContextSummary>) {
        let current_context_id = self.context_id().to_owned();
        let focus_paths = self.focus_paths();
        self.queried_sections.insert(Section::Contexts);
        self.contexts = contexts;
        if let Some(context_index) = self
            .contexts
            .iter()
            .position(|context| context.id == current_context_id)
        {
            self.context_index = context_index;
        }
        self.restore_focus_paths(focus_paths);
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
        let focus_paths = self.focus_paths();
        self.target_details
            .retain(|(connection_id, target_id), detail| {
                snapshot.target_forest.iter().any(|target| {
                    target.connection_id == *connection_id
                        && target.target.target_id == *target_id
                        && target.connection_generation == detail.connection_generation
                        && target.attachment == TargetAttachmentState::Debugger
                })
            });
        if let Some(summary) = self
            .contexts
            .iter_mut()
            .find(|context| context.id == snapshot.id)
        {
            summary.revision = snapshot.revision;
            summary.connection_count = snapshot.connections.len() as u32;
            summary.breakpoint_count = snapshot.breakpoints.len() as u32;
        }
        self.context = Some(snapshot);
        self.expand_defaults();
        self.restore_focus_paths(focus_paths);
    }

    pub fn set_processes(&mut self, processes: Vec<ProcessTreeSnapshot>) {
        let focus_paths = self.focus_paths();
        self.queried_sections.insert(Section::Processes);
        self.queried_sections.remove(&Section::Connections);
        self.processes = processes;
        self.expand_defaults();
        self.restore_focus_paths(focus_paths);
    }

    pub fn set_resources(&mut self, resources: ResourceGraphSnapshot) {
        let focus_paths = self.focus_paths();
        self.queried_sections.insert(Section::Connections);
        self.resources = Some(resources);
        self.restore_focus_paths(focus_paths);
    }

    pub fn set_sources(&mut self, sources: SourceTreeSnapshot) {
        if sources.kind != self.source_kind {
            return;
        }
        self.queried_sections.insert(Section::Sources);
        if self.sources.as_ref() == Some(&sources) {
            return;
        }
        let focus_paths = self.focus_paths();
        let hierarchy = SourceFolder::from_snapshot(&sources);
        if self.initialized_source_kinds.insert(sources.kind) {
            hierarchy.collect_folder_keys(sources.kind, &mut Vec::new(), &mut self.expanded);
        }
        let mut revisions = BTreeMap::<String, Vec<_>>::new();
        for source in &sources.sources {
            revisions
                .entry(source.uri.clone())
                .or_default()
                .push(source.revision.clone());
        }
        for revisions in revisions.values_mut() {
            revisions.sort();
        }
        self.sources = Some(sources);
        self.source_hierarchy = Some(hierarchy);
        self.source_revisions = revisions;
        self.rebuild_source_rows();
        self.restore_focus_paths(focus_paths);
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
        self.source_hierarchy = None;
        self.source_rows.clear();
        self.source_revisions.clear();
        self.source_content = None;
        self.loading_sections.remove(&Section::Sources);
        self.queried_sections.remove(&Section::Sources);
        self.focus[Tab::Debug.index()] = SidebarFocus {
            section: Section::Sources,
            row: None,
        };
    }

    pub fn set_captures(&mut self, captures: Vec<CaptureSnapshot>) {
        let focus_paths = self.focus_paths();
        self.queried_sections.insert(Section::Captures);
        self.captures = captures;
        self.restore_focus_paths(focus_paths);
    }

    pub fn set_source_content(&mut self, content: Option<SourceContentSnapshot>) {
        if let Some(content) = &content {
            self.source_line = content.start_line.max(1);
            self.source_scroll = 0;
        } else {
            self.document_focused = false;
        }
        self.source_content = content;
        self.ensure_source_line_visible();
    }

    pub fn set_target_detail(&mut self, detail: Option<TargetDebuggerSnapshot>) {
        let Some(detail) = detail else {
            return;
        };
        if detail.context_id != self.context_id() {
            return;
        }
        let current = self.context.as_ref().and_then(|context| {
            context.target_forest.iter().find(|target| {
                target.connection_id == detail.connection_id
                    && target.target.target_id == detail.target_id
            })
        });
        if !current.is_some_and(|target| {
            target.attachment == TargetAttachmentState::Debugger
                && target.connection_generation == detail.connection_generation
        }) {
            return;
        }
        let focus_paths = self.focus_paths();
        self.target_details.insert(
            (detail.connection_id.clone(), detail.target_id.clone()),
            detail,
        );
        self.restore_focus_paths(focus_paths);
    }

    pub fn remove_target_detail(&mut self, connection_id: &str, target_id: &str) {
        let focus_paths = self.focus_paths();
        self.target_details
            .remove(&(connection_id.to_owned(), target_id.to_owned()));
        self.restore_focus_paths(focus_paths);
    }

    pub fn begin_loading(&mut self, section: Section) -> bool {
        let started = self.loading_sections.insert(section);
        if started && self.pending_action.is_none() {
            self.message = None;
        }
        started
    }

    pub fn needs_load(&self, section: Section) -> bool {
        if self.loading_sections.contains(&section) {
            return false;
        }
        match section {
            Section::Contexts => !self.queried_sections.contains(&section),
            Section::Processes => {
                !self.queried_sections.contains(&section)
                    || self.processes.iter().any(|tree| {
                        self.demanded_process_roots()
                            .contains(&tree.root_process_id)
                            && !tree.targets_observed
                    })
            }
            Section::Connections => {
                !self.queried_sections.contains(&section)
                    || self
                        .context
                        .as_ref()
                        .zip(self.resources.as_ref())
                        .is_some_and(|(context, resources)| {
                            context.resource_revision > resources.revision
                        })
            }
            Section::Sources | Section::Captures => !self.queried_sections.contains(&section),
            Section::Targets | Section::Attention | Section::Breakpoints | Section::CallStacks => {
                false
            }
        }
    }

    pub fn refresh_active_data(&mut self) {
        for section in self.sections().to_vec() {
            if matches!(
                section,
                Section::Contexts
                    | Section::Processes
                    | Section::Connections
                    | Section::Sources
                    | Section::Captures
            ) {
                self.queried_sections.remove(&section);
            }
        }
        if self.tab == Tab::Runtime && !self.is_section_collapsed(Section::Connections) {
            self.queried_sections.remove(&Section::Processes);
        }
    }

    pub fn finish_loading(&mut self, section: Section) {
        self.loading_sections.remove(&section);
    }

    pub fn is_loading(&self, section: Section) -> bool {
        self.loading_sections.contains(&section)
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

    pub fn sections(&self) -> &'static [Section] {
        match self.tab {
            Tab::Runtime => &Section::RUNTIME,
            Tab::Debug => &Section::DEBUG,
        }
    }

    pub fn rows(&self, section: Section) -> Cow<'_, [OutlineRow]> {
        match section {
            Section::Contexts => Cow::Owned(self.context_rows()),
            Section::Processes => Cow::Owned(self.process_rows()),
            Section::Connections => Cow::Owned(self.connection_rows()),
            Section::Targets => Cow::Owned(self.target_rows()),
            Section::Attention => Cow::Owned(self.attention_rows()),
            Section::Sources => Cow::Borrowed(&self.source_rows),
            Section::Breakpoints => Cow::Owned(self.breakpoint_rows()),
            Section::Captures => Cow::Owned(self.capture_rows()),
            Section::CallStacks => Cow::Owned(self.call_stack_rows()),
        }
    }

    pub fn section_count(&self, section: Section) -> usize {
        match section {
            Section::Contexts => self.contexts.len(),
            Section::Processes => self.processes.iter().map(|tree| tree.processes.len()).sum(),
            Section::Connections => self
                .context
                .as_ref()
                .map_or(0, |context| context.connections.len()),
            Section::Targets => self
                .context
                .as_ref()
                .map_or(0, |context| context.target_forest.len()),
            Section::Attention => self.attention_rows().len(),
            Section::Sources => self.sources.as_ref().map_or(0, |tree| tree.sources.len()),
            Section::Breakpoints => self
                .context
                .as_ref()
                .map_or(0, |context| context.breakpoints.len()),
            Section::Captures => self.captures.len(),
            Section::CallStacks => self
                .target_details
                .values()
                .filter(|detail| detail.pause.is_some())
                .count(),
        }
    }

    pub fn section_header_status(&self, section: Section) -> String {
        if self.is_loading(section) {
            return "TUI: querying…".to_owned();
        }
        if matches!(
            section,
            Section::Contexts
                | Section::Processes
                | Section::Connections
                | Section::Sources
                | Section::Captures
        ) && !self.queried_sections.contains(&section)
        {
            return if self.is_section_collapsed(section) {
                "TUI: expand to query".to_owned()
            } else {
                "TUI: awaiting result".to_owned()
            };
        }
        self.section_count(section).to_string()
    }

    pub fn section_is_queried(&self, section: Section) -> bool {
        self.queried_sections.contains(&section)
    }

    pub fn demanded_process_roots(&self) -> Vec<u32> {
        self.processes
            .iter()
            .filter(|tree| {
                (!self.is_section_collapsed(Section::Connections)
                    && tree
                        .processes
                        .iter()
                        .any(|process| process.role == dbgjs::api::service_api::ProcessRole::Renderer))
                    || tree.processes.iter().any(|process| {
                        self.expanded.contains(&format!(
                            "process:{}:{}",
                            tree.root_process_id, process.process_id
                        ))
                    })
            })
            .map(|tree| tree.root_process_id)
            .collect()
    }

    pub fn scope_label(&self) -> String {
        match self.tab {
            Tab::Runtime => "context runtime".to_owned(),
            Tab::Debug => format!(
                "context · {} attached",
                self.context.as_ref().map_or(0, |context| {
                    context
                        .target_forest
                        .iter()
                        .filter(|target| target.attachment == TargetAttachmentState::Debugger)
                        .count()
                })
            ),
        }
    }

    pub fn paused_count(&self) -> usize {
        self.target_details
            .values()
            .filter(|detail| detail.pause.is_some())
            .count()
    }

    pub fn attention_count(&self) -> usize {
        self.attention_rows().len()
    }

    pub fn issue_count(&self) -> usize {
        self.attention_count().saturating_sub(self.paused_count())
    }

    pub fn is_section_collapsed(&self, section: Section) -> bool {
        self.collapsed_sections.contains(&section)
    }

    pub fn is_header_selected(&self, section: Section) -> bool {
        self.focus[self.tab.index()] == (SidebarFocus { section, row: None })
    }

    pub fn selected_index(&self, section: Section) -> Option<usize> {
        let focus = self.focus[self.tab.index()];
        (focus.section == section).then_some(focus.row).flatten()
    }

    pub fn selected_row(&self) -> Option<OutlineRow> {
        let focus = self.focus[self.tab.index()];
        let index = focus.row?;
        self.rows(focus.section).get(index).cloned()
    }

    pub fn selected_section(&self) -> Section {
        self.focus[self.tab.index()].section
    }

    pub fn selected_context_id(&self) -> Option<&str> {
        let OutlineItem::Context { context_id } = self.selected_row()?.item else {
            return None;
        };
        Some(
            self.contexts
                .iter()
                .find(|context| context.id == context_id)?
                .id
                .as_str(),
        )
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.document_focused {
            self.move_source_line(delta);
            return;
        }
        let positions = self.focus_positions();
        let focus = self.focus[self.tab.index()];
        let current = positions
            .iter()
            .position(|candidate| *candidate == focus)
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, positions.len().saturating_sub(1) as isize)
            as usize;
        self.focus[self.tab.index()] = positions[next];
        self.remember_focus_path(self.tab.index());
        self.ensure_selected_visible();
    }

    pub fn select_edge(&mut self, last: bool) {
        if self.document_focused {
            self.source_line = if last {
                self.source_content
                    .as_ref()
                    .map_or(1, |content| content.end_line.max(content.start_line).max(1))
            } else {
                self.source_content
                    .as_ref()
                    .map_or(1, |content| content.start_line.max(1))
            };
            self.ensure_source_line_visible();
            return;
        }
        let positions = self.focus_positions();
        if let Some(focus) = if last {
            positions.last()
        } else {
            positions.first()
        } {
            self.focus[self.tab.index()] = *focus;
            self.remember_focus_path(self.tab.index());
            self.ensure_selected_visible();
        }
    }

    pub fn navigate_left(&mut self) {
        if self.document_focused {
            return;
        }
        let focus = self.focus[self.tab.index()];
        if focus.row.is_none() {
            return;
        }
        let rows = self.rows(focus.section);
        let Some(selected_index) = focus.row.filter(|index| *index < rows.len()) else {
            return;
        };
        let selected = &rows[selected_index];
        let selected_path = row_node_id_path(&rows, selected_index);
        let selected_key = selected.key.clone();
        let parent_node_id = selected_path
            .len()
            .checked_sub(2)
            .and_then(|index| selected_path.get(index))
            .cloned();
        let parent_row = parent_node_id
            .as_ref()
            .and_then(|node_id| rows.iter().position(|row| row.key == *node_id));
        drop(rows);
        if let Some(parent_node_id) = &parent_node_id {
            self.expanded.remove(parent_node_id);
        } else {
            self.collapsed_sections.insert(focus.section);
        }
        if focus.section == Section::Sources {
            self.rebuild_source_rows();
        }
        self.last_visited_child.insert(
            parent_node_id
                .clone()
                .unwrap_or_else(|| section_key(focus.section)),
            selected_key,
        );
        self.focus[self.tab.index()].row = parent_row;
        self.remember_focus_path(self.tab.index());
        self.ensure_selected_visible();
    }

    pub fn navigate_right(&mut self) {
        if self.document_focused {
            return;
        }
        let focus = self.focus[self.tab.index()];
        let parent_node_ids = if let Some(index) = focus.row {
            let rows = self.rows(focus.section);
            let Some(row) = rows.get(index).filter(|row| row.expandable) else {
                return;
            };
            let row_key = row.key.clone();
            let parent_node_ids = row_node_id_path(&rows, index);
            drop(rows);
            self.expanded.insert(row_key);
            if focus.section == Section::Sources {
                self.rebuild_source_rows();
            }
            parent_node_ids
        } else {
            self.collapsed_sections.remove(&focus.section);
            Vec::new()
        };
        let parent_key = parent_node_ids
            .last()
            .cloned()
            .unwrap_or_else(|| section_key(focus.section));
        let rows = self.rows(focus.section);
        let children = rows
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                let candidate_path = row_node_id_path(&rows, *index);
                candidate_path.len() == parent_node_ids.len() + 1
                    && candidate_path.starts_with(&parent_node_ids)
            })
            .collect::<Vec<_>>();
        let remembered = self.last_visited_child.get(&parent_key);
        let next = remembered
            .and_then(|key| {
                children
                    .iter()
                    .find(|(_, row)| row.key == *key)
                    .map(|(index, _)| *index)
            })
            .or_else(|| children.first().map(|(index, _)| *index));
        if let Some(index) = next {
            self.focus[self.tab.index()].row = Some(index);
            self.remember_focus_path(self.tab.index());
            self.ensure_selected_visible();
        }
    }

    pub fn toggle_expanded(&mut self) {
        if self.document_focused {
            return;
        }
        let focus = self.focus[self.tab.index()];
        if focus.row.is_none() {
            if !self.collapsed_sections.remove(&focus.section) {
                self.collapsed_sections.insert(focus.section);
            }
            return;
        }
        let Some(row) = self.selected_row().filter(|row| row.expandable) else {
            return;
        };
        if !self.expanded.remove(&row.key) {
            self.expanded.insert(row.key);
        }
        if focus.section == Section::Sources {
            self.rebuild_source_rows();
        }
        self.clamp_focus();
    }

    pub fn open_selected(&mut self) {
        if matches!(
            self.selected_row().map(|row| row.item),
            Some(OutlineItem::Source { .. })
        ) && self.source_content.is_some()
        {
            self.document_focused = true;
            self.ensure_source_line_visible();
        }
    }

    pub fn target_action_for_selected(&self, force: bool) -> Result<UiAction, String> {
        if self.pending_action.is_some() {
            return Err("another lifecycle operation is still pending".to_owned());
        }
        let row = self
            .selected_row()
            .ok_or_else(|| "nothing is selected".to_owned())?;
        match row.item {
            OutlineItem::Target {
                connection_id,
                target_id,
            } => {
                let target = self.target(&connection_id, &target_id)?;
                match target.attachment {
                    TargetAttachmentState::Debugger => Ok(UiAction::SetTargetAttachment {
                        target: TargetRef {
                            reference: dbgjs::api::service_api::TargetRef {
                                connection: dbgjs::api::service_api::ConnectionRef {
                                    context_id: self.context_id().to_owned(),
                                    connection_id,
                                },
                                target_id,
                            },
                            connection_generation: target.connection_generation,
                        },
                        attached: false,
                        force: false,
                    }),
                    TargetAttachmentState::Detached | TargetAttachmentState::CdpClient => Ok(UiAction::SetTargetAttachment {
                        target: TargetRef {
                            reference: dbgjs::api::service_api::TargetRef {
                                connection: dbgjs::api::service_api::ConnectionRef {
                                    context_id: self.context_id().to_owned(),
                                    connection_id,
                                },
                                target_id,
                            },
                            connection_generation: target.connection_generation,
                        },
                        attached: true,
                        force: force && target.attachment == TargetAttachmentState::CdpClient,
                    }),
                }
            }
            _ => Err("select a target to attach or detach".to_owned()),
        }
    }

    pub fn connect_action_for_selected(&self) -> Result<UiAction, String> {
        if self.pending_action.is_some() {
            return Err("another lifecycle operation is still pending".to_owned());
        }
        let row = self
            .selected_row()
            .ok_or_else(|| "select a process or connection".to_owned())?;
        match row.item {
            OutlineItem::Process {
                root_pid,
                process_id,
            } => self.connect_action_for_process(
                root_pid,
                process_id,
                row.key,
                self.process(root_pid, process_id).map(process_label)?,
            ),
            OutlineItem::ProcessWindow {
                root_pid,
                window_id,
                process_id,
            } => self.connect_action_for_process(
                root_pid,
                process_id,
                row.key,
                self.window_label(root_pid, window_id),
            ),
            OutlineItem::ConnectionCandidate {
                root_pid,
                process_id,
            } => self.connect_action_for_process(
                root_pid,
                process_id,
                row.key,
                self.process(root_pid, process_id).map(process_label)?,
            ),
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
            _ => Err("select an available or configured connection".to_owned()),
        }
    }

    pub fn toggle_connection_configuration_action_for_selected(&self) -> Result<UiAction, String> {
        if self.pending_action.is_some() {
            return Err("another lifecycle operation is still pending".to_owned());
        }
        let row = self
            .selected_row()
            .ok_or_else(|| "select an attachable process or window".to_owned())?;
        let (root_pid, process_id, connection_name) = match row.item {
            OutlineItem::Process {
                root_pid,
                process_id,
            } => (
                root_pid,
                process_id,
                self.process(root_pid, process_id).map(process_label)?,
            ),
            OutlineItem::ProcessWindow {
                root_pid,
                window_id,
                process_id,
            } => (root_pid, process_id, self.window_label(root_pid, window_id)),
            _ => return Err("select an attachable process or window".to_owned()),
        };
        let path = self.connection_path_ref(root_pid, process_id, row.key, connection_name)?;
        Ok(UiAction::SetConnectionPathConfigured {
            configured: self
                .process_connection(root_pid, self.process(root_pid, process_id)?)
                .is_none(),
            path,
        })
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

    pub fn observed_targets(&self) -> Vec<TargetRef> {
        if self.tab != Tab::Debug {
            return Vec::new();
        }
        self.context
            .iter()
            .flat_map(|context| &context.target_forest)
            .filter(|target| target.attachment == TargetAttachmentState::Debugger)
            .map(|target| TargetRef {
                reference: dbgjs::api::service_api::TargetRef {
                    connection: dbgjs::api::service_api::ConnectionRef {
                        context_id: self.context_id().to_owned(),
                        connection_id: target.connection_id.clone(),
                    },
                    target_id: target.target.target_id.clone(),
                },
                connection_generation: target.connection_generation,
            })
            .collect()
    }

    pub fn selected_source(&self) -> Option<String> {
        if self.tab != Tab::Debug {
            return None;
        }
        let OutlineItem::Source { path } = self.selected_row()?.item else {
            return self
                .source_content
                .as_ref()
                .map(|source| source.path.clone());
        };
        Some(path)
    }

    pub fn source_revision_token(&self, path: &str) -> Vec<UncompactedSourceRevisionSnapshot> {
        self.source_revisions.get(path).cloned().unwrap_or_default()
    }

    pub fn breakpoint_action_for_source_line(&self) -> Result<UiAction, String> {
        if self.pending_action.is_some() {
            return Err("another lifecycle operation is still pending".to_owned());
        }
        if !self.document_focused {
            return Err("open a source with Enter before toggling a breakpoint".to_owned());
        }
        let source = self
            .source_content
            .as_ref()
            .ok_or_else(|| "source content is still loading".to_owned())?;
        if let Some(breakpoint) = self.context.as_ref().and_then(|context| {
            context.breakpoints.iter().find(|breakpoint| {
                breakpoint.source_path == source.path && breakpoint.line == self.source_line
            })
        }) {
            return Ok(UiAction::DeleteBreakpoint {
                context_id: self.context_id().to_owned(),
                breakpoint_id: breakpoint.id.clone(),
                expected_revision: self.context.as_ref().unwrap().revision,
            });
        }
        Ok(UiAction::PutBreakpoint {
            context_id: self.context_id().to_owned(),
            breakpoint_id: source_breakpoint_id(&source.path, self.source_line),
            source_path: source.path.clone(),
            line: self.source_line,
        })
    }

    pub fn document_focused(&self) -> bool {
        self.document_focused
    }

    pub fn leave_document(&mut self) {
        self.document_focused = false;
    }

    pub fn inspector(&self) -> Inspector {
        let Some(row) = self.selected_row() else {
            return Inspector {
                title: self.selected_section().title().to_owned(),
                lines: vec!["Section header selected.".to_owned()],
            };
        };
        match row.item {
            OutlineItem::Context { context_id } => self
                .contexts
                .iter()
                .find(|context| context.id == context_id)
                .map(|context| Inspector {
                    title: context.display_name.clone(),
                    lines: vec![
                        format!("ID: {}", context.id),
                        format!("Kind: {:?}", context.kind),
                        format!("Connections: {}", context.connection_count),
                        format!("Breakpoints: {}", context.breakpoint_count),
                        if context.id == self.context_id() {
                            "Active context".to_owned()
                        } else {
                            "Press Enter to activate this context.".to_owned()
                        },
                    ],
                })
                .unwrap_or_else(|| unavailable_inspector("Context")),
            OutlineItem::ProcessTree { root_pid } => self.process_tree_inspector(root_pid),
            OutlineItem::Process {
                root_pid,
                process_id,
            } => self.process_inspector(root_pid, process_id),
            OutlineItem::ProcessWindow {
                root_pid,
                window_id,
                process_id,
            } => {
                let mut inspector = self.process_inspector(root_pid, process_id);
                inspector.title = self.window_label(root_pid, window_id);
                inspector
                    .lines
                    .insert(0, format!("VS Code window: {window_id}"));
                inspector
            }
            OutlineItem::AgentSession {
                process_id,
                session_id,
            } => Inspector {
                title: "Agent session".to_owned(),
                lines: vec![
                    format!("Process: {process_id}"),
                    format!("Session: {session_id}"),
                ],
            },
            OutlineItem::ProcessTarget {
                root_pid,
                target_id,
            } => self.process_target_inspector(root_pid, &target_id),
            OutlineItem::ConnectionSet { configured } => Inspector {
                title: if configured {
                    "Configured connections"
                } else {
                    "Available connections"
                }
                .to_owned(),
                lines: if configured {
                    vec![
                        "Durable connection recipes in this context.".to_owned(),
                        "c connects or disconnects; d removes an inactive recipe.".to_owned(),
                    ]
                } else {
                    vec![
                        "Live access paths discovered from runtime resources.".to_owned(),
                        "c persists and connects the selected path.".to_owned(),
                    ]
                },
            },
            OutlineItem::ConnectionCandidate {
                root_pid,
                process_id,
            } => self.connection_candidate_inspector(root_pid, process_id),
            OutlineItem::Connection { connection_id } => self.connection_inspector(&connection_id),
            OutlineItem::TargetSet { attached } => Inspector {
                title: if attached {
                    "Attached targets"
                } else {
                    "Available targets"
                }
                .to_owned(),
                lines: if attached {
                    vec!["Targets currently owned by this debugger context.".to_owned()]
                } else {
                    vec![
                        "Targets in configured connection scopes that are not debugger-owned."
                            .to_owned(),
                    ]
                },
            },
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
            OutlineItem::Resource { resource_id } => self.resource_inspector(&resource_id),
            OutlineItem::CallStack {
                connection_id,
                target_id,
            } => self.target_inspector(&connection_id, &target_id),
            OutlineItem::StackFrame {
                connection_id,
                target_id,
                frame_index,
            } => self.frame_inspector(&connection_id, &target_id, frame_index),
            OutlineItem::AttentionConnection { connection_id } => {
                self.connection_inspector(&connection_id)
            }
            OutlineItem::AttentionTarget {
                connection_id,
                target_id,
            } => self.target_inspector(&connection_id, &target_id),
            OutlineItem::AttentionBreakpoint { breakpoint_id } => {
                self.breakpoint_inspector(&breakpoint_id)
            }
        }
    }

    pub fn footer_hint(&self) -> String {
        if self.document_focused {
            let action = self
                .source_content
                .as_ref()
                .map_or("toggle breakpoint", |source| {
                    if self.has_breakpoint(&source.path, self.source_line) {
                        "remove breakpoint"
                    } else {
                        "set breakpoint"
                    }
                });
            return format!("b {action}  m change projection  Esc return to sidebar");
        }
        let Some(row) = self.selected_row() else {
            if self.selected_section() == Section::Sources {
                return "Enter expand/collapse sources  m change projection  r query now"
                    .to_owned();
            }
            return format!(
                "Enter expand/collapse {}  r query visible TUI sections",
                self.selected_section().title().to_lowercase()
            );
        };
        match row.item {
            OutlineItem::Context { context_id } => {
                if context_id == self.context_id() {
                    "Active context".to_owned()
                } else {
                    "Enter activate context".to_owned()
                }
            }
            OutlineItem::Process { .. } | OutlineItem::ProcessWindow { .. } => {
                "Space add/remove connection · c connect/disconnect · Right enter child".to_owned()
            }
            OutlineItem::AgentSession { .. } => "Observed Copilot agent session".to_owned(),
            OutlineItem::ConnectionSet { .. } | OutlineItem::TargetSet { .. } => {
                "Space expand/collapse projection".to_owned()
            }
            OutlineItem::ConnectionCandidate { .. } => {
                "c configure and connect this access path".to_owned()
            }
            OutlineItem::Connection { connection_id } => self
                .connection(&connection_id)
                .map(|connection| match connection.status {
                    ConnectionStatus::Disconnected | ConnectionStatus::Failed { .. } => {
                        "c connect  d remove configuration  Space resources".to_owned()
                    }
                    ConnectionStatus::Connected { .. } => {
                        "c disconnect  Space resources".to_owned()
                    }
                    ConnectionStatus::Connecting | ConnectionStatus::Disconnecting => {
                        "Connection transition in progress".to_owned()
                    }
                })
                .unwrap_or_else(|_| "Connection is no longer available".to_owned()),
            OutlineItem::Target {
                connection_id,
                target_id,
            } => self
                .target(&connection_id, &target_id)
                .map(|target| match target.attachment {
                    TargetAttachmentState::Debugger => "a detach target".to_owned(),
                    TargetAttachmentState::Detached => "a attach target".to_owned(),
                    TargetAttachmentState::CdpClient => {
                        "a attach target · f force takeover · CDP client attached".to_owned()
                    }
                })
                .unwrap_or_else(|_| "Target is no longer available".to_owned()),
            OutlineItem::Source { .. } => "Enter open source  m change projection".to_owned(),
            OutlineItem::SourceFolder { .. } => {
                "Enter expand/collapse folder  m change projection".to_owned()
            }
            OutlineItem::Breakpoint { .. } => "Enter show target applications".to_owned(),
            OutlineItem::CallStack { .. } => "Enter show paused frames".to_owned(),
            OutlineItem::AttentionConnection { .. }
            | OutlineItem::AttentionTarget { .. }
            | OutlineItem::AttentionBreakpoint { .. } => {
                "Inspect here · act from the owning Runtime or Debug section".to_owned()
            }
            OutlineItem::ProcessTree { .. }
            | OutlineItem::ProcessTarget { .. }
            | OutlineItem::TargetGroup { .. }
            | OutlineItem::BreakpointApplication { .. }
            | OutlineItem::Capture { .. }
            | OutlineItem::Resource { .. }
            | OutlineItem::StackFrame { .. } => "Enter expand/collapse or inspect".to_owned(),
        }
    }

    fn context_rows(&self) -> Vec<OutlineRow> {
        self.contexts
            .iter()
            .map(|context| {
                let active = context.id == self.context_id();
                OutlineRow {
                    key: format!("context:{}", context.id),
                    depth: 0,
                    label: format!(
                        "{}{}",
                        if active { "● " } else { "  " },
                        context.display_name
                    ),
                    state: format!(
                        "{} connection{}",
                        context.connection_count,
                        if context.connection_count == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    detail: context.id.clone(),
                    item: OutlineItem::Context {
                        context_id: context.id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone: if active { Tone::Good } else { Tone::Normal },
                }
            })
            .collect()
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
                    if pending { "[~]" } else { "" },
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

            let roots = tree
                .processes
                .iter()
                .filter(|process| {
                    process.parent_process_id.is_none()
                        || !tree.processes.iter().any(|candidate| {
                            Some(candidate.process_id) == process.parent_process_id
                        })
                })
                .collect::<Vec<_>>();
            let mut visited = BTreeSet::new();
            for process in roots {
                self.append_process_row(
                    tree,
                    process,
                    1,
                    process.window_id,
                    &mut visited,
                    &mut rows,
                );
            }
        }
        rows
    }

    fn append_process_row(
        &self,
        tree: &ProcessTreeSnapshot,
        process: &ProcessSnapshot,
        depth: usize,
        active_window: Option<u32>,
        visited: &mut BTreeSet<u32>,
        rows: &mut Vec<OutlineRow>,
    ) {
        if !visited.insert(process.process_id) {
            return;
        }
        let mut children = tree
            .processes
            .iter()
            .filter(|candidate| candidate.parent_process_id == Some(process.process_id))
            .collect::<Vec<_>>();
        children.sort_by_key(|candidate| candidate.process_id);
        let process_targets = tree
            .targets
            .iter()
            .filter(|target| target.process_id == Some(process.process_id))
            .collect::<Vec<_>>();
        let key = format!("process:{}:{}", tree.root_process_id, process.process_id);
        let expanded = self.expanded.contains(&key);
        let (connection_marker, connection_tone) =
            self.process_connection_marker(tree.root_process_id, process);
        rows.push(OutlineRow {
            key,
            depth,
            label: format!("{connection_marker} {}", process_label(process)),
            state: format!("{:?}", process.role).to_lowercase(),
            detail: if process.attachable {
                if tree.targets_observed {
                    format!(
                        "pid {} · {} runtime resources",
                        process.process_id,
                        process_targets.len()
                    )
                } else {
                    format!("pid {} · runtime unexplored", process.process_id)
                }
            } else {
                format!("pid {}", process.process_id)
            },
            item: OutlineItem::Process {
                root_pid: tree.root_process_id,
                process_id: process.process_id,
            },
            expandable: !children.is_empty()
                || !process.agent_sessions.is_empty()
                || process.attachable
                || !process_targets.is_empty(),
            expanded,
            tone: if process.attachable {
                connection_tone
            } else {
                Tone::Muted
            },
        });
        if expanded {
            for session in &process.agent_sessions {
                rows.push(OutlineRow {
                    key: format!(
                        "agent-session:{}:{}",
                        process.process_id, session.internal_id
                    ),
                    depth: depth + 1,
                    label: session
                        .title
                        .as_deref()
                        .unwrap_or(&session.internal_id)
                        .to_owned(),
                    state: if session.disconnected == Some(true) {
                        "disconnected"
                    } else {
                        "session"
                    }
                    .to_owned(),
                    detail: session.chat_uri.clone().unwrap_or_default(),
                    item: OutlineItem::AgentSession {
                        process_id: process.process_id,
                        session_id: session.internal_id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone: if session.disconnected == Some(true) {
                        Tone::Muted
                    } else {
                        Tone::Good
                    },
                });
            }
            let mut rendered_windows = BTreeSet::new();
            for child in children.drain(..) {
                match child
                    .window_id
                    .filter(|window_id| Some(*window_id) != active_window)
                {
                    Some(window_id) if rendered_windows.insert(window_id) => {
                        let window_processes = tree
                            .processes
                            .iter()
                            .filter(|candidate| {
                                candidate.parent_process_id == Some(process.process_id)
                                    && candidate.window_id == Some(window_id)
                            })
                            .collect::<Vec<_>>();
                        let selected_process = window_processes
                            .iter()
                            .copied()
                            .find(|candidate| {
                                candidate.role == dbgjs::api::service_api::ProcessRole::Renderer
                                    && candidate.attachable
                            })
                            .or_else(|| {
                                window_processes
                                    .iter()
                                    .copied()
                                    .find(|candidate| candidate.attachable)
                            })
                            .unwrap_or(child);
                        let window_title = window_processes
                            .iter()
                            .find_map(|candidate| candidate.window_title.as_deref())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("window {window_id}"));
                        let window_key =
                            format!("process-window:{}:{window_id}", tree.root_process_id);
                        let window_expanded = self.expanded.contains(&window_key);
                        let (marker, tone) =
                            self.process_connection_marker(tree.root_process_id, selected_process);
                        rows.push(OutlineRow {
                            key: window_key,
                            depth: depth + 1,
                            label: format!("{marker} {window_title}"),
                            state: "window".to_owned(),
                            detail: format!("{} processes", window_processes.len()),
                            item: OutlineItem::ProcessWindow {
                                root_pid: tree.root_process_id,
                                window_id,
                                process_id: selected_process.process_id,
                            },
                            expandable: !window_processes.is_empty(),
                            expanded: window_expanded,
                            tone,
                        });
                        if window_expanded {
                            for window_process in window_processes {
                                self.append_process_row(
                                    tree,
                                    window_process,
                                    depth + 2,
                                    Some(window_id),
                                    visited,
                                    rows,
                                );
                            }
                        }
                    }
                    Some(_) => {}
                    None => self.append_process_row(
                        tree,
                        child,
                        depth + 1,
                        active_window,
                        visited,
                        rows,
                    ),
                }
            }
            self.append_process_target_rows(tree, process.process_id, depth + 1, rows);
        }
    }

    fn append_process_target_rows(
        &self,
        tree: &ProcessTreeSnapshot,
        process_id: u32,
        depth: usize,
        rows: &mut Vec<OutlineRow>,
    ) {
        let targets = tree
            .targets
            .iter()
            .filter(|target| target.process_id == Some(process_id))
            .collect::<Vec<_>>();
        let ids = targets
            .iter()
            .map(|target| target.target.target_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut roots = targets
            .iter()
            .copied()
            .filter(|target| {
                target
                    .target
                    .parent_id
                    .as_deref()
                    .is_none_or(|parent| !ids.contains(parent))
            })
            .collect::<Vec<_>>();
        roots.sort_by(|left, right| left.target.target_id.cmp(&right.target.target_id));
        let mut visited = BTreeSet::new();
        for target in roots {
            self.append_process_target_row(tree, target, &targets, depth, &mut visited, rows);
        }
    }

    fn append_process_target_row(
        &self,
        tree: &ProcessTreeSnapshot,
        target: &dbgjs::api::service_api::ProcessTargetSnapshot,
        targets: &[&dbgjs::api::service_api::ProcessTargetSnapshot],
        depth: usize,
        visited: &mut BTreeSet<String>,
        rows: &mut Vec<OutlineRow>,
    ) {
        if !visited.insert(target.target.target_id.clone()) {
            return;
        }
        let mut children = targets
            .iter()
            .copied()
            .filter(|candidate| {
                candidate.target.parent_id.as_deref() == Some(&target.target.target_id)
            })
            .collect::<Vec<_>>();
        children.sort_by(|left, right| left.target.target_id.cmp(&right.target.target_id));
        let key = format!(
            "process-target:{}:{}",
            tree.root_process_id, target.target.target_id
        );
        let expanded = self.expanded.contains(&key);
        rows.push(OutlineRow {
            key,
            depth,
            label: display_or(&target.target.title, &target.target.target_id).to_owned(),
            state: target.target.target_type.clone(),
            detail: display_or(&target.target.url, &target.target.target_id).to_owned(),
            item: OutlineItem::ProcessTarget {
                root_pid: tree.root_process_id,
                target_id: target.target.target_id.clone(),
            },
            expandable: !children.is_empty(),
            expanded,
            tone: Tone::Muted,
        });
        if expanded {
            for child in children {
                self.append_process_target_row(tree, child, targets, depth + 1, visited, rows);
            }
        }
    }

    fn connection_candidates(&self) -> Vec<(&ProcessTreeSnapshot, &ProcessSnapshot)> {
        let mut candidates = self
            .processes
            .iter()
            .flat_map(|tree| {
                tree.processes
                    .iter()
                    .filter(|process| process.attachable)
                    .filter(|process| {
                        self.process_connection(tree.root_process_id, process)
                            .is_none()
                    })
                    .map(move |process| (tree, process))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(tree, process)| (tree.root_process_id, process.process_id));
        candidates
    }

    fn connection_rows(&self) -> Vec<OutlineRow> {
        let Some(context) = &self.context else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let candidates = self.connection_candidates();
        let available_key = "connections:available".to_owned();
        let available_expanded = self.expanded.contains(&available_key);
        rows.push(OutlineRow {
            key: available_key,
            depth: 0,
            label: "Available".to_owned(),
            state: format!("{}", candidates.len()),
            detail: "live access paths".to_owned(),
            item: OutlineItem::ConnectionSet { configured: false },
            expandable: !candidates.is_empty(),
            expanded: available_expanded,
            tone: Tone::Normal,
        });
        if available_expanded {
            for (tree, process) in candidates {
                let key = format!(
                    "connection-candidate:{}:{}",
                    tree.root_process_id, process.process_id
                );
                rows.push(OutlineRow {
                    key: key.clone(),
                    depth: 1,
                    label: process_path(tree, process.process_id),
                    state: if process.role == dbgjs::api::service_api::ProcessRole::Renderer
                        && self
                            .renderer_target(tree.root_process_id, process.process_id)
                            .is_none()
                    {
                        "expand process to resolve".to_owned()
                    } else {
                        "available".to_owned()
                    },
                    detail: format!("pid {}", process.process_id),
                    item: OutlineItem::ConnectionCandidate {
                        root_pid: tree.root_process_id,
                        process_id: process.process_id,
                    },
                    expandable: false,
                    expanded: false,
                    tone: Tone::Normal,
                });
            }
        }

        let mut connections = context.connections.iter().collect::<Vec<_>>();
        connections.sort_by(|left, right| left.id.cmp(&right.id));
        let configured_key = "connections:configured".to_owned();
        let configured_expanded = self.expanded.contains(&configured_key);
        rows.push(OutlineRow {
            key: configured_key,
            depth: 0,
            label: "Configured".to_owned(),
            state: format!("{}", connections.len()),
            detail: "durable recipes".to_owned(),
            item: OutlineItem::ConnectionSet { configured: true },
            expandable: !connections.is_empty(),
            expanded: configured_expanded,
            tone: Tone::Normal,
        });
        if !configured_expanded {
            return rows;
        }
        for connection in connections {
            let (toggle, state, tone) = connection_state(&connection.status);
            let key = format!("connection:{}", connection.id);
            let expanded = self.expanded.contains(&key);
            let toggle = if self.pending_action.as_deref() == Some(&key) {
                "[~] ◐"
            } else {
                toggle
            };
            let resource_count = self.connection_resources(&connection.id).len();
            rows.push(OutlineRow {
                key: key.clone(),
                depth: 1,
                label: format!("{toggle} {}", connection.id),
                state: state.to_owned(),
                detail: format!(
                    "{} · {} · {} resources",
                    connection_kind(&connection.configuration),
                    self.connection_path_label(connection),
                    resource_count
                ),
                item: OutlineItem::Connection {
                    connection_id: connection.id.clone(),
                },
                expandable: resource_count > 0,
                expanded,
                tone,
            });
            if expanded {
                self.append_connection_resource_rows(&connection.id, 2, &mut rows);
            }
        }
        rows
    }

    fn target_rows(&self) -> Vec<OutlineRow> {
        let Some(context) = &self.context else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for attached in [false, true] {
            let projection_key = format!(
                "targets:{}",
                if attached { "attached" } else { "available" }
            );
            let projection_targets = context
                .target_forest
                .iter()
                .filter(|target| (target.attachment == TargetAttachmentState::Debugger) == attached)
                .count();
            let projection_expanded = self.expanded.contains(&projection_key);
            rows.push(OutlineRow {
                key: projection_key,
                depth: 0,
                label: if attached { "Attached" } else { "Available" }.to_owned(),
                state: projection_targets.to_string(),
                detail: if attached {
                    "debugger-owned"
                } else {
                    "within configured scopes"
                }
                .to_owned(),
                item: OutlineItem::TargetSet { attached },
                expandable: projection_targets > 0,
                expanded: projection_expanded,
                tone: if attached { Tone::Good } else { Tone::Normal },
            });
            if projection_expanded {
                self.append_target_projection_rows(attached, context, &mut rows);
            }
        }
        rows
    }

    fn append_target_projection_rows(
        &self,
        attached: bool,
        context: &ContextSnapshot,
        rows: &mut Vec<OutlineRow>,
    ) {
        let mut connections = context.connections.iter().collect::<Vec<_>>();
        connections.sort_by(|left, right| left.id.cmp(&right.id));
        for connection in connections {
            let targets = context
                .target_forest
                .iter()
                .filter(|target| target.connection_id == connection.id)
                .filter(|target| (target.attachment == TargetAttachmentState::Debugger) == attached)
                .collect::<Vec<_>>();
            if targets.is_empty() {
                continue;
            }
            let group_key = format!(
                "target-group:{}:{}",
                if attached { "attached" } else { "available" },
                connection.id
            );
            let expanded = self.expanded.contains(&group_key);
            rows.push(OutlineRow {
                key: group_key,
                depth: 1,
                label: connection.id.clone(),
                state: connection_status_name(&connection.status).to_owned(),
                detail: format!("{} targets", targets.len()),
                item: OutlineItem::TargetGroup {
                    connection_id: connection.id.clone(),
                },
                expandable: !targets.is_empty(),
                expanded,
                tone: connection_state(&connection.status).2,
            });
            if !expanded {
                continue;
            }
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
                let selected_detail = self
                    .target_details
                    .get(&(
                        target.connection_id.clone(),
                        target.target.target_id.clone(),
                    ))
                    .filter(|detail| detail.connection_generation == target.connection_generation);
                let state = selected_detail
                    .map(|detail| target_phase(&detail.phase))
                    .unwrap_or_else(|| attachment_name(target.attachment).to_owned());
                rows.push(OutlineRow {
                    key,
                    depth: depths
                        .get(target.target.target_id.as_str())
                        .copied()
                        .unwrap_or(0)
                        + 2,
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
                        TargetAttachmentState::CdpClient => Tone::Normal,
                        TargetAttachmentState::Detached => Tone::Muted,
                    },
                });
            }
        }
    }

    fn rebuild_source_rows(&mut self) {
        let mut rows = Vec::new();
        if let Some(hierarchy) = &self.source_hierarchy {
            hierarchy.append_rows(
                &self.expanded,
                self.source_kind,
                &mut Vec::new(),
                0,
                &mut rows,
            );
        }
        self.source_rows = rows;
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

    fn call_stack_rows(&self) -> Vec<OutlineRow> {
        let mut details = self
            .target_details
            .values()
            .filter(|detail| detail.pause.is_some())
            .collect::<Vec<_>>();
        details.sort_by(|left, right| {
            (&left.connection_id, &left.target_id).cmp(&(&right.connection_id, &right.target_id))
        });
        let mut rows = Vec::new();
        for detail in details {
            let pause = detail.pause.as_ref().unwrap();
            let key = format!("call-stack:{}:{}", detail.connection_id, detail.target_id);
            let expanded = self.expanded.contains(&key);
            rows.push(OutlineRow {
                key: key.clone(),
                depth: 0,
                label: format!("{}/{}", detail.connection_id, detail.target_id),
                state: pause.reason.clone(),
                detail: format!("{} frames", pause.frames.len()),
                item: OutlineItem::CallStack {
                    connection_id: detail.connection_id.clone(),
                    target_id: detail.target_id.clone(),
                },
                expandable: !pause.frames.is_empty(),
                expanded,
                tone: Tone::Warning,
            });
            if !expanded {
                continue;
            }
            for frame in &pause.frames {
                let location = match &frame.projected {
                    FrameProjectionSnapshot::Resolved { location } => location,
                    _ => &frame.raw,
                };
                rows.push(OutlineRow {
                    key: format!(
                        "stack-frame:{}:{}:{}",
                        detail.connection_id, detail.target_id, frame.index
                    ),
                    depth: 1,
                    label: display_or(&frame.function_name, "(anonymous)").to_owned(),
                    state: format!("{}:{}", location.line, location.column),
                    detail: location.source_url.clone(),
                    item: OutlineItem::StackFrame {
                        connection_id: detail.connection_id.clone(),
                        target_id: detail.target_id.clone(),
                        frame_index: frame.index,
                    },
                    expandable: false,
                    expanded: false,
                    tone: Tone::Normal,
                });
            }
        }
        rows
    }

    fn attention_rows(&self) -> Vec<OutlineRow> {
        let Some(context) = &self.context else {
            return Vec::new();
        };
        let mut rows = Vec::new();

        let mut paused = self
            .target_details
            .values()
            .filter_map(|detail| detail.pause.as_ref().map(|pause| (detail, pause)))
            .collect::<Vec<_>>();
        paused.sort_by(|(left, _), (right, _)| {
            (&left.connection_id, &left.target_id).cmp(&(&right.connection_id, &right.target_id))
        });
        for (detail, pause) in paused {
            rows.push(OutlineRow {
                key: format!(
                    "attention:paused:{}:{}",
                    detail.connection_id, detail.target_id
                ),
                depth: 0,
                label: format!("⏸ {}/{}", detail.connection_id, detail.target_id),
                state: pause.reason.clone(),
                detail: format!("{} frames", pause.frames.len()),
                item: OutlineItem::AttentionTarget {
                    connection_id: detail.connection_id.clone(),
                    target_id: detail.target_id.clone(),
                },
                expandable: false,
                expanded: false,
                tone: Tone::Warning,
            });
        }

        for connection in &context.connections {
            if let ConnectionStatus::Failed { message } = &connection.status {
                rows.push(OutlineRow {
                    key: format!("attention:connection:{}", connection.id),
                    depth: 0,
                    label: format!("! {}", connection.id),
                    state: "connection failed".to_owned(),
                    detail: message.clone(),
                    item: OutlineItem::AttentionConnection {
                        connection_id: connection.id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone: Tone::Error,
                });
            }
        }

        for detail in self.target_details.values() {
            if let TargetDebuggerPhase::Failed { message } = &detail.phase {
                rows.push(OutlineRow {
                    key: format!(
                        "attention:target-failed:{}:{}",
                        detail.connection_id, detail.target_id
                    ),
                    depth: 0,
                    label: format!("! {}/{}", detail.connection_id, detail.target_id),
                    state: "debugger failed".to_owned(),
                    detail: message.clone(),
                    item: OutlineItem::AttentionTarget {
                        connection_id: detail.connection_id.clone(),
                        target_id: detail.target_id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone: Tone::Error,
                });
            }
        }

        for breakpoint in &context.breakpoints {
            let (state, tone, needs_attention) = match &breakpoint.status {
                BreakpointStatus::PartiallyBound { .. } => ("partially bound", Tone::Warning, true),
                BreakpointStatus::Failed { .. } => ("failed", Tone::Error, true),
                _ => ("", Tone::Normal, false),
            };
            if needs_attention {
                rows.push(OutlineRow {
                    key: format!("attention:breakpoint:{}", breakpoint.id),
                    depth: 0,
                    label: format!("! {}", breakpoint.id),
                    state: state.to_owned(),
                    detail: format!("{}:{}", breakpoint.source_path, breakpoint.line),
                    item: OutlineItem::AttentionBreakpoint {
                        breakpoint_id: breakpoint.id.clone(),
                    },
                    expandable: false,
                    expanded: false,
                    tone,
                });
            }
        }

        rows
    }

    fn connection_resources(&self, connection_id: &str) -> Vec<&ResourceSnapshot> {
        self.resources
            .iter()
            .flat_map(|graph| &graph.resources)
            .filter(|resource| {
                resource
                    .attributes
                    .get("connectionId")
                    .and_then(serde_json::Value::as_str)
                    == Some(connection_id)
            })
            .collect()
    }

    fn append_connection_resource_rows(
        &self,
        connection_id: &str,
        depth: usize,
        rows: &mut Vec<OutlineRow>,
    ) {
        let resources = self.connection_resources(connection_id);
        let ids = resources
            .iter()
            .map(|resource| resource.id.as_str())
            .collect::<BTreeSet<_>>();
        let Some(graph) = &self.resources else {
            return;
        };
        let mut incoming = BTreeSet::new();
        for relation in &graph.relations {
            if ids.contains(relation.from.as_str()) && ids.contains(relation.to.as_str()) {
                incoming.insert(relation.to.as_str());
            }
        }
        let mut roots = resources
            .iter()
            .copied()
            .filter(|resource| !incoming.contains(resource.id.as_str()))
            .collect::<Vec<_>>();
        if roots.is_empty() {
            roots = resources;
        }
        roots.sort_by(|left, right| left.id.cmp(&right.id));
        let mut visited = BTreeSet::new();
        for resource in roots {
            self.append_resource_row(resource, &ids, depth, &mut visited, rows);
        }
    }

    fn append_resource_row(
        &self,
        resource: &ResourceSnapshot,
        connection_resource_ids: &BTreeSet<&str>,
        depth: usize,
        visited: &mut BTreeSet<String>,
        rows: &mut Vec<OutlineRow>,
    ) {
        if !visited.insert(resource.id.clone()) {
            return;
        }
        let children = self.resource_children(&resource.id, connection_resource_ids);
        let key = format!("resource:{}", resource.id);
        let expanded = self.expanded.contains(&key);
        rows.push(OutlineRow {
            key,
            depth,
            label: resource
                .label
                .clone()
                .unwrap_or_else(|| resource.id.clone()),
            state: resource.kinds.join(", "),
            detail: format!("{} capabilities", resource.capabilities.len()),
            item: OutlineItem::Resource {
                resource_id: resource.id.clone(),
            },
            expandable: !children.is_empty(),
            expanded,
            tone: Tone::Normal,
        });
        if expanded {
            for child in children {
                self.append_resource_row(child, connection_resource_ids, depth + 1, visited, rows);
            }
        }
    }

    fn resource_children(
        &self,
        resource_id: &str,
        connection_resource_ids: &BTreeSet<&str>,
    ) -> Vec<&ResourceSnapshot> {
        let Some(graph) = &self.resources else {
            return Vec::new();
        };
        let child_ids = graph
            .relations
            .iter()
            .filter(|relation| {
                relation.from == resource_id
                    && connection_resource_ids.contains(relation.to.as_str())
            })
            .map(|relation| relation.to.as_str())
            .collect::<BTreeSet<_>>();
        let mut children = graph
            .resources
            .iter()
            .filter(|resource| child_ids.contains(resource.id.as_str()))
            .collect::<Vec<_>>();
        children.sort_by(|left, right| left.id.cmp(&right.id));
        children
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

    fn focus_paths(&self) -> [FocusPath; 2] {
        std::array::from_fn(|tab_index| {
            let focus = self.focus[tab_index];
            FocusPath {
                section: focus.section,
                node_ids: focus
                    .row
                    .map(|row| row_node_id_path(&self.rows(focus.section), row))
                    .unwrap_or_default(),
            }
        })
    }

    fn restore_focus_paths(&mut self, focus_paths: [FocusPath; 2]) {
        for (tab_index, path) in focus_paths.into_iter().enumerate() {
            let rows = self.rows(path.section);
            let row = if self.collapsed_sections.contains(&path.section) {
                None
            } else {
                (1..=path.node_ids.len()).rev().find_map(|path_len| {
                    rows.iter().enumerate().find_map(|(index, _)| {
                        (row_node_id_path(&rows, index) == path.node_ids[..path_len])
                            .then_some(index)
                    })
                })
            };
            self.focus[tab_index] = SidebarFocus {
                section: path.section,
                row,
            };
            self.remember_focus_path(tab_index);
        }
        self.ensure_selected_visible();
    }

    fn remember_focus_path(&mut self, tab_index: usize) {
        let focus = self.focus[tab_index];
        let Some(row) = focus.row else {
            return;
        };
        let mut path = Vec::from([section_key(focus.section)]);
        path.extend(row_node_id_path(&self.rows(focus.section), row));
        for edge in path.windows(2) {
            self.last_visited_child
                .insert(edge[0].clone(), edge[1].clone());
        }
    }

    fn clamp_focus(&mut self) {
        let tab_index = self.tab.index();
        let section = self.focus[tab_index].section;
        if self.collapsed_sections.contains(&section) {
            self.focus[tab_index].row = None;
            return;
        }
        let row_count = self.rows(section).len();
        let focus = &mut self.focus[tab_index];
        if self.collapsed_sections.contains(&focus.section) {
            focus.row = None;
            return;
        }
        if let Some(row) = &mut focus.row {
            if row_count == 0 {
                focus.row = None;
            } else {
                *row = (*row).min(row_count - 1);
            }
        }
        self.ensure_selected_visible();
    }

    fn focus_positions(&self) -> Vec<SidebarFocus> {
        let mut positions = Vec::new();
        for section in self.sections() {
            positions.push(SidebarFocus {
                section: *section,
                row: None,
            });
            if !self.is_section_collapsed(*section) && self.section_viewport[section.index()] > 0 {
                positions.extend((0..self.rows(*section).len()).map(|row| SidebarFocus {
                    section: *section,
                    row: Some(row),
                }));
            }
        }
        positions
    }

    pub fn set_section_viewport(&mut self, section: Section, height: usize) {
        self.section_viewport[section.index()] = height;
        if height == 0 {
            for focus in &mut self.focus {
                if focus.section == section {
                    focus.row = None;
                }
            }
        }
        self.ensure_section_visible(section);
    }

    pub fn section_scroll(&self, section: Section) -> usize {
        self.section_scroll[section.index()]
    }

    fn ensure_selected_visible(&mut self) {
        let focus = self.focus[self.tab.index()];
        self.ensure_section_visible(focus.section);
    }

    fn ensure_section_visible(&mut self, section: Section) {
        let selected = self
            .focus
            .iter()
            .find(|focus| focus.section == section)
            .and_then(|focus| focus.row);
        let Some(selected) = selected else {
            return;
        };
        let height = self.section_viewport[section.index()];
        if height == 0 {
            return;
        }
        let offset = &mut self.section_scroll[section.index()];
        if selected < *offset {
            *offset = selected;
        } else if selected >= *offset + height {
            *offset = selected + 1 - height;
        }
    }

    pub fn set_source_viewport(&mut self, height: usize) {
        self.source_viewport = height;
        self.ensure_source_line_visible();
    }

    pub fn source_scroll(&self) -> usize {
        self.source_scroll
    }

    pub fn source_line(&self) -> u32 {
        self.source_line
    }

    pub fn source_content(&self) -> Option<&SourceContentSnapshot> {
        self.source_content.as_ref()
    }

    pub fn has_breakpoint(&self, path: &str, line: u32) -> bool {
        self.context.as_ref().is_some_and(|context| {
            context
                .breakpoints
                .iter()
                .any(|breakpoint| breakpoint.source_path == path && breakpoint.line == line)
        })
    }

    fn move_source_line(&mut self, delta: isize) {
        let (first, last) = self.source_content.as_ref().map_or((1, 1), |content| {
            (
                content.start_line.max(1),
                content.end_line.max(content.start_line).max(1),
            )
        });
        self.source_line =
            (self.source_line as isize + delta).clamp(first as isize, last as isize) as u32;
        self.ensure_source_line_visible();
    }

    fn ensure_source_line_visible(&mut self) {
        if self.source_viewport == 0 {
            return;
        }
        let first = self
            .source_content
            .as_ref()
            .map_or(1, |content| content.start_line.max(1));
        let selected = self.source_line.saturating_sub(first) as usize;
        if selected < self.source_scroll {
            self.source_scroll = selected;
        } else if selected >= self.source_scroll + self.source_viewport {
            self.source_scroll = selected + 1 - self.source_viewport;
        }
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

    fn connect_action_for_process(
        &self,
        root_pid: u32,
        process_id: u32,
        outline_key: String,
        connection_name: String,
    ) -> Result<UiAction, String> {
        let process = self.process(root_pid, process_id)?;
        if !process.attachable {
            return Err(format!("process {process_id} is not attachable"));
        }
        if process.role == dbgjs::api::service_api::ProcessRole::Renderer
            && self.renderer_target(root_pid, process_id).is_none()
        {
            return Err(
                "expand the renderer in Processes first so its runtime access path can be resolved"
                    .to_owned(),
            );
        }
        if let Some(connection) = self.process_connection(root_pid, process) {
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
            return Ok(UiAction::SetConnection {
                context_id: self.context_id().to_owned(),
                connection_id: connection.id.clone(),
                connected,
            });
        }
        Ok(UiAction::ConfigureConnectionPath {
            path: self.connection_path_ref(root_pid, process_id, outline_key, connection_name)?,
        })
    }

    fn connection_path_ref(
        &self,
        root_pid: u32,
        process_id: u32,
        outline_key: String,
        connection_name: String,
    ) -> Result<ConnectionPathRef, String> {
        let process = self.process(root_pid, process_id)?;
        let debug_target_id = self
            .process_connection_target(root_pid, process)
            .map(str::to_owned);
        let (_, configuration) =
            connection_path_spec(root_pid, process.process_id, debug_target_id.as_deref());
        let connection_id = self
            .process_connection(root_pid, process)
            .map(|connection| connection.id.clone())
            .unwrap_or_else(|| self.available_connection_name(&connection_name, &configuration));
        Ok(ConnectionPathRef {
            context_id: self.context_id().to_owned(),
            connection_id,
            outline_key,
            root_process_id: root_pid,
            process_id: process.process_id,
            role: process.role.clone(),
            debug_target_id,
        })
    }

    fn process_connection(
        &self,
        root_pid: u32,
        process: &ProcessSnapshot,
    ) -> Option<&ConnectionSnapshot> {
        let (_, configuration) = connection_path_spec(
            root_pid,
            process.process_id,
            self.process_connection_target(root_pid, process),
        );
        self.context
            .as_ref()?
            .connections
            .iter()
            .find(|connection| connection.configuration == configuration)
    }

    fn available_connection_name(
        &self,
        node_name: &str,
        configuration: &ConnectionConfiguration,
    ) -> String {
        let base = if node_name.trim().is_empty() {
            "runtime"
        } else {
            node_name.trim()
        };
        let Some(context) = &self.context else {
            return base.to_owned();
        };
        if context
            .connections
            .iter()
            .all(|connection| connection.id != base)
            || context.connections.iter().any(|connection| {
                connection.id == base && &connection.configuration == configuration
            })
        {
            return base.to_owned();
        }
        let mut suffix = 2;
        loop {
            let candidate = format!("{base} ({suffix})");
            if context
                .connections
                .iter()
                .all(|connection| connection.id != candidate)
            {
                return candidate;
            }
            suffix += 1;
        }
    }

    fn process_connection_marker(
        &self,
        root_pid: u32,
        process: &ProcessSnapshot,
    ) -> (&'static str, Tone) {
        if !process.attachable {
            return ("·", Tone::Muted);
        }
        match self.process_connection(root_pid, process) {
            None => ("[ ]", Tone::Normal),
            Some(connection) => match connection.status {
                ConnectionStatus::Disconnected => ("[+]", Tone::Normal),
                ConnectionStatus::Connecting | ConnectionStatus::Disconnecting => {
                    ("[~]", Tone::Warning)
                }
                ConnectionStatus::Connected { .. } => ("[+] ●", Tone::Good),
                ConnectionStatus::Failed { .. } => ("[+] !", Tone::Error),
            },
        }
    }

    fn window_label(&self, root_pid: u32, window_id: u32) -> String {
        self.processes
            .iter()
            .find(|tree| tree.root_process_id == root_pid)
            .into_iter()
            .flat_map(|tree| &tree.processes)
            .filter(|process| process.window_id == Some(window_id))
            .find_map(|process| process.window_title.clone())
            .unwrap_or_else(|| format!("window {window_id}"))
    }

    fn process_connection_target<'a>(
        &'a self,
        root_pid: u32,
        process: &'a ProcessSnapshot,
    ) -> Option<&'a str> {
        self.renderer_target(root_pid, process.process_id)
            .map(|target| target.target.target_id.as_str())
            .or(process.debug_target_id.as_deref())
    }

    fn renderer_target(
        &self,
        root_pid: u32,
        process_id: u32,
    ) -> Option<&dbgjs::api::service_api::ProcessTargetSnapshot> {
        self.processes
            .iter()
            .find(|tree| tree.root_process_id == root_pid)
            .and_then(|tree| {
                tree.targets.iter().find(|target| {
                    target.process_id == Some(process_id)
                        && target.target.subtype.as_deref() == Some("electron-renderer")
                })
            })
    }

    fn connection_path_label(&self, connection: &ConnectionSnapshot) -> String {
        match &connection.configuration {
            ConnectionConfiguration::ProcessTree { root_pid } => self
                .processes
                .iter()
                .find(|tree| tree.root_process_id == *root_pid)
                .and_then(|tree| {
                    tree.processes
                        .first()
                        .map(|process| process_path(tree, process.process_id))
                })
                .unwrap_or_else(|| format!("process {root_pid}")),
            ConnectionConfiguration::ScopedProcessTree {
                root_pid,
                target_id,
            } => self
                .processes
                .iter()
                .find(|tree| tree.root_process_id == *root_pid)
                .and_then(|tree| {
                    tree.targets
                        .iter()
                        .find(|target| target.target.target_id == *target_id)
                        .and_then(|target| target.process_id)
                        .map(|process_id| process_path(tree, process_id))
                })
                .unwrap_or_else(|| format!("process {root_pid} → {target_id}")),
            _ => connection_kind(&connection.configuration).to_owned(),
        }
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
                format!("Targets observed: {}", yes_no(tree.targets_observed)),
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

    fn process_target_inspector(&self, root_pid: u32, target_id: &str) -> Inspector {
        let target = self
            .processes
            .iter()
            .find(|tree| tree.root_process_id == root_pid)
            .and_then(|tree| {
                tree.targets
                    .iter()
                    .find(|target| target.target.target_id == target_id)
            });
        let Some(target) = target else {
            return unavailable_inspector("Runtime resource");
        };
        Inspector {
            title: display_or(&target.target.title, target_id).to_owned(),
            lines: vec![
                "Canonical runtime resource projected through its process host.".to_owned(),
                format!("Type: {}", target.target.target_type),
                format!("Target ID: {}", target.target.target_id),
                format!("URL: {}", display_or(&target.target.url, "(none)")),
                format!(
                    "Host process: {}",
                    target
                        .process_id
                        .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string())
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
                "Canonical process resource".to_owned(),
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
                    "Space adds or removes this connection; c connects or disconnects it."
                        .to_owned()
                } else {
                    "This process does not expose a debugger target.".to_owned()
                },
            ],
        }
    }

    fn connection_candidate_inspector(&self, root_pid: u32, process_id: u32) -> Inspector {
        let Ok(process) = self.process(root_pid, process_id) else {
            return unavailable_inspector("Available connection");
        };
        let Some(tree) = self
            .processes
            .iter()
            .find(|tree| tree.root_process_id == root_pid)
        else {
            return unavailable_inspector("Available connection");
        };
        let (_, configuration) =
            connection_path_spec(root_pid, process_id, process.debug_target_id.as_deref());
        let mut lines = vec![
            format!("Access path: {}", process_path(tree, process_id)),
            format!("Scope root: {}", process_label(process)),
            format!("Kind: {}", connection_kind(&configuration)),
        ];
        lines.extend(connection_configuration_lines(&configuration));
        lines.push(String::new());
        lines.push("c persists this path and connects it.".to_owned());
        Inspector {
            title: "Available connection".to_owned(),
            lines,
        }
    }

    fn connection_inspector(&self, connection_id: &str) -> Inspector {
        let Ok(connection) = self.connection(connection_id) else {
            return unavailable_inspector("Connection");
        };
        let mut lines = vec![
            format!("Status: {}", connection_status_name(&connection.status)),
            format!("Kind: {}", connection_kind(&connection.configuration)),
            format!("Access path: {}", self.connection_path_label(connection)),
            format!("Generation: {}", connection.generation),
            format!("Targets: {}", connection.targets.len()),
        ];
        lines.extend(connection_configuration_lines(&connection.configuration));
        lines.push(String::new());
        lines.push("c toggles connect/disconnect.".to_owned());
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
        if let Some(detail) = self
            .target_details
            .get(&(connection_id.to_owned(), target_id.to_owned()))
            .filter(|detail| detail.connection_generation == target.connection_generation)
        {
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
            TargetAttachmentState::Debugger => "a detaches this target.".to_owned(),
            TargetAttachmentState::Detached => "a attaches this target.".to_owned(),
            TargetAttachmentState::CdpClient => {
                "A CDP client is attached; a attaches without takeover, f forces takeover.".to_owned()
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
                "Use Space to collapse or expand.".to_owned(),
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

    fn resource_inspector(&self, resource_id: &str) -> Inspector {
        let Some(resource) = self.resources.as_ref().and_then(|graph| {
            graph
                .resources
                .iter()
                .find(|resource| resource.id == resource_id)
        }) else {
            return unavailable_inspector("Resource");
        };
        let mut lines = vec![
            format!("ID: {}", resource.id),
            format!("Kinds: {}", resource.kinds.join(", ")),
            format!("Contributors: {}", resource.contributors.join(", ")),
        ];
        for (name, value) in &resource.attributes {
            lines.push(format!("{name}: {value}"));
        }
        if !resource.capabilities.is_empty() {
            lines.push(String::new());
            lines.push(format!("Capabilities: {}", resource.capabilities.len()));
            lines.extend(
                resource
                    .capabilities
                    .iter()
                    .map(|capability| format!("  {}: {}", capability.kind, capability.title)),
            );
        }
        Inspector {
            title: resource
                .label
                .clone()
                .unwrap_or_else(|| "Resource".to_owned()),
            lines,
        }
    }

    fn frame_inspector(&self, connection_id: &str, target_id: &str, frame_index: u32) -> Inspector {
        let frame = self
            .target_details
            .get(&(connection_id.to_owned(), target_id.to_owned()))
            .and_then(|detail| detail.pause.as_ref())
            .and_then(|pause| pause.frames.iter().find(|frame| frame.index == frame_index));
        let Some(frame) = frame else {
            return unavailable_inspector("Stack frame");
        };
        let projected = match &frame.projected {
            FrameProjectionSnapshot::Resolved { location } => Some(location),
            _ => None,
        };
        Inspector {
            title: display_or(&frame.function_name, "(anonymous)").to_owned(),
            lines: vec![
                format!("Target: {connection_id}/{target_id}"),
                format!(
                    "Raw: {}:{}:{}",
                    frame.raw.source_url, frame.raw.line, frame.raw.column
                ),
                projected.map_or_else(
                    || "Projected: unavailable".to_owned(),
                    |location| {
                        format!(
                            "Projected: {}:{}:{}",
                            location.source_url, location.line, location.column
                        )
                    },
                ),
                format!("Scopes: {}", frame.scopes.len()),
            ],
        }
    }
}

pub fn connection_path_spec(
    root_process_id: u32,
    process_id: u32,
    debug_target_id: Option<&str>,
) -> (String, ConnectionConfiguration) {
    if debug_target_id.is_some() {
        let debug_target_id = debug_target_id.expect("checked above");
        (
            if debug_target_id == "$node-root" {
                format!("process-tree-{root_process_id}")
            } else {
                format!("process-tree-{root_process_id}-{process_id}")
            },
            if debug_target_id == "$node-root" {
                ConnectionConfiguration::ProcessTree {
                    root_pid: root_process_id,
                }
            } else {
                ConnectionConfiguration::ScopedProcessTree {
                    root_pid: root_process_id,
                    target_id: debug_target_id.to_owned(),
                }
            },
        )
    } else {
        (
            format!("process-{process_id}"),
            ConnectionConfiguration::Process { process_id },
        )
    }
}

fn source_breakpoint_id(path: &str, line: u32) -> String {
    let hash = path
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("tui-{hash:016x}-{line}")
}

fn section_key(section: Section) -> String {
    format!("section:{}", section.index())
}

fn row_node_id_path(rows: &[OutlineRow], row_index: usize) -> Vec<String> {
    let Some(selected) = rows.get(row_index) else {
        return Vec::new();
    };
    let mut path = vec![selected.key.clone()];
    let mut child_depth = selected.depth;
    for row in rows[..row_index].iter().rev() {
        if row.depth < child_depth {
            path.push(row.key.clone());
            child_depth = row.depth;
        }
        if child_depth == 0 {
            break;
        }
    }
    path.reverse();
    path
}

fn process_path(tree: &ProcessTreeSnapshot, process_id: u32) -> String {
    let by_id = tree
        .processes
        .iter()
        .map(|process| (process.process_id, process))
        .collect::<BTreeMap<_, _>>();
    let mut current = by_id.get(&process_id).copied();
    let mut visited = BTreeSet::new();
    let mut path = Vec::new();
    while let Some(process) = current.filter(|process| visited.insert(process.process_id)) {
        path.push(process_label(process));
        current = process
            .parent_process_id
            .and_then(|parent| by_id.get(&parent).copied());
    }
    path.reverse();
    path.join(" → ")
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

fn target_label(target: &TargetNodeSnapshot) -> String {
    display_or(&target.target.title, &target.target.target_id).to_owned()
}

fn connection_state(status: &ConnectionStatus) -> (&'static str, &'static str, Tone) {
    match status {
        ConnectionStatus::Disconnected => ("[+] ○", "disconnected", Tone::Muted),
        ConnectionStatus::Connecting => ("[+] ◐", "connecting", Tone::Warning),
        ConnectionStatus::Disconnecting => ("[+] ◐", "disconnecting", Tone::Warning),
        ConnectionStatus::Connected { .. } => ("[+] ●", "connected", Tone::Good),
        ConnectionStatus::Failed { .. } => ("[+] !", "failed", Tone::Error),
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
        ConnectionConfiguration::ScopedProcessTree { .. } => "scoped process tree",
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
        ConnectionConfiguration::ScopedProcessTree {
            root_pid,
            target_id,
        } => vec![
            format!("Access root PID: {root_pid}"),
            format!("Visible scope root: {target_id}"),
        ],
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
        TargetAttachmentState::CdpClient => "[~]",
        TargetAttachmentState::Debugger => "[x]",
    }
}

fn attachment_name(state: TargetAttachmentState) -> &'static str {
    match state {
        TargetAttachmentState::Detached => "detached",
        TargetAttachmentState::CdpClient => "CDP client attached",
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

#[derive(Clone, Debug, Default)]
struct SourceFolder {
    folders: BTreeMap<String, SourceFolder>,
    files: BTreeMap<String, BTreeMap<String, SourceFile>>,
    source_count: usize,
    snapshot_count: usize,
}

#[derive(Clone, Debug)]
struct SourceFile {
    snapshot_count: usize,
    revision: String,
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
            let file = folder
                .files
                .entry(file_name)
                .or_default()
                .entry(source.uri.clone())
                .or_insert_with(|| SourceFile {
                    snapshot_count: 0,
                    revision: source_revision_name(&source.revision).to_owned(),
                });
            file.snapshot_count += 1;
        }
        root.recalculate_metrics();
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
        expanded_keys: &BTreeSet<String>,
        kind: SourceTreeKind,
        path: &mut Vec<String>,
        depth: usize,
        rows: &mut Vec<OutlineRow>,
    ) {
        for (name, folder) in &self.folders {
            path.push(name.clone());
            let key = source_folder_key(kind, path);
            let expanded = expanded_keys.contains(&key);
            rows.push(OutlineRow {
                key,
                depth,
                label: name.clone(),
                state: format!(
                    "{} source{}",
                    folder.source_count,
                    if folder.source_count == 1 { "" } else { "s" }
                ),
                detail: (folder.snapshot_count > folder.source_count)
                    .then(|| format!("{} snapshots", folder.snapshot_count))
                    .unwrap_or_default(),
                item: OutlineItem::SourceFolder {
                    path: path.join("/"),
                },
                expandable: true,
                expanded,
                tone: Tone::Normal,
            });
            if expanded {
                folder.append_rows(expanded_keys, kind, path, depth + 1, rows);
            }
            path.pop();
        }
        for (name, files) in &self.files {
            for (uri, file) in files {
                let state = if file.snapshot_count > 1 {
                    format!("{} snapshots", file.snapshot_count)
                } else {
                    file.revision.clone()
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

    fn recalculate_metrics(&mut self) -> (usize, usize) {
        let mut source_count = self.files.values().map(BTreeMap::len).sum();
        let mut snapshot_count = self
            .files
            .values()
            .flat_map(BTreeMap::values)
            .map(|file| file.snapshot_count)
            .sum::<usize>();
        for folder in self.folders.values_mut() {
            let (folder_sources, folder_snapshots) = folder.recalculate_metrics();
            source_count += folder_sources;
            snapshot_count += folder_snapshots;
        }
        self.source_count = source_count;
        self.snapshot_count = snapshot_count;
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
    use dbgjs::api::service_api::{
        ConnectionConfiguration, ProcessRole, ProcessRootKind, SourceFormattingSettings,
        TargetSnapshot,
    };

    #[test]
    fn configured_connection_uses_explicit_connect_action() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.expanded.insert("connections:configured".to_owned());
        focus_row(&mut app, Section::Connections, 2);
        assert_eq!(
            app.connect_action_for_selected().unwrap(),
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
            app.connect_action_for_selected().unwrap(),
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
        app.expanded.insert("connections:configured".to_owned());
        focus_row(&mut app, Section::Connections, 2);
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
    fn cdp_client_state_allows_plain_attach_without_implied_takeover() {
        let mut app = test_app(
            ConnectionStatus::Connected {
                product: "Chrome".to_owned(),
                protocol_version: "1.3".to_owned(),
            },
            TargetAttachmentState::CdpClient,
        );
        app.set_tab(Tab::Runtime);
        app.expanded.insert("targets:available".to_owned());
        app.expanded
            .insert("target-group:available:browser".to_owned());
        focus_row(&mut app, Section::Targets, 2);

        assert!(matches!(
            app.target_action_for_selected(false).unwrap(),
            UiAction::SetTargetAttachment {
                attached: true,
                force: false,
                ..
            }
        ));
        assert!(matches!(
            app.target_action_for_selected(true).unwrap(),
            UiAction::SetTargetAttachment {
                attached: true,
                force: true,
                ..
            }
        ));
    }

    #[test]
    fn available_process_path_is_configured_only_from_connections() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_processes(vec![test_process_tree()]);
        app.expanded.insert("connections:available".to_owned());
        focus_row(&mut app, Section::Connections, 1);

        assert!(matches!(
            app.connect_action_for_selected().unwrap(),
            UiAction::ConfigureConnectionPath {
                path: ConnectionPathRef {
                    root_process_id: 100,
                    process_id: 100,
                    ..
                },
            }
        ));
    }

    #[test]
    fn process_space_configures_while_c_configures_and_connects() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_processes(vec![test_process_tree()]);
        focus_row(&mut app, Section::Processes, 1);

        let configured = app
            .toggle_connection_configuration_action_for_selected()
            .unwrap();
        let connected = app.connect_action_for_selected().unwrap();

        assert!(matches!(
            configured,
            UiAction::SetConnectionPathConfigured {
                configured: true,
                path: ConnectionPathRef {
                    connection_id,
                    root_process_id: 100,
                    process_id: 100,
                    ..
                },
            } if connection_id == "Code.exe"
        ));
        assert!(matches!(
            connected,
            UiAction::ConfigureConnectionPath {
                path: ConnectionPathRef {
                    connection_id,
                    root_process_id: 100,
                    process_id: 100,
                    ..
                },
            } if connection_id == "Code.exe"
        ));
    }

    #[test]
    fn window_connection_uses_the_window_name_and_renderer_scope() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        let mut tree = test_process_tree();
        tree.processes.push(ProcessSnapshot {
            process_id: 200,
            parent_process_id: Some(100),
            attachable: true,
            debug_target_id: Some("process-200".to_owned()),
            name: "renderer.exe".to_owned(),
            command_line: "renderer.exe".to_owned(),
            creation_date: "1".to_owned(),
            role: ProcessRole::Renderer,
            display_name: Some("renderer".to_owned()),
            window_id: Some(7),
            window_title: Some("GitHub Objects".to_owned()),
            cpu_percent: None,
            memory_bytes: None,
            agent_sessions: Vec::new(),
        });
        tree.targets = vec![dbgjs::api::service_api::ProcessTargetSnapshot {
            process_id: Some(200),
            attachment: None,
            target: TargetSnapshot {
                target_id: "renderer-7".to_owned(),
                target_type: "page".to_owned(),
                title: "GitHub Objects".to_owned(),
                url: "vscode-file://workbench.html".to_owned(),
                attached: false,
                parent_id: None,
                opener_id: None,
                browser_context_id: None,
                subtype: Some("electron-renderer".to_owned()),
            },
        }];
        tree.targets_observed = true;
        app.set_processes(vec![tree]);
        app.collapsed_sections.remove(&Section::Processes);
        app.expanded.insert("process:100:100".to_owned());
        let window_index = app
            .rows(Section::Processes)
            .iter()
            .position(|row| row.key == "process-window:100:7")
            .unwrap();
        focus_row(&mut app, Section::Processes, window_index);

        let UiAction::SetConnectionPathConfigured { path, configured } = app
            .toggle_connection_configuration_action_for_selected()
            .unwrap()
        else {
            panic!("Space should configure the selected window path");
        };
        let (_, configuration) = connection_path_spec(
            path.root_process_id,
            path.process_id,
            path.debug_target_id.as_deref(),
        );

        assert!(configured);
        assert_eq!(path.connection_id, "GitHub Objects");
        assert_eq!(
            configuration,
            ConnectionConfiguration::ScopedProcessTree {
                root_pid: 100,
                target_id: "renderer-7".to_owned(),
            }
        );
    }

    #[test]
    fn process_tree_renders_agent_sessions_under_their_process() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        let mut tree = test_process_tree();
        tree.processes.push(ProcessSnapshot {
            process_id: 200,
            parent_process_id: Some(100),
            attachable: false,
            debug_target_id: None,
            name: "copilot.exe".to_owned(),
            command_line: "copilot.exe".to_owned(),
            creation_date: "1".to_owned(),
            role: ProcessRole::Copilot,
            display_name: Some("Copilot".to_owned()),
            window_id: None,
            window_title: None,
            cpu_percent: None,
            memory_bytes: None,
            agent_sessions: vec![dbgjs::api::service_api::AgentSessionSnapshot {
                internal_id: "session-1".to_owned(),
                chat_uri: Some("agent-host-session://session-1".to_owned()),
                title: Some("Review pull request".to_owned()),
                working_directories: vec!["D:\\dev\\dbgjs".to_owned()],
                disconnected: Some(false),
            }],
        });
        app.set_processes(vec![tree]);
        app.collapsed_sections.remove(&Section::Processes);
        app.expanded.insert("process:100:100".to_owned());
        app.expanded.insert("process:100:200".to_owned());

        assert!(app.rows(Section::Processes).iter().any(|row| {
            matches!(
                &row.item,
                OutlineItem::AgentSession {
                    process_id: 200,
                    session_id,
                } if session_id == "session-1"
            )
        }));
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
        assert!(app.begin_loading(Section::Sources));

        app.select_context(1);

        assert!(!app.is_loading(Section::Sources));
        assert!(app.initialized_source_kinds.is_empty());
    }

    #[test]
    fn contexts_panel_starts_on_the_inferred_context_and_switches_by_id() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.contexts.push(ContextSummary {
            id: "other".to_owned(),
            display_name: "Other".to_owned(),
            ..app.contexts[0].clone()
        });

        assert_eq!(app.selected_context_id(), Some("ctx"));
        assert!(app.rows(Section::Contexts)[0].label.starts_with("● "));
        assert_eq!(app.select_context_id("other").as_deref(), Some("other"));
        assert_eq!(app.context_id(), "other");
    }

    #[test]
    fn starting_a_load_clears_stale_messages_but_keeps_pending_actions() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_error("old error");
        assert!(app.begin_loading(Section::Sources));
        assert!(app.message().is_none());
        app.finish_loading(Section::Sources);

        let action = UiAction::SetConnection {
            context_id: "ctx".to_owned(),
            connection_id: "browser".to_owned(),
            connected: true,
        };
        app.begin_action(&action);
        assert!(app.begin_loading(Section::Sources));
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
        app.set_tab(Tab::Debug);

        assert_eq!(app.source_kind(), SourceTreeKind::SourceMapped);
        assert_eq!(app.source_kind_label(), "source maps");

        app.toggle_source_kind();
        assert_eq!(app.source_kind(), SourceTreeKind::Formatted);
        assert_eq!(app.source_kind_label(), "formatted");
        assert!(app.rows(Section::Sources).is_empty());

        app.toggle_source_kind();
        assert_eq!(app.source_kind(), SourceTreeKind::Loaded);
        assert_eq!(app.source_kind_label(), "no projection");
        assert!(app.rows(Section::Sources).is_empty());

        app.toggle_source_kind();
        assert_eq!(app.source_kind(), SourceTreeKind::SourceMapped);
    }

    #[test]
    fn source_rows_form_an_expandable_uri_folder_hierarchy() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_tab(Tab::Debug);
        let source = |id, uri: &str| dbgjs::api::service_api::UncompactedSourceNodeSnapshot {
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

        let rows = app.rows(Section::Sources);
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
        let source_folder_index = rows
            .iter()
            .position(|row| {
                matches!(
                    &row.item,
                    OutlineItem::SourceFolder { path }
                        if path == "https://example.test/src"
                )
            })
            .unwrap();
        drop(rows);
        let source_path = "https://example.test/src/a.ts";
        let previous_revision = app.source_revision_token(source_path);
        let mut updated_sources = app.sources.clone().unwrap();
        updated_sources.sources[0].revision = UncompactedSourceRevisionSnapshot::Version {
            namespace: "test".to_owned(),
            value: "updated".to_owned(),
        };
        app.set_sources(updated_sources);
        assert_ne!(app.source_revision_token(source_path), previous_revision);

        app.collapsed_sections.remove(&Section::Sources);
        app.focus[Tab::Debug.index()] = SidebarFocus {
            section: Section::Sources,
            row: Some(source_folder_index),
        };
        app.toggle_expanded();
        assert!(
            !app.rows(Section::Sources)
                .iter()
                .any(|row| row.label == "a.ts")
        );
    }

    #[test]
    fn large_source_tree_navigation_reuses_the_cached_outline() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_tab(Tab::Debug);
        app.set_sources(SourceTreeSnapshot {
            kind: SourceTreeKind::SourceMapped,
            sources: (0..4_282)
                .map(|id| dbgjs::api::service_api::UncompactedSourceNodeSnapshot {
                    id,
                    uri: format!("https://example.test/src/folder-{}/file-{id}.ts", id / 100),
                    revision: UncompactedSourceRevisionSnapshot::Version {
                        namespace: "test".to_owned(),
                        value: id.to_string(),
                    },
                })
                .collect(),
        });
        app.collapsed_sections.remove(&Section::Sources);
        app.set_section_viewport(Section::Sources, 20);
        let first_source = app
            .source_rows
            .iter()
            .position(|row| matches!(row.item, OutlineItem::Source { .. }))
            .unwrap();
        app.focus[Tab::Debug.index()] = SidebarFocus {
            section: Section::Sources,
            row: Some(first_source),
        };
        let cached_rows = app.source_rows.as_ptr();

        for _ in 0..100 {
            app.move_selection(1);
        }

        assert_eq!(app.source_rows.as_ptr(), cached_rows);
        assert_eq!(
            app.focus[Tab::Debug.index()],
            SidebarFocus {
                section: Section::Sources,
                row: Some(first_source + 100),
            }
        );
    }

    #[test]
    fn observed_target_identity_includes_connection_generation() {
        let mut app = test_app(
            ConnectionStatus::Connected {
                product: "Chrome".to_owned(),
                protocol_version: "1.3".to_owned(),
            },
            TargetAttachmentState::Debugger,
        );
        app.set_tab(Tab::Debug);
        let previous = app.observed_targets();

        app.context.as_mut().unwrap().target_forest[0].connection_generation += 1;

        assert_ne!(app.observed_targets(), previous);
    }

    #[test]
    fn sections_start_collapsed_and_scroll_only_when_selection_leaves_the_viewport() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        assert!(
            Section::RUNTIME
                .iter()
                .filter(|section| **section != Section::Contexts)
                .all(|section| app.is_section_collapsed(*section))
        );
        assert!(!app.is_section_collapsed(Section::Contexts));

        app.collapsed_sections.remove(&Section::Processes);
        app.focus[Tab::Runtime.index()] = SidebarFocus {
            section: Section::Processes,
            row: Some(5),
        };
        app.set_section_viewport(Section::Processes, 3);
        assert_eq!(app.section_scroll(Section::Processes), 3);

        app.focus[Tab::Runtime.index()].row = Some(4);
        app.set_section_viewport(Section::Processes, 3);
        assert_eq!(
            app.section_scroll(Section::Processes),
            3,
            "moving back within the viewport must not eagerly scroll"
        );
        app.focus[Tab::Runtime.index()].row = Some(2);
        app.set_section_viewport(Section::Processes, 3);
        assert_eq!(app.section_scroll(Section::Processes), 2);

        app.set_section_viewport(Section::Processes, 0);
        assert_eq!(app.selected_index(Section::Processes), None);
    }

    #[test]
    fn processes_show_configured_connection_state_without_duplicating_lifecycle_rows() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_processes(vec![test_process_tree()]);
        app.context
            .as_mut()
            .unwrap()
            .connections
            .push(ConnectionSnapshot {
                id: "process-tree-100".to_owned(),
                configuration: ConnectionConfiguration::ProcessTree { root_pid: 100 },
                generation: 1,
                status: ConnectionStatus::Connected {
                    product: "Node".to_owned(),
                    protocol_version: "1.3".to_owned(),
                },
                targets: Vec::new(),
            });
        focus_row(&mut app, Section::Processes, 1);

        let process = &app.rows(Section::Processes)[1];
        assert!(process.label.contains("[+] ●"));
        assert!(matches!(process.item, OutlineItem::Process { .. }));

        app.expanded.insert("connections:configured".to_owned());
        let connection_row = app
            .rows(Section::Connections)
            .iter()
            .position(|row| {
                matches!(
                    &row.item,
                    OutlineItem::Connection { connection_id }
                        if connection_id == "process-tree-100"
                )
            })
            .unwrap();
        focus_row(&mut app, Section::Connections, connection_row);
        assert!(matches!(
            app.connect_action_for_selected().unwrap(),
            UiAction::SetConnection {
                connected: false,
                ..
            }
        ));
    }

    #[test]
    fn source_document_uses_actual_line_range_and_lazy_scrolling() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_source_content(Some(SourceContentSnapshot {
            path: "file:///workspace/src/app.ts".to_owned(),
            content: "ten\neleven\ntwelve\nthirteen\nfourteen".to_owned(),
            start_line: 10,
            end_line: 14,
            total_lines: 20,
        }));
        app.document_focused = true;
        app.set_source_viewport(2);

        assert_eq!(app.source_line(), 10);
        app.move_selection(2);
        assert_eq!(app.source_line(), 12);
        assert_eq!(app.source_scroll(), 1);
        app.move_selection(-1);
        assert_eq!(app.source_scroll(), 1);
        app.move_selection(-1);
        assert_eq!(app.source_scroll(), 0);
        app.select_edge(true);
        assert_eq!(app.source_line(), 14);

        let action = app.breakpoint_action_for_source_line().unwrap();
        assert!(matches!(
            &action,
            UiAction::PutBreakpoint {
                source_path,
                line,
                ..
            } if source_path == "file:///workspace/src/app.ts" && *line == 14
        ));
        app.begin_action(&action);
        assert!(
            app.breakpoint_action_for_source_line()
                .unwrap_err()
                .contains("still pending")
        );
    }

    #[test]
    fn targets_partition_available_and_attached_resources() {
        let app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        let rows = app.rows(Section::Targets);
        assert_eq!(rows[0].label, "Available");
        assert_eq!(rows[0].state, "1");
        assert_eq!(rows[1].label, "Attached");
        assert_eq!(rows[1].state, "0");
    }

    #[test]
    fn left_and_right_retrace_the_remembered_tree_path() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        let mut tree = test_process_tree();
        tree.processes.push(ProcessSnapshot {
            process_id: 200,
            parent_process_id: Some(100),
            attachable: true,
            debug_target_id: Some("process-200-1".to_owned()),
            name: "renderer.exe".to_owned(),
            command_line: "renderer.exe".to_owned(),
            creation_date: "1".to_owned(),
            role: ProcessRole::Renderer,
            display_name: None,
            window_id: Some(7),
            window_title: Some("Project".to_owned()),
            cpu_percent: None,
            memory_bytes: None,
            agent_sessions: Vec::new(),
        });
        app.set_processes(vec![tree]);
        app.collapsed_sections.remove(&Section::Processes);
        app.expanded.insert("process:100:100".to_owned());
        app.expanded.insert("process-window:100:7".to_owned());
        let renderer_index = app
            .rows(Section::Processes)
            .iter()
            .position(|row| row.key == "process:100:200")
            .unwrap();
        app.focus[Tab::Runtime.index()] = SidebarFocus {
            section: Section::Processes,
            row: Some(renderer_index),
        };

        app.navigate_left();
        assert!(!app.expanded.contains("process-window:100:7"));
        assert!(matches!(
            app.selected_row().map(|row| row.item),
            Some(OutlineItem::ProcessWindow { window_id: 7, .. })
        ));
        app.navigate_right();
        assert!(app.expanded.contains("process-window:100:7"));
        assert!(matches!(
            app.selected_row().map(|row| row.item),
            Some(OutlineItem::Process {
                process_id: 200,
                ..
            })
        ));
    }

    #[test]
    fn left_from_a_tree_root_collapses_the_section_and_selects_its_header() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        app.set_processes(vec![test_process_tree()]);
        app.collapsed_sections.remove(&Section::Processes);
        focus_row(&mut app, Section::Processes, 0);

        app.navigate_left();

        assert!(app.is_section_collapsed(Section::Processes));
        assert!(app.is_header_selected(Section::Processes));
    }

    #[test]
    fn moving_between_children_updates_the_parent_memory() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        let mut tree = test_process_tree();
        for process_id in [200, 300] {
            tree.processes.push(ProcessSnapshot {
                process_id,
                parent_process_id: Some(100),
                attachable: true,
                debug_target_id: Some(format!("process-{process_id}")),
                name: format!("renderer-{process_id}.exe"),
                command_line: format!("renderer-{process_id}.exe"),
                creation_date: "1".to_owned(),
                role: ProcessRole::Renderer,
                display_name: None,
                window_id: None,
                window_title: None,
                cpu_percent: None,
                memory_bytes: None,
                agent_sessions: Vec::new(),
            });
        }
        app.set_processes(vec![tree]);
        app.collapsed_sections.remove(&Section::Processes);
        app.expanded.insert("process:100:100".to_owned());
        app.set_section_viewport(Section::Processes, 10);
        let renderer_index = app
            .rows(Section::Processes)
            .iter()
            .position(|row| row.key == "process:100:200")
            .unwrap();
        focus_row(&mut app, Section::Processes, renderer_index);

        app.move_selection(1);
        app.navigate_left();
        app.navigate_right();

        assert!(matches!(
            app.selected_row().map(|row| row.item),
            Some(OutlineItem::Process {
                process_id: 300,
                ..
            })
        ));
    }

    #[test]
    fn process_refresh_restores_the_cursor_by_node_id_path() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        let mut tree = test_process_tree();
        tree.processes.push(ProcessSnapshot {
            process_id: 200,
            parent_process_id: Some(100),
            attachable: true,
            debug_target_id: Some("process-200".to_owned()),
            name: "renderer-200.exe".to_owned(),
            command_line: "renderer-200.exe".to_owned(),
            creation_date: "1".to_owned(),
            role: ProcessRole::Renderer,
            display_name: None,
            window_id: None,
            window_title: None,
            cpu_percent: None,
            memory_bytes: None,
            agent_sessions: Vec::new(),
        });
        app.set_processes(vec![tree.clone()]);
        app.collapsed_sections.remove(&Section::Processes);
        app.expanded.insert("process:100:100".to_owned());
        let renderer_index = app
            .rows(Section::Processes)
            .iter()
            .position(|row| row.key == "process:100:200")
            .unwrap();
        focus_row(&mut app, Section::Processes, renderer_index);

        tree.processes.push(ProcessSnapshot {
            process_id: 150,
            parent_process_id: Some(100),
            attachable: false,
            debug_target_id: None,
            name: "utility.exe".to_owned(),
            command_line: "utility.exe".to_owned(),
            creation_date: "1".to_owned(),
            role: ProcessRole::Utility,
            display_name: None,
            window_id: None,
            window_title: None,
            cpu_percent: None,
            memory_bytes: None,
            agent_sessions: Vec::new(),
        });
        app.set_processes(vec![tree.clone()]);
        assert!(matches!(
            app.selected_row().map(|row| row.item),
            Some(OutlineItem::Process {
                process_id: 200,
                ..
            })
        ));

        tree.processes.retain(|process| process.process_id != 200);
        app.set_processes(vec![tree]);
        assert!(matches!(
            app.selected_row().map(|row| row.item),
            Some(OutlineItem::Process {
                process_id: 100,
                ..
            })
        ));
    }

    #[test]
    fn section_status_describes_tui_query_state() {
        let mut app = test_app(
            ConnectionStatus::Disconnected,
            TargetAttachmentState::Detached,
        );
        assert_eq!(
            app.section_header_status(Section::Processes),
            "TUI: expand to query"
        );
        app.collapsed_sections.remove(&Section::Processes);
        assert_eq!(
            app.section_header_status(Section::Processes),
            "TUI: awaiting result"
        );
        assert!(app.begin_loading(Section::Processes));
        assert_eq!(
            app.section_header_status(Section::Processes),
            "TUI: querying…"
        );
        app.finish_loading(Section::Processes);
        app.set_processes(vec![test_process_tree()]);
        assert_eq!(app.section_header_status(Section::Processes), "1");
    }

    #[test]
    fn native_cdp_client_is_not_misreported_as_ownership_conflict() {
        let app = test_app(
            ConnectionStatus::Failed {
                message: "endpoint refused".to_owned(),
            },
            TargetAttachmentState::CdpClient,
        );
        let rows = app.rows(Section::Attention);
        assert_eq!(rows.len(), 1);
        assert!(rows.iter().any(|row| {
            matches!(row.item, OutlineItem::AttentionConnection { .. })
                && row.detail == "endpoint refused"
        }));
    }

    fn focus_row(app: &mut App, section: Section, row: usize) {
        app.set_tab(if Section::RUNTIME.contains(&section) {
            Tab::Runtime
        } else {
            Tab::Debug
        });
        app.collapsed_sections.remove(&section);
        app.focus[app.tab.index()] = SidebarFocus {
            section,
            row: Some(row),
        };
    }

    fn test_process_tree() -> ProcessTreeSnapshot {
        ProcessTreeSnapshot {
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
            targets_observed: false,
            target_discovery_error: None,
        }
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
                kind: dbgjs::service::context_identity::ContextKind::Named,
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

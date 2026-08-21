use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::content_store::ContentStore;
use crate::debugger_engine::{
    DebuggerState, Effect, EffectId, Input, ScriptKey, ScriptSourceState,
};
use crate::source_view::{
    GeneratedSourceInput, ResolutionPolicy, ResolvedSourceView, SourceViewError,
};

pub struct SourceEffectOptions {
    pub policy: ResolutionPolicy,
    pub workspace: BTreeMap<String, String>,
    pub format_unmapped_sources: bool,
}

impl Default for SourceEffectOptions {
    fn default() -> Self {
        Self {
            policy: ResolutionPolicy::PreferSourcesContent,
            workspace: BTreeMap::new(),
            format_unmapped_sources: false,
        }
    }
}

struct RetainedView {
    script: ScriptKey,
    generated_url: String,
    view: Arc<ResolvedSourceView>,
}

pub struct SourceEffectInterpreter {
    options: SourceEffectOptions,
    store: Arc<ContentStore>,
    views: BTreeMap<EffectId, RetainedView>,
}

impl SourceEffectInterpreter {
    pub fn new(options: SourceEffectOptions, store: Arc<ContentStore>) -> Self {
        Self {
            options,
            store,
            views: BTreeMap::new(),
        }
    }

    pub fn interpret(&mut self, effect: &Effect) -> Result<Option<Input>, SourceEffectError> {
        match effect {
            Effect::BuildSourceView {
                effect_id,
                script,
                generated_url,
                content,
                source_map,
                ..
            } => {
                let mut view = ResolvedSourceView::new(
                    self.options.policy,
                    self.store.clone(),
                    self.options.workspace.clone(),
                );
                view.add_generated(GeneratedSourceInput {
                    url: generated_url,
                    content,
                    source_map: source_map.as_deref(),
                    minified: source_map.is_none() && self.options.format_unmapped_sources,
                })?;
                let logical_sources = view
                    .files()
                    .iter()
                    .map(|(url, file)| (url.clone(), file.primary.clone()))
                    .collect();
                self.views.insert(
                    *effect_id,
                    RetainedView {
                        script: script.clone(),
                        generated_url: generated_url.clone(),
                        view: Arc::new(view),
                    },
                );
                Ok(Some(Input::SourceViewBuilt {
                    effect_id: *effect_id,
                    logical_sources,
                }))
            }
            Effect::MapBreakpoint {
                effect_id,
                script,
                view_id,
                source_url,
                position,
                ..
            } => {
                let retained = self.view_for(*view_id, script)?;
                let generated_positions = retained
                    .view
                    .reverse(source_url, *position)
                    .into_iter()
                    .filter(|candidate| candidate.source_url == retained.generated_url)
                    .map(|candidate| candidate.position)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                Ok(Some(Input::BreakpointMapped {
                    effect_id: *effect_id,
                    generated_positions,
                }))
            }
            Effect::MapFrame {
                effect_id,
                script,
                view_id,
                position,
                ..
            } => {
                let retained = self.view_for(*view_id, script)?;
                let mapped = retained
                    .view
                    .forward(&retained.generated_url, *position)
                    .into_iter()
                    .next()
                    .ok_or(SourceEffectError::UnmappedPosition {
                        view_id: *view_id,
                        position: *position,
                    })?;
                Ok(Some(Input::FrameMapped {
                    effect_id: *effect_id,
                    source_url: mapped.source_url,
                    position: mapped.position,
                }))
            }
            _ => Ok(None),
        }
    }

    pub fn retain_for_state(&mut self, state: &DebuggerState) {
        let retained_ids: BTreeSet<_> = state
            .scripts
            .values()
            .filter_map(|script| match &script.source {
                ScriptSourceState::Resolved(view) => Some(view.view_id),
                _ => None,
            })
            .collect();
        self.views
            .retain(|view_id, _| retained_ids.contains(view_id));
    }

    pub fn retained_view_count(&self) -> usize {
        self.views.len()
    }

    pub fn logical_source_content(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        source_url: &str,
    ) -> Option<Arc<str>> {
        state
            .scripts
            .get(script_key)
            .and_then(|script| match &script.source {
                ScriptSourceState::Resolved(view) => view
                    .logical_sources
                    .get(source_url)
                    .and_then(|candidate| self.store.get(candidate.content)),
                _ => None,
            })
    }

    fn view_for(
        &self,
        view_id: EffectId,
        script: &ScriptKey,
    ) -> Result<&RetainedView, SourceEffectError> {
        let retained = self
            .views
            .get(&view_id)
            .ok_or(SourceEffectError::UnknownView(view_id))?;
        if &retained.script != script {
            return Err(SourceEffectError::ScriptMismatch {
                view_id,
                expected: retained.script.clone(),
                actual: script.clone(),
            });
        }
        Ok(retained)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceEffectError {
    #[error(transparent)]
    SourceView(#[from] SourceViewError),
    #[error("source view {0:?} is not retained")]
    UnknownView(EffectId),
    #[error("source view {view_id:?} belongs to {expected:?}, not {actual:?}")]
    ScriptMismatch {
        view_id: EffectId,
        expected: ScriptKey,
        actual: ScriptKey,
    },
    #[error("source view {view_id:?} cannot map generated position {position:?}")]
    UnmappedPosition {
        view_id: EffectId,
        position: crate::source_view::Position,
    },
}

#[cfg(test)]
mod tests {
    use sourcemap::SourceMapBuilder;

    use super::*;
    use crate::debugger_engine::{
        BreakpointBinding, BreakpointKey, Diagnostic, FrameProjection, RawFrame, SessionPhase,
        reduce,
    };
    use crate::source_view::Position;

    #[test]
    fn drives_real_source_maps_through_a_complete_debugger_scenario() {
        let (first, first_revisions) = run_scenario();
        let (second, second_revisions) = run_scenario();

        assert_eq!(first_revisions, second_revisions);
        assert_eq!(first.revision, second.revision);
        assert_eq!(first.sessions.len(), 0);
        assert_eq!(first.scripts.len(), 0);
        assert_eq!(first.physical_breakpoints.len(), 0);
        assert!(matches!(
            first.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { .. })
        ));
    }

    fn run_scenario() -> (Arc<DebuggerState>, Vec<u64>) {
        let mut revisions = Vec::new();
        let mut state = Arc::new(DebuggerState::default());
        let mut interpreter = SourceEffectInterpreter::new(
            SourceEffectOptions::default(),
            Arc::new(ContentStore::default()),
        );

        state = apply(&state, Input::Connected, &mut revisions).state;
        let attached = apply(
            &state,
            Input::SessionAttached {
                session_id: "session-1".into(),
                target_id: "target-1".into(),
                parent_session_id: None,
                waiting_for_debugger: true,
            },
            &mut revisions,
        );
        let Effect::ConfigureSession { effect_id, session } = attached.effects[0].clone() else {
            panic!("expected session configuration");
        };
        let configured = apply(
            &attached.state,
            Input::SessionConfigured { effect_id },
            &mut revisions,
        );
        let waiting_for_debugger = configured
            .state
            .sessions
            .get(&session)
            .map(|session| session.waiting_for_debugger)
            .expect("session exists");
        assert!(waiting_for_debugger);
        let Effect::RunIfWaitingForDebugger {
            effect_id: run_effect,
            ..
        } = configured.effects[0]
        else {
            panic!("expected run-if-waiting");
        };
        state = apply(
            &configured.state,
            Input::CommandAccepted {
                effect_id: run_effect,
            },
            &mut revisions,
        )
        .state;

        let parsed = apply(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "script-1".into(),
                url: "file:///bundle.js".into(),
                hash: "hash-1".into(),
                source_map_url: Some("file:///bundle.js.map".into()),
            },
            &mut revisions,
        );
        assert!(parsed.effects.is_empty());
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        let requested = apply(
            &parsed.state,
            Input::RequestScriptSource {
                script: script.clone(),
            },
            &mut revisions,
        );
        let Effect::FetchScriptSource {
            effect_id: fetch_id,
            script: effect_script,
            ..
        } = requested.effects[0].clone()
        else {
            panic!("expected source fetch");
        };
        assert_eq!(effect_script, script);
        let fetched = apply(
            &requested.state,
            Input::ScriptSourceFetched {
                effect_id: fetch_id,
                content: Arc::from("var answer=42;"),
                source_map: Some(Arc::from(source_map())),
                source_map_error: None,
            },
            &mut revisions,
        );
        let build = &fetched.effects[0];
        let built_input = interpreter
            .interpret(build)
            .expect("source view builds")
            .expect("build is a source effect");
        state = apply(&fetched.state, built_input, &mut revisions).state;
        interpreter.retain_for_state(&state);
        assert_eq!(interpreter.retained_view_count(), 1);

        let breakpoint = BreakpointKey {
            client_id: "client-1".into(),
            breakpoint_id: "breakpoint-1".into(),
        };
        let mapped = apply(
            &state,
            Input::SetBreakpoint {
                key: breakpoint.clone(),
                source_url: "src/app.ts".into(),
                position: Position {
                    line: 0,
                    column: 10,
                },
                condition: None,
            },
            &mut revisions,
        );
        let mapped_input = interpreter
            .interpret(&mapped.effects[0])
            .expect("breakpoint maps")
            .expect("mapping is a source effect");
        let installing = apply(&mapped.state, mapped_input, &mut revisions);
        let Effect::InstallBreakpoint {
            effect_id: install_id,
            physical,
        } = installing.effects[0].clone()
        else {
            panic!("expected physical breakpoint installation");
        };
        assert_eq!(
            physical.position,
            Position {
                line: 0,
                column: 10
            }
        );
        state = apply(
            &installing.state,
            Input::BreakpointInstalled {
                effect_id: install_id,
                backend_id: "chrome-breakpoint-1".into(),
            },
            &mut revisions,
        )
        .state;
        assert!(matches!(
            state.breakpoints[&breakpoint].bindings.values().next(),
            Some(BreakpointBinding::Installed { backend_id })
                if backend_id == "chrome-breakpoint-1"
        ));

        let paused = apply(
            &state,
            Input::Paused {
                session: session.clone(),
                reason: "breakpoint".into(),
                frames: vec![RawFrame {
                    call_frame_id: "frame-1".into(),
                    function_name: "main".into(),
                    script_id: script.script_id.clone(),
                    position: Position {
                        line: 0,
                        column: 10,
                    },
                }],
            },
            &mut revisions,
        );
        let late_frame_input = interpreter
            .interpret(&paused.effects[0])
            .expect("frame maps")
            .expect("frame mapping is a source effect");
        state = apply(&paused.state, late_frame_input.clone(), &mut revisions).state;
        let pause = state.sessions[&session].pause.as_ref().expect("paused");
        assert!(matches!(
            pause.frames[0].projected,
            FrameProjection::Resolved {
                ref source_url,
                position: Position {
                    line: 0,
                    column: 10
                }
            } if source_url == "src/app.ts"
        ));
        let pause_epoch = pause.epoch;

        let resuming = apply(
            &state,
            Input::ResumeRequested {
                session: session.clone(),
                pause_epoch,
            },
            &mut revisions,
        );
        assert!(matches!(
            resuming.state.sessions[&session].phase,
            SessionPhase::Resuming { epoch } if epoch == pause_epoch
        ));
        let Effect::Resume {
            effect_id: resume_id,
            ..
        } = resuming.effects[0]
        else {
            panic!("expected resume command");
        };
        let resumed = apply(
            &resuming.state,
            Input::CommandAccepted {
                effect_id: resume_id,
            },
            &mut revisions,
        );
        state = apply(
            &resumed.state,
            Input::Resumed {
                session: session.clone(),
                pause_epoch,
            },
            &mut revisions,
        )
        .state;
        assert!(matches!(
            state.sessions[&session].phase,
            SessionPhase::Running
        ));

        state = apply(
            &state,
            Input::SessionDetached {
                session: session.clone(),
            },
            &mut revisions,
        )
        .state;
        interpreter.retain_for_state(&state);
        assert_eq!(interpreter.retained_view_count(), 0);

        state = apply(&state, late_frame_input, &mut revisions).state;
        (state, revisions)
    }

    fn apply(
        state: &Arc<DebuggerState>,
        input: Input,
        revisions: &mut Vec<u64>,
    ) -> crate::debugger_engine::Transition {
        let transition = reduce(state, input);
        revisions.push(transition.state.revision);
        transition
    }

    fn source_map() -> Vec<u8> {
        let mut builder = SourceMapBuilder::new(Some("bundle.js"));
        let source = builder.add_source("src/app.ts");
        builder.set_source_contents(source, Some("let answer: number = 42;"));
        builder.add(0, 0, 0, 0, Some("src/app.ts"), None, false);
        builder.add(0, 10, 0, 10, Some("src/app.ts"), None, false);
        let mut raw = Vec::new();
        builder
            .into_sourcemap()
            .to_writer(&mut raw)
            .expect("map serializes");
        raw
    }
}

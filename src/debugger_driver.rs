use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cdp::CdpClient;
use crate::cdp_runtime::{
    CdpDebuggerSession, CdpRuntimeError, CdpRuntimeEvent, CdpRuntimeEventError,
    HeapSnapshotStreamProgress, SourceMapCacheStats,
};
use crate::debugger_engine::{DebuggerState, Effect, Input, SessionPhase, reduce};
use crate::source_effects::{SourceEffectError, SourceEffectInterpreter};

pub struct DebuggerDriver {
    state: Arc<DebuggerState>,
    session: CdpDebuggerSession,
    sources: SourceEffectInterpreter,
    recording: DebuggerRecording,
    console_messages: VecDeque<(u64, Vec<String>)>,
    next_console_index: u64,
}

impl DebuggerDriver {
    pub fn new(
        state: Arc<DebuggerState>,
        session: CdpDebuggerSession,
        sources: SourceEffectInterpreter,
    ) -> Self {
        Self {
            state,
            session,
            sources,
            recording: DebuggerRecording::default(),
            console_messages: VecDeque::new(),
            next_console_index: 1,
        }
    }

    pub fn state(&self) -> &Arc<DebuggerState> {
        &self.state
    }

    pub fn client(&self) -> &CdpClient<hubrpc::connection::channel::Channel> {
        self.session.client()
    }

    pub fn heap_snapshot_progress(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<HeapSnapshotStreamProgress>> {
        self.session.heap_snapshot_progress()
    }

    pub async fn begin_heap_snapshot(&self, destination: PathBuf) -> std::io::Result<()> {
        self.session.begin_heap_snapshot(destination).await
    }

    pub(crate) async fn finish_heap_snapshot(
        &self,
    ) -> std::io::Result<crate::cdp_runtime::HeapSnapshotWriteResult> {
        self.session.finish_heap_snapshot().await
    }

    pub async fn abort_heap_snapshot(&self) {
        self.session.abort_heap_snapshot().await;
    }

    pub fn set_source_map_cache_enabled(&self, enabled: bool) {
        self.session.set_source_map_cache_enabled(enabled);
    }

    pub fn source_map_cache_stats(&self) -> SourceMapCacheStats {
        self.session.source_map_cache_stats()
    }

    pub fn recording(&self) -> &DebuggerRecording {
        &self.recording
    }

    pub fn console_messages(&self) -> &VecDeque<(u64, Vec<String>)> {
        &self.console_messages
    }

    pub fn logical_source_content(
        &self,
        script: &crate::debugger_engine::ScriptKey,
        source_url: &str,
    ) -> Option<Arc<str>> {
        self.sources
            .logical_source_content(&self.state, script, source_url)
    }

    pub fn generated_source_content(
        &self,
        script: &crate::debugger_engine::ScriptKey,
    ) -> Option<Arc<str>> {
        self.sources.generated_source_content(&self.state, script)
    }

    pub fn clear_source_caches(&self) {
        self.sources.clear_caches();
    }

    pub fn project_generated_offset(
        &self,
        script: &crate::debugger_engine::ScriptKey,
        utf16_offset: u32,
    ) -> Option<(String, crate::source_view::Position, Arc<str>)> {
        self.sources
            .project_generated_offset(&self.state, script, utf16_offset)
    }

    pub fn source_effects(&self) -> &SourceEffectInterpreter {
        &self.sources
    }

    pub fn breadcrumb(
        &self,
        script: &crate::debugger_engine::ScriptKey,
        source_url: &str,
        line: u32,
        column: u32,
        content: &str,
    ) -> Option<String> {
        self.sources
            .breadcrumb(&self.state, script, source_url, line, column, content)
    }

    pub async fn apply(&mut self, input: Input) -> Result<(), DebuggerDriverError> {
        let effects = self.reduce_recorded(input);
        self.drain_effects(effects).await
    }

    pub async fn process_next_event(&mut self) -> Result<bool, DebuggerDriverError> {
        let event = self
            .session
            .next_event()
            .await
            .ok_or(DebuggerDriverError::EventStreamClosed)??;
        let pause_epoch = match &event {
            CdpRuntimeEvent::Resumed { session } => {
                self.state
                    .sessions
                    .get(session)
                    .and_then(|state| match state.phase {
                        SessionPhase::Paused { epoch } | SessionPhase::Resuming { epoch } => {
                            Some(epoch)
                        }
                        _ => None,
                    })
            }
            _ => None,
        };
        if let CdpRuntimeEvent::Console { params, .. } = &event {
            const MAX_CONSOLE_MESSAGES: usize = 100;
            if self.console_messages.len() == MAX_CONSOLE_MESSAGES {
                self.console_messages.pop_front();
            }
            let index = self.next_console_index;
            self.next_console_index = self.next_console_index.saturating_add(1);
            self.console_messages.push_back((
                index,
                params
                    .args
                    .iter()
                    .map(|argument| {
                        argument
                            .value
                            .as_ref()
                            .map(|value| match value {
                                serde_json::Value::String(value) => value.clone(),
                                value => value.to_string(),
                            })
                            .or_else(|| argument.unserializable_value.clone())
                            .or_else(|| argument.description.clone())
                            .unwrap_or_else(|| "undefined".to_owned())
                    })
                    .collect(),
            ));
            self.apply(Input::ConsoleMessageObserved).await?;
            return Ok(true);
        }
        let Some(input) = event.into_input(pause_epoch)? else {
            return Ok(false);
        };
        self.apply(input).await?;
        Ok(true)
    }

    async fn drain_effects(&mut self, effects: Vec<Effect>) -> Result<(), DebuggerDriverError> {
        let mut queue = VecDeque::from(effects);
        while let Some(effect) = queue.pop_front() {
            let completion =
                match source_effect_completion(&effect, self.sources.interpret(&effect)) {
                    Some(input) => Some(input),
                    None => self.session.execute(&effect).await?,
                };
            let Some(completion) = completion else {
                return Err(DebuggerDriverError::UnhandledEffect(effect));
            };
            queue.extend(self.reduce_recorded(completion));
        }
        Ok(())
    }

    fn reduce_recorded(&mut self, input: Input) -> Vec<Effect> {
        let transition = reduce(&self.state, input.clone());
        let effects = transition.effects;
        self.recording.transitions.push(RecordedTransition {
            input,
            effects: effects.clone(),
        });
        self.state = transition.state;
        self.sources.retain_for_state(&self.state);
        effects
    }
}

fn source_effect_completion(
    effect: &Effect,
    result: Result<Option<Input>, SourceEffectError>,
) -> Option<Input> {
    match result {
        Ok(completion) => completion,
        Err(error) => Some(Input::EffectFailed {
            effect_id: effect.effect_id(),
            message: error.to_string(),
        }),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebuggerRecording {
    pub transitions: Vec<RecordedTransition>,
}

impl DebuggerRecording {
    pub fn replay(
        &self,
        initial: Arc<DebuggerState>,
    ) -> Result<Arc<DebuggerState>, DebuggerReplayError> {
        let mut state = initial;
        for (index, recorded) in self.transitions.iter().enumerate() {
            let transition = reduce(&state, recorded.input.clone());
            if transition.effects != recorded.effects {
                return Err(DebuggerReplayError::EffectDivergence {
                    index,
                    expected: recorded.effects.clone(),
                    actual: transition.effects,
                });
            }
            state = transition.state;
        }
        Ok(state)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedTransition {
    pub input: Input,
    pub effects: Vec<Effect>,
}

#[derive(Debug, thiserror::Error)]
pub enum DebuggerReplayError {
    #[error("debugger replay diverged at transition {index}")]
    EffectDivergence {
        index: usize,
        expected: Vec<Effect>,
        actual: Vec<Effect>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DebuggerDriverError {
    #[error(transparent)]
    Runtime(#[from] CdpRuntimeError),
    #[error(transparent)]
    RuntimeEvent(#[from] CdpRuntimeEventError),
    #[error(transparent)]
    Source(#[from] SourceEffectError),
    #[error("CDP event stream closed")]
    EventStreamClosed,
    #[error("no interpreter handled debugger effect: {0:?}")]
    UnhandledEffect(Effect),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debugger_engine::{EffectId, ScriptKey, SessionKey};
    use crate::source_view::Position;

    #[test]
    fn source_mapping_failure_completes_effect_without_stopping_driver() {
        let effect = Effect::MapFrame {
            effect_id: EffectId(7),
            session: SessionKey {
                connection_generation: 1,
                session_id: "session".into(),
            },
            pause_epoch: 2,
            frame_index: 1,
            script: ScriptKey {
                session: SessionKey {
                    connection_generation: 1,
                    session_id: "session".into(),
                },
                script_id: "script".into(),
            },
            view_id: EffectId(6),
            position: Position {
                line: 232,
                column: 41,
            },
        };

        let completion = source_effect_completion(
            &effect,
            Err(SourceEffectError::UnmappedPosition {
                view_id: EffectId(6),
                position: Position {
                    line: 232,
                    column: 41,
                },
            }),
        );

        assert!(matches!(
            completion,
            Some(Input::EffectFailed {
                effect_id: EffectId(7),
                message,
            }) if message.contains("cannot map generated position")
        ));
    }

    #[test]
    fn replay_reports_the_transition_with_effect_divergence() {
        let initial = Arc::new(DebuggerState::default());
        let connected = reduce(&initial, Input::Connected);
        let attached_input = Input::SessionAttached {
            session_id: "session".into(),
            target_id: "target".into(),
            parent_session_id: None,
            waiting_for_debugger: false,
        };
        let attached = reduce(&connected.state, attached_input.clone());
        assert!(!attached.effects.is_empty());

        let recording = DebuggerRecording {
            transitions: vec![
                RecordedTransition {
                    input: Input::Connected,
                    effects: connected.effects,
                },
                RecordedTransition {
                    input: attached_input,
                    effects: Vec::new(),
                },
            ],
        };

        let error = recording
            .replay(initial)
            .expect_err("tampered effects must fail replay");
        assert!(matches!(
            error,
            DebuggerReplayError::EffectDivergence { index: 1, .. }
        ));
    }
}

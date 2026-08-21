use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cdp::CdpClient;
use crate::cdp_runtime::{
    CdpDebuggerSession, CdpRuntimeError, CdpRuntimeEvent, CdpRuntimeEventError,
};
use crate::debugger_engine::{DebuggerState, Effect, Input, SessionPhase, reduce};
use crate::source_effects::{SourceEffectError, SourceEffectInterpreter};

pub struct DebuggerDriver {
    state: Arc<DebuggerState>,
    session: CdpDebuggerSession,
    sources: SourceEffectInterpreter,
    recording: DebuggerRecording,
    console_messages: Vec<Vec<String>>,
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
            console_messages: Vec::new(),
        }
    }

    pub fn state(&self) -> &Arc<DebuggerState> {
        &self.state
    }

    pub fn client(&self) -> &CdpClient<hubrpc::connection::channel::Channel> {
        self.session.client()
    }

    pub fn recording(&self) -> &DebuggerRecording {
        &self.recording
    }

    pub fn console_messages(&self) -> &[Vec<String>] {
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

    pub fn project_generated_offset(
        &self,
        script: &crate::debugger_engine::ScriptKey,
        utf16_offset: u32,
    ) -> Option<(String, crate::source_view::Position, Arc<str>)> {
        self.sources
            .project_generated_offset(&self.state, script, utf16_offset)
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
                self.console_messages.remove(0);
            }
            self.console_messages.push(
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
            );
            return Ok(false);
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
            let completion = match self.sources.interpret(&effect)? {
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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cdp::CdpClient;
use crate::cdp_runtime::{
    CdpDebuggerSession, CdpRuntimeError, CdpRuntimeEvent, CdpRuntimeEventError,
    HeapSnapshotStreamProgress, RawCdpEvent, SourceMapCacheStats,
};
use crate::debugger_engine::{DebuggerState, Effect, Input, SessionPhase, reduce};
use crate::service_api::{
    ConsoleMessageSnapshot, LogCaptureSnapshot, LogCaptureStatus, LogpointCaptureSnapshot,
};

pub(crate) const LOGPOINT_BINDING_NAME: &str = "__dbgjs_logpoint_emit_v1";
const MAX_LOGPOINT_PAYLOAD: usize = 16 * 1024;
use crate::source_effects::{SourceEffectError, SourceEffectInterpreter};

#[derive(Default)]
struct ConsoleLog {
    messages: VecDeque<ConsoleMessageSnapshot>,
    evicted_count: u64,
    next_index: u64,
}

impl ConsoleLog {
    fn push_message(&mut self, values: Vec<String>, params: Option<serde_json::Value>) {
        const MAX_CONSOLE_MESSAGES: usize = 100;
        if self.messages.len() == MAX_CONSOLE_MESSAGES {
            self.messages.pop_front();
            self.evicted_count = self.evicted_count.saturating_add(1);
        }
        self.next_index = self.next_index.saturating_add(1);
        self.messages.push_back(ConsoleMessageSnapshot {
            index: self.next_index,
            values,
            params,
        });
    }

    fn push(&mut self, params: &crate::cdp::RuntimeConsoleApicalledParams) {
        self.push_message(
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
            serde_json::to_value(params).ok(),
        );
    }
}

pub struct DebuggerDriver {
    state: Arc<DebuggerState>,
    session: CdpDebuggerSession,
    sources: SourceEffectInterpreter,
    recording: DebuggerRecording,
    console_log: ConsoleLog,
    log_capture: LogCaptureSnapshot,
    logpoint_binding_ready: bool,
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
            console_log: ConsoleLog::default(),
            log_capture: LogCaptureSnapshot::default(),
            logpoint_binding_ready: false,
        }
    }

    pub fn state(&self) -> &Arc<DebuggerState> {
        &self.state
    }

    pub fn client(&self) -> &CdpClient<linkrpc::connection::channel::Channel> {
        self.session.client()
    }

    pub async fn raw_cdp_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, linkrpc::prelude::JsonRpcError> {
        self.session.raw_request(method, params).await
    }

    pub fn heap_snapshot_progress(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<HeapSnapshotStreamProgress>> {
        self.session.heap_snapshot_progress()
    }

    /// Subscribes to every raw CDP notification for this target's session, regardless of
    /// whether the reducer understands it. Used by relay dispatchers to mirror events verbatim.
    pub fn raw_events_sender(&self) -> tokio::sync::broadcast::Sender<RawCdpEvent> {
        self.session.raw_events_sender()
    }

    pub fn raw_event_history(&self) -> Arc<std::sync::Mutex<Vec<RawCdpEvent>>> {
        self.session.raw_event_history()
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

    pub fn console_messages(&self) -> &VecDeque<ConsoleMessageSnapshot> {
        &self.console_log.messages
    }

    pub fn log_capture(&self) -> LogCaptureSnapshot {
        LogCaptureSnapshot {
            evicted_count: Some(self.console_log.evicted_count),
            ..self.log_capture.clone()
        }
    }

    pub async fn ensure_logpoint_binding(
        &mut self,
    ) -> Result<(), linkrpc::prelude::JsonRpcError> {
        if !self.logpoint_binding_ready {
            self.session
                .raw_request(
                    "Runtime.addBinding",
                    serde_json::json!({"name": LOGPOINT_BINDING_NAME}),
                )
                .await?;
            self.logpoint_binding_ready = true;
            self.log_capture
                .collected_events
                .push("Runtime.bindingCalled".to_owned());
            self.log_capture.dropped_count = Some(0);
        }
        Ok(())
    }

    pub fn register_logpoints(&mut self, ids: &[String]) {
        for id in ids {
            if !self.log_capture.logpoints.iter().any(|item| item.id == *id) {
                self.log_capture.logpoints.push(LogpointCaptureSnapshot {
                    id: id.clone(),
                    ..Default::default()
                });
            }
        }
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

    pub fn project_generated_position(
        &self,
        script: &crate::debugger_engine::ScriptKey,
        position: crate::source_view::Position,
    ) -> Option<(String, crate::source_view::Position, Arc<str>)> {
        self.sources
            .project_generated_position(&self.state, script, position)
    }

    pub fn source_effects(&self) -> &SourceEffectInterpreter {
        &self.sources
    }

    pub fn resolve_generated_position(
        &self,
        script: &crate::debugger_engine::ScriptKey,
        position: crate::source_view::Position,
    ) -> Option<crate::source_location::ResolvedSourcePosition> {
        self.sources
            .resolve_generated_position(&self.state, script, position)
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

    pub async fn acquire_script_source(
        &mut self,
        script: crate::debugger_engine::ScriptKey,
        control: Option<&crate::source_search::SearchControl>,
    ) -> Result<(), DebuggerDriverError> {
        let effects = self.reduce_recorded(Input::RequestScriptSource { script });
        self.drain_effects_with_control(effects, control).await
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
            self.console_log.push(params);
            self.apply(Input::ConsoleMessageObserved).await?;
            return Ok(true);
        }
        if let CdpRuntimeEvent::Other { session, method, params } = &event
            && method == "Runtime.bindingCalled"
            && params.get("name").and_then(serde_json::Value::as_str) == Some(LOGPOINT_BINDING_NAME)
        {
            self.record_logpoint_event(session, params);
            self.apply(Input::ConsoleMessageObserved).await?;
            return Ok(true);
        }
        let Some(input) = event.into_input(pause_epoch)? else {
            return Ok(false);
        };
        self.apply(input).await?;
        Ok(true)
    }

    fn record_logpoint_event(
        &mut self,
        session: &crate::debugger_engine::SessionKey,
        params: &serde_json::Value,
    ) {
        let Some(payload) = params.get("payload").and_then(serde_json::Value::as_str) else {
            self.log_capture.dropped_count =
                Some(self.log_capture.dropped_count.unwrap_or(0).saturating_add(1));
            return;
        };
        if payload.len() > MAX_LOGPOINT_PAYLOAD {
            self.log_capture.dropped_count =
                Some(self.log_capture.dropped_count.unwrap_or(0).saturating_add(1));
            return;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
            self.log_capture.dropped_count =
                Some(self.log_capture.dropped_count.unwrap_or(0).saturating_add(1));
            return;
        };
        let (Some(id), Some(outcome)) = (
            event.get("id").and_then(serde_json::Value::as_str),
            event.get("outcome").and_then(serde_json::Value::as_str),
        ) else {
            self.log_capture.dropped_count =
                Some(self.log_capture.dropped_count.unwrap_or(0).saturating_add(1));
            return;
        };
        let Some(stats) = self.log_capture.logpoints.iter_mut().find(|item| item.id == id) else {
            self.log_capture.dropped_count =
                Some(self.log_capture.dropped_count.unwrap_or(0).saturating_add(1));
            return;
        };
        stats.hits = stats.hits.saturating_add(1);
        match outcome {
            "success" => stats.successful_evaluations = stats.successful_evaluations.saturating_add(1),
            "evaluationError" => stats.failed_evaluations = stats.failed_evaluations.saturating_add(1),
            "serializationError" => stats.failed_serializations = stats.failed_serializations.saturating_add(1),
            _ => {
                stats.dropped_events = stats.dropped_events.saturating_add(1);
                self.log_capture.dropped_count =
                    Some(self.log_capture.dropped_count.unwrap_or(0).saturating_add(1));
                return;
            }
        };
        stats.recorded_events = stats.recorded_events.saturating_add(1);
        let breakpoint = self
            .state
            .breakpoints
            .iter()
            .find(|(key, _)| key.breakpoint_id == format!("log:{id}"))
            .map(|(_, breakpoint)| breakpoint);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
        self.console_log.push_message(
            vec![id.to_owned(), event.get("value").or_else(|| event.get("error"))
                .map_or_else(|| "undefined".to_owned(), |value| value.as_str()
                    .map_or_else(|| value.to_string(), str::to_owned))],
            Some(serde_json::json!({
                "kind": "logpoint",
                "outcome": outcome,
                "id": id,
                "targetId": self.state.sessions.get(session).map(|session| &session.target_id),
                "sourceUrl": breakpoint.map(|b| &b.source_url),
                "line": breakpoint.map(|b| b.position.line + 1),
                "column": breakpoint.map(|b| b.position.column + 1),
                "timestampUnixMs": timestamp,
                "exception": event.get("error"),
                "executionContextId": params.get("executionContextId"),
            })),
        );
    }

    async fn drain_effects(&mut self, effects: Vec<Effect>) -> Result<(), DebuggerDriverError> {
        self.drain_effects_with_control(effects, None).await
    }

    async fn drain_effects_with_control(
        &mut self,
        effects: Vec<Effect>,
        control: Option<&crate::source_search::SearchControl>,
    ) -> Result<(), DebuggerDriverError> {
        let mut queue = VecDeque::from(effects);
        while let Some(effect) = queue.pop_front() {
            if let Effect::ConfigureSession { session, .. } = &effect {
                self.log_capture = begin_log_capture(&session.session_id);
            }
            let completion =
                match source_effect_completion(&effect, self.sources.interpret(&effect)) {
                    Some(input) => Some(input),
                    None if matches!(effect, Effect::FetchScriptSource { .. }) => {
                        let result = match control {
                            Some(control) => tokio::select! {
                                biased;
                                error = control.interrupted() => Ok(Some(Input::EffectFailed {
                                    effect_id: effect.effect_id(),
                                    message: error.to_string(),
                                })),
                                result = self.session.execute(&effect) => result,
                            },
                            None => self.session.execute(&effect).await,
                        };
                        Some(match result {
                            Ok(Some(input)) => input,
                            Ok(None) => return Err(DebuggerDriverError::UnhandledEffect(effect)),
                            Err(error) => Input::EffectFailed {
                                effect_id: effect.effect_id(),
                                message: error.to_string(),
                            },
                        })
                    }
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

fn begin_log_capture(session_id: &str) -> LogCaptureSnapshot {
    static NEXT_CAPTURE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let started = std::time::SystemTime::now();
    LogCaptureSnapshot {
        status: LogCaptureStatus::Active,
        capture_id: Some(format!(
            "{started:?}-{}-{}",
            std::process::id(),
            NEXT_CAPTURE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )),
        session_id: Some(session_id.to_owned()),
        started_at_unix_ms: started
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok()),
        collected_events: vec!["Runtime.consoleAPICalled".to_owned()],
        evicted_count: Some(0),
        dropped_count: None,
        logpoints: Vec::new(),
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
    fn log_capture_start_is_observed_and_reattachment_has_a_new_identity() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let first = begin_log_capture("");
        let second = begin_log_capture("");
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert_eq!(first.status, LogCaptureStatus::Active);
        assert_eq!(first.session_id.as_deref(), Some(""));
        assert_eq!(first.collected_events, ["Runtime.consoleAPICalled"]);
        assert_eq!(first.evicted_count, Some(0));
        assert_eq!(first.dropped_count, None);
        assert!((before..=after).contains(&u128::from(first.started_at_unix_ms.unwrap())));
        assert_ne!(first.capture_id, second.capture_id);
    }

    #[test]
    fn log_ring_counts_eviction_and_preserves_console_provenance() {
        let mut log = ConsoleLog::default();
        assert!(log.messages.is_empty());
        assert_eq!(log.evicted_count, 0);
        let params = serde_json::json!({
            "type": "error",
            "args": [{"type": "string", "value": "error from frame"}],
            "executionContextId": 42,
            "timestamp": 1234.5,
            "stackTrace": {"callFrames": [{
                "functionName": "render",
                "scriptId": "17",
                "url": "https://example.test/frame.js",
                "lineNumber": 3,
                "columnNumber": 7
            }]}
        });
        let event = serde_json::from_value(params.clone()).unwrap();
        for _ in 0..103 {
            log.push(&event);
        }
        assert_eq!(log.messages.len(), 100);
        assert_eq!(log.evicted_count, 3);
        let first = log.messages.front().unwrap();
        assert_eq!(first.index, 4);
        assert_eq!(first.values, ["error from frame"]);
        assert_eq!(first.params, Some(params));
        assert_eq!(log.messages.back().unwrap().index, 103);
        let fresh = ConsoleLog::default();
        assert!(fresh.messages.is_empty());
        assert_eq!(fresh.evicted_count, 0);
        assert_eq!(fresh.next_index, 0);
    }

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

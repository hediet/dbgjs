use super::*;
use linkrpc::prelude::RpcCallError;

pub(super) fn target_selector_rpc_error(
    error: crate::debugger::target_selector::TargetSelectorError,
) -> JsonRpcError {
    match error {
        crate::debugger::target_selector::TargetSelectorError::NotFound { selector } => {
            TargetError::TargetSelectorNotFound { selector }
        }
        crate::debugger::target_selector::TargetSelectorError::StaleGeneration {
            selector,
            connection_id,
            generation,
            current_generation,
        } => TargetError::StaleConnection {
            selector,
            connection_id,
            generation,
            current_generation,
        },
    }
    .into()
}

fn unexpected_target_error(error: TargetDebuggerError) -> RpcCallError {
    match error {
        TargetDebuggerError::Driver(crate::debugger::debugger_driver::DebuggerDriverError::Runtime(
            crate::debugger::cdp_runtime::CdpRuntimeError::Protocol {
                code,
                message,
                data,
            },
        )) => RpcCallError::Remote(JsonRpcError {
            code,
            message,
            data,
        }),
        TargetDebuggerError::Driver(crate::debugger::debugger_driver::DebuggerDriverError::Runtime(
            crate::debugger::cdp_runtime::CdpRuntimeError::Call(error),
        )) => error,
        error => RpcCallError::Local(target_debugger_rpc_error(error)),
    }
}

impl From<TargetDebuggerError> for TargetError {
    fn from(error: TargetDebuggerError) -> Self {
        match error {
            TargetDebuggerError::SessionMissing | TargetDebuggerError::Stopped => {
                Self::SessionUnavailable
            }
            TargetDebuggerError::StalePause(pause_epoch) => Self::StalePause { pause_epoch },
            TargetDebuggerError::FrameNotFound(frame_index) => Self::FrameNotFound { frame_index },
            TargetDebuggerError::ScopeNotFound(scope_index) => Self::ScopeNotFound { scope_index },
            TargetDebuggerError::InvalidValueInspection(reason) => {
                Self::InvalidInspection { reason }
            }
            TargetDebuggerError::InvalidBreakpointPosition => Self::InvalidBreakpointPosition,
            TargetDebuggerError::WaitTimedOut => Self::WaitTimedOut,
            TargetDebuggerError::InvalidTimeout => Self::InvalidTimeout,
            error => Self::Generic(unexpected_target_error(error)),
        }
    }
}

impl From<TargetDebuggerError> for CoverageError {
    fn from(error: TargetDebuggerError) -> Self {
        match error {
            TargetDebuggerError::SessionMissing | TargetDebuggerError::Stopped => {
                Self::SessionUnavailable
            }
            TargetDebuggerError::CoverageAlreadyActive => Self::AlreadyActive,
            TargetDebuggerError::CoverageNotActive => Self::NotActive,
            TargetDebuggerError::CoverageCaptureNotFound(capture_id) => {
                Self::CaptureNotFound { capture_id }
            }
            TargetDebuggerError::CoverageCaptureAlreadyExists(capture_id) => {
                Self::CaptureAlreadyExists { capture_id }
            }
            error => Self::Generic(unexpected_target_error(error)),
        }
    }
}

impl From<TargetDebuggerError> for CpuProfilerError {
    fn from(error: TargetDebuggerError) -> Self {
        match error {
            TargetDebuggerError::SessionMissing | TargetDebuggerError::Stopped => {
                Self::SessionUnavailable
            }
            TargetDebuggerError::CpuProfileAlreadyActive => Self::AlreadyActive,
            TargetDebuggerError::CpuProfileNotActive => Self::NotActive,
            TargetDebuggerError::CpuProfileCaptureNotFound(capture_id) => {
                Self::CaptureNotFound { capture_id }
            }
            TargetDebuggerError::CpuProfileCaptureAlreadyExists(capture_id) => {
                Self::CaptureAlreadyExists { capture_id }
            }
            TargetDebuggerError::InvalidCpuProfileSamplingInterval => Self::InvalidSamplingInterval,
            error => Self::Generic(unexpected_target_error(error)),
        }
    }
}

impl From<TargetDebuggerError> for HeapProfilerError {
    fn from(error: TargetDebuggerError) -> Self {
        match error {
            TargetDebuggerError::SessionMissing | TargetDebuggerError::Stopped => {
                Self::SessionUnavailable
            }
            TargetDebuggerError::HeapCaptureNotFound(capture_id) => {
                Self::CaptureNotFound { capture_id }
            }
            TargetDebuggerError::HeapNodeNotFound(object_id) => Self::NodeNotFound { object_id },
            TargetDebuggerError::InvalidHeapFilter(reason) => Self::InvalidFilter { reason },
            TargetDebuggerError::InvalidHeapSelector(reason) => Self::InvalidSelector { reason },
            TargetDebuggerError::InvalidHeapReference(reason) => Self::InvalidReference { reason },
            TargetDebuggerError::IncompatibleHeapCaptures { older, newer } => {
                Self::IncompatibleCaptures { older, newer }
            }
            error => Self::Generic(unexpected_target_error(error)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debugger_failures_map_to_structured_application_errors() {
        assert!(matches!(
            TargetError::from(TargetDebuggerError::StalePause(12)),
            TargetError::StalePause { pause_epoch: 12 }
        ));
        assert!(matches!(
            TargetError::from(TargetDebuggerError::FrameNotFound(3)),
            TargetError::FrameNotFound { frame_index: 3 }
        ));
        assert!(matches!(
            CoverageError::from(TargetDebuggerError::CoverageAlreadyActive),
            CoverageError::AlreadyActive
        ));
        assert!(matches!(
            CpuProfilerError::from(TargetDebuggerError::CpuProfileNotActive),
            CpuProfilerError::NotActive
        ));
        assert!(matches!(
            HeapProfilerError::from(TargetDebuggerError::HeapNodeNotFound("node-7".into())),
            HeapProfilerError::NodeNotFound { object_id } if object_id == "node-7"
        ));
    }

    #[test]
    fn upstream_cdp_errors_keep_code_message_and_data() {
        let wire = JsonRpcError {
            code: -32000,
            message: "upstream failure".into(),
            data: Some(serde_json::json!({ "detail": 7 })),
        };
        let error = TargetError::from(TargetDebuggerError::Driver(
            crate::debugger::debugger_driver::DebuggerDriverError::Runtime(
                crate::debugger::cdp_runtime::CdpRuntimeError::Protocol {
                    code: wire.code,
                    message: wire.message.clone(),
                    data: wire.data.clone(),
                },
            ),
        ));
        assert!(
            matches!(error, TargetError::Generic(RpcCallError::Remote(ref original)) if original == &wire)
        );
        assert_eq!(JsonRpcError::from(error), wire);
    }
}

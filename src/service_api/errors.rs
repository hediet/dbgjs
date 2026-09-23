use linkrpc::prelude::{ApplicationError, JsonRpcError, RpcCallError};

/// Recoverable failures while controlling or inspecting an attached debugger.
#[derive(Debug, linkrpc::ApplicationError)]
#[rpc_error(display)]
pub enum TargetError {
    #[rpc_error(
        message = "target selector '{selector}' did not match a discovered target; target discovery may be incomplete"
    )]
    TargetSelectorNotFound { selector: String },
    #[rpc_error(
        message = "target selector '{selector}' has stale connection generation {generation}; connection '{connection_id}' is at generation {current_generation}"
    )]
    StaleConnection {
        selector: String,
        connection_id: String,
        generation: u64,
        current_generation: u64,
    },
    #[rpc_error(message = "attached target '{target_id}' does not exist")]
    TargetNotFound { target_id: String },
    #[rpc_error(message = "the debugger session is no longer available")]
    SessionUnavailable,
    #[rpc_error(message = "pause epoch {pause_epoch} is stale")]
    StalePause { pause_epoch: u64 },
    #[rpc_error(message = "frame {frame_index} does not exist in the current pause")]
    FrameNotFound { frame_index: u32 },
    #[rpc_error(message = "scope {scope_index} does not exist in the selected frame")]
    ScopeNotFound { scope_index: u32 },
    #[rpc_error(message = "invalid value inspection: {reason}")]
    InvalidInspection { reason: String },
    #[rpc_error(message = "breakpoint line and column must be one-based")]
    InvalidBreakpointPosition,
    #[rpc_error(message = "target wait timed out")]
    WaitTimedOut,
    #[rpc_error(message = "target wait timeout must be between 1ms and 5 minutes")]
    InvalidTimeout,
    #[rpc_error(generic)]
    Generic(RpcCallError),
}

/// Recoverable failures of precise coverage recording.
#[derive(Debug, linkrpc::ApplicationError)]
#[rpc_error(display)]
pub enum CoverageError {
    #[rpc_error(
        message = "target selector '{selector}' did not match a discovered target; target discovery may be incomplete"
    )]
    TargetSelectorNotFound { selector: String },
    #[rpc_error(
        message = "target selector '{selector}' has stale connection generation {generation}; connection '{connection_id}' is at generation {current_generation}"
    )]
    StaleConnection {
        selector: String,
        connection_id: String,
        generation: u64,
        current_generation: u64,
    },
    #[rpc_error(message = "attached target '{target_id}' does not exist")]
    TargetNotFound { target_id: String },
    #[rpc_error(message = "the debugger session is no longer available")]
    SessionUnavailable,
    #[rpc_error(message = "coverage recording is already active")]
    AlreadyActive,
    #[rpc_error(message = "coverage recording is not active")]
    NotActive,
    #[rpc_error(message = "coverage capture '{capture_id}' does not exist in the active recording")]
    CaptureNotFound { capture_id: String },
    #[rpc_error(message = "coverage capture '{capture_id}' already exists in the active recording")]
    CaptureAlreadyExists { capture_id: String },
    #[rpc_error(message = "capture '{capture_id}' already exists in context '{context_id}'")]
    NameConflict {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(
        message = "capture '{capture_id}' is already being stored in context '{context_id}'"
    )]
    CaptureBusy {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(generic)]
    Generic(RpcCallError),
}

/// Recoverable failures of CPU profile recording.
#[derive(Debug, linkrpc::ApplicationError)]
#[rpc_error(display)]
pub enum CpuProfilerError {
    #[rpc_error(
        message = "target selector '{selector}' did not match a discovered target; target discovery may be incomplete"
    )]
    TargetSelectorNotFound { selector: String },
    #[rpc_error(
        message = "target selector '{selector}' has stale connection generation {generation}; connection '{connection_id}' is at generation {current_generation}"
    )]
    StaleConnection {
        selector: String,
        connection_id: String,
        generation: u64,
        current_generation: u64,
    },
    #[rpc_error(message = "attached target '{target_id}' does not exist")]
    TargetNotFound { target_id: String },
    #[rpc_error(message = "the debugger session is no longer available")]
    SessionUnavailable,
    #[rpc_error(message = "CPU profile recording is already active")]
    AlreadyActive,
    #[rpc_error(message = "CPU profile recording is not active")]
    NotActive,
    #[rpc_error(message = "CPU profile capture '{capture_id}' does not exist")]
    CaptureNotFound { capture_id: String },
    #[rpc_error(message = "CPU profile capture '{capture_id}' already exists")]
    CaptureAlreadyExists { capture_id: String },
    #[rpc_error(message = "CPU profile sampling interval must be between 1us and 2147483647us")]
    InvalidSamplingInterval,
    #[rpc_error(message = "capture '{capture_id}' already exists in context '{context_id}'")]
    NameConflict {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(
        message = "capture '{capture_id}' is already being stored in context '{context_id}'"
    )]
    CaptureBusy {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(generic)]
    Generic(RpcCallError),
}

/// Recoverable failures while capturing or querying a heap.
#[derive(Debug, linkrpc::ApplicationError)]
#[rpc_error(display)]
pub enum HeapProfilerError {
    #[rpc_error(
        message = "target selector '{selector}' did not match a discovered target; target discovery may be incomplete"
    )]
    TargetSelectorNotFound { selector: String },
    #[rpc_error(
        message = "target selector '{selector}' has stale connection generation {generation}; connection '{connection_id}' is at generation {current_generation}"
    )]
    StaleConnection {
        selector: String,
        connection_id: String,
        generation: u64,
        current_generation: u64,
    },
    #[rpc_error(message = "attached target '{target_id}' does not exist")]
    TargetNotFound { target_id: String },
    #[rpc_error(message = "the debugger session is no longer available")]
    SessionUnavailable,
    #[rpc_error(message = "heap capture '{capture_id}' does not exist")]
    CaptureNotFound { capture_id: String },
    #[rpc_error(message = "heap object id '{object_id}' does not exist in the capture")]
    NodeNotFound { object_id: String },
    #[rpc_error(message = "invalid heap class filter: {reason}")]
    InvalidFilter { reason: String },
    #[rpc_error(message = "invalid heap selector: {reason}")]
    InvalidSelector { reason: String },
    #[rpc_error(message = "invalid heap reference: {reason}")]
    InvalidReference { reason: String },
    #[rpc_error(message = "heap captures '{older}' and '{newer}' are incompatible")]
    IncompatibleCaptures { older: String, newer: String },
    #[rpc_error(message = "capture '{capture_id}' already exists in context '{context_id}'")]
    NameConflict {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(
        message = "capture '{capture_id}' is already being stored in context '{context_id}'"
    )]
    CaptureBusy {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(generic)]
    Generic(RpcCallError),
}

/// Recoverable failures of the persistent capture catalog.
#[derive(Debug, linkrpc::ApplicationError)]
#[rpc_error(display)]
pub enum CaptureError {
    #[rpc_error(message = "context '{context_id}' does not exist")]
    ContextNotFound { context_id: String },
    #[rpc_error(
        message = "capture '{selector}' does not exist in context '{context_id}' for the requested kind and target scope"
    )]
    CaptureNotFound {
        context_id: String,
        selector: String,
    },
    #[rpc_error(message = "capture '{capture_id}' already exists in context '{context_id}'")]
    NameConflict {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(
        message = "capture '{capture_id}' is already being stored in context '{context_id}'"
    )]
    CaptureBusy {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(
        message = "capture '{capture_id}' is currently being deleted from context '{context_id}'"
    )]
    CaptureDeleting {
        context_id: String,
        capture_id: String,
    },
    #[rpc_error(generic)]
    Generic(RpcCallError),
}

// Shared service helpers also serve legacy, generic-only capabilities. Decode
// their structured wire errors here, without inferring cases from message text.
macro_rules! service_error_conversions {
    ($($error:ty),+ $(,)?) => {$(
        impl From<JsonRpcError> for $error {
            fn from(error: JsonRpcError) -> Self {
                Self::try_from_rpc_error(error)
                    .unwrap_or_else(|error| Self::Generic(RpcCallError::Local(error)))
            }
        }

        impl From<$error> for JsonRpcError {
            fn from(error: $error) -> Self {
                error.into_rpc_error()
            }
        }
    )+};
}

service_error_conversions!(
    TargetError,
    CoverageError,
    CpuProfilerError,
    HeapProfilerError,
    CaptureError,
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn application_messages_include_data_and_default_code() {
        let error = TargetError::FrameNotFound { frame_index: 3 };
        let message = error.to_string();
        let wire = error.into_rpc_error();
        assert_eq!(message, "frame 3 does not exist in the current pause");
        assert_eq!(wire.code, 1);
        assert_eq!(wire.message, message);
        assert_eq!(
            wire.data,
            Some(json!({
                "type": "FrameNotFound", "data": { "frame_index": 3 }
            }))
        );
        assert!(matches!(
            TargetError::try_from_rpc_error(wire).unwrap(),
            TargetError::FrameNotFound { frame_index: 3 }
        ));
    }

    #[test]
    fn generic_fallbacks_are_not_part_of_the_wire_contract() {
        for errors in [
            TargetError::error_schemas(),
            CoverageError::error_schemas(),
            CpuProfilerError::error_schemas(),
            HeapProfilerError::error_schemas(),
            CaptureError::error_schemas(),
        ] {
            assert!(!errors.is_empty());
            assert!(errors.iter().all(|error| {
                error.code == 1
                    && error
                        .r#type
                        .as_deref()
                        .is_some_and(|name| name != "Generic")
            }));
        }
    }

    #[test]
    fn shared_service_errors_decode_into_each_declaring_capability() {
        let error: JsonRpcError = CaptureError::NameConflict {
            context_id: "ctx".into(),
            capture_id: "sample".into(),
        }
        .into();
        assert!(matches!(
            CoverageError::from(error.clone()),
            CoverageError::NameConflict { context_id, capture_id }
                if context_id == "ctx" && capture_id == "sample"
        ));
        assert!(matches!(
            CpuProfilerError::from(error.clone()),
            CpuProfilerError::NameConflict { context_id, capture_id }
                if context_id == "ctx" && capture_id == "sample"
        ));
        assert!(matches!(
            HeapProfilerError::from(error),
            HeapProfilerError::NameConflict { context_id, capture_id }
                if context_id == "ctx" && capture_id == "sample"
        ));
    }
}

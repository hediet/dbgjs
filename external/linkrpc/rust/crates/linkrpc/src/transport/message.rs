//! The `MessageTransport` trait — a bidirectional pump of messages. Higher layers
//! (`Channel`, connection, hub) are written against this seam; concrete transports
//! (in-memory pair, Unix sockets with NDJSON framing, …) implement it.
//!
//! The trait is **generic over the payload**, defaulting to the wire [`JsonRpcMessage`].
//! `Out` is what this endpoint sends, `In` what it receives — usually the same type, but
//! the split lets a transport carry a richer payload (e.g. the hub's `MuxEnvelope`, or an
//! out-of-band context wrapper that attaches provenance / headers without touching the wire
//! bytes). Per-send configuration will ride in a future `SendOpts`, never in the payload
//! type itself.

use async_trait::async_trait;

use crate::protocol::jsonrpc::JsonRpcMessage;

/// Errors a transport can surface while sending.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transport is closed")]
    Closed,
    #[error("transport error: {0}")]
    Other(String),
}

/// A bidirectional message transport.
///
/// `recv` is only ever driven by a single reader (the `Channel` read loop), so taking
/// `&self` with interior mutability is sufficient; `send` may be called concurrently.
#[async_trait]
pub trait MessageTransport<In = JsonRpcMessage, Out = JsonRpcMessage>: Send + Sync
where
    In: Send + 'static,
    Out: Send + 'static,
{
    /// Send one message. Errors if the transport is closed.
    async fn send(&self, msg: Out) -> Result<(), TransportError>;

    /// Receive the next message, or `None` when the transport has closed.
    async fn recv(&self) -> Option<In>;
}

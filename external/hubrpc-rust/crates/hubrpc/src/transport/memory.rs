//! An in-memory `MessageTransport` pair: two linked endpoints where each one's `send`
//! delivers to the other's `recv`. Used by tests and by the hub loopback/overlay wiring.
//!
//! Generic over the payload `T` (both directions carry the same type), defaulting to the
//! wire [`JsonRpcMessage`]. The hub mux uses richer payloads (e.g. `MuxEnvelope`).

use async_trait::async_trait;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::lock::Mutex;
use futures::StreamExt;

use crate::protocol::jsonrpc::JsonRpcMessage;
use crate::transport::message::{MessageTransport, TransportError};

/// One endpoint of an in-memory transport pair.
pub struct MemoryTransport<T = JsonRpcMessage> {
    tx: UnboundedSender<T>,
    rx: Mutex<UnboundedReceiver<T>>,
}

#[async_trait]
impl<T: Send + 'static> MessageTransport<T, T> for MemoryTransport<T> {
    async fn send(&self, msg: T) -> Result<(), TransportError> {
        self.tx
            .unbounded_send(msg)
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Option<T> {
        let mut rx = self.rx.lock().await;
        rx.next().await
    }
}

/// Create a linked pair of in-memory transports carrying payload `T`. A message sent on `a`
/// is received on `b` and vice versa.
pub fn transport_pair_of<T>() -> (MemoryTransport<T>, MemoryTransport<T>) {
    let (tx_a, rx_b) = unbounded();
    let (tx_b, rx_a) = unbounded();
    (
        MemoryTransport {
            tx: tx_a,
            rx: Mutex::new(rx_a),
        },
        MemoryTransport {
            tx: tx_b,
            rx: Mutex::new(rx_b),
        },
    )
}

/// Create a linked pair of in-memory transports carrying the wire [`JsonRpcMessage`].
pub fn transport_pair() -> (MemoryTransport, MemoryTransport) {
    transport_pair_of::<JsonRpcMessage>()
}

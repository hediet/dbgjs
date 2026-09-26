use std::sync::Arc;

use async_trait::async_trait;
use linkrpc::prelude::{MessageTransport, TransportError};
use tokio::sync::Mutex;

use crate::connection::transport::session_transport::CdpEnvelope;

#[async_trait]
pub trait ManagedCdpTransport: MessageTransport<CdpEnvelope, CdpEnvelope> + Send + Sync {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>>;

    async fn wait_closed(&self) -> String;

    async fn close(&self);
}

pub(crate) fn closed_transport_error(message: impl Into<String>) -> TransportError {
    TransportError::Other(message.into())
}

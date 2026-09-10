//! A transport-level multiplexer: fan many logical JSON-RPC transports over a
//! single base transport that carries an **envelope** tagging each message with
//! a channel id. Port of the canonical TS `transport/multiplexedTransport.ts`.
//!
//! # Semantics (identical to TS)
//!
//! - **Dynamic channels.** Channels can be added before or after the base pump
//!   starts ([`MultiplexedTransport::add_channel`]).
//! - **Permanently retired ids.** A channel id, once used, is retired forever:
//!   re-adding it fails even after the channel is disposed. This prevents a late
//!   envelope for an old iframe/session from being delivered to a replacement
//!   that happened to reuse the id.
//! - **Unknown / late drops.** An envelope whose channel id is not currently
//!   live is silently dropped (unknown id, or an id whose channel was disposed).
//! - **Base close tears down children.** When the base transport closes,
//!   [`MultiplexedTransport::run`] disposes the multiplexer, so every live
//!   channel's `recv` yields `None` and a layered [`Channel`] fails its pending
//!   calls instead of hanging after a disconnect.
//! - **Independent id spaces.** Each channel is a plain [`MessageTransport`] of
//!   [`JsonRpcMessage`]; layering a [`Channel`](crate::connection::channel::Channel)
//!   on each gives every channel its own request-id counter and pending map.
//!
//! # Generic envelope
//!
//! The base transport is generic over the envelope type `Env`, and the mapping
//! between `(channel_id, JsonRpcMessage)` and `Env` is supplied by a
//! [`MuxCodec`]. The default [`MuxEnvelopeCodec`] uses the canonical
//! `{ "$mux": "v1", "ch": <id>, "m": <message> }` wire shape.
//!
//! A different codec can carry a **flat** envelope — e.g. CDP's
//! `{ "sessionId": <id>, "id": …, "method": …, "params": … }`, where the
//! channel id rides *beside* the JSON-RPC fields rather than nested under a
//! wrapper (and crucially not inside `params`). See `flat_envelope_codec` in the
//! tests for a worked example.
//!
//! # Runtime-agnostic pump
//!
//! Like [`Channel`](crate::connection::channel::Channel), the multiplexer does
//! not spawn: the owner drives inbound routing by awaiting
//! [`MultiplexedTransport::run`] (loop) or [`MultiplexedTransport::route_next`]
//! (one message, for deterministic tests) on their executor.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::lock::Mutex as AsyncMutex;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::protocol::jsonrpc::JsonRpcMessage;
use crate::transport::message::{MessageTransport, TransportError};

/// Maps between a `(channel_id, JsonRpcMessage)` pair and a wire envelope `Env`.
///
/// `encode` is called on every outbound send; `decode` on every inbound
/// envelope. `decode` returns `None` to reject an envelope the multiplexer
/// cannot route (missing/invalid channel tag) — it is then dropped, exactly
/// like an unknown channel id.
pub trait MuxCodec<Env>: Send + Sync {
    /// Wrap an outbound message for `channel_id` into a wire envelope.
    fn encode(&self, channel_id: &str, message: JsonRpcMessage) -> Env;

    /// Extract the channel id and inner message from an inbound envelope, or
    /// `None` if the envelope is not addressable.
    fn decode(&self, envelope: Env) -> Option<(String, JsonRpcMessage)>;
}

/// The canonical multiplexer envelope: `{ "$mux": "v1", "ch": <id>, "m": <msg> }`.
///
/// Byte-compatible with the TS `MuxEnvelope`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MuxEnvelope {
    /// Envelope version tag. Always `"v1"`.
    #[serde(rename = "$mux")]
    pub mux: String,
    /// Channel id this message belongs to.
    pub ch: String,
    /// The wrapped JSON-RPC message.
    pub m: JsonRpcMessage,
}

impl MuxEnvelope {
    /// Build a `v1` envelope for `ch`.
    pub fn v1(ch: impl Into<String>, m: JsonRpcMessage) -> Self {
        MuxEnvelope {
            mux: "v1".to_string(),
            ch: ch.into(),
            m,
        }
    }
}

/// The default codec producing/consuming [`MuxEnvelope`] (`$mux`/`ch`/`m`).
#[derive(Debug, Clone, Default)]
pub struct MuxEnvelopeCodec;

impl MuxCodec<MuxEnvelope> for MuxEnvelopeCodec {
    fn encode(&self, channel_id: &str, message: JsonRpcMessage) -> MuxEnvelope {
        MuxEnvelope::v1(channel_id, message)
    }

    fn decode(&self, envelope: MuxEnvelope) -> Option<(String, JsonRpcMessage)> {
        // Route purely by channel id. The concrete transport owns framing, so we
        // don't re-validate `$mux` here — anything without a known id is dropped
        // by the router anyway.
        Some((envelope.ch, envelope.m))
    }
}

/// Errors from multiplexer channel management.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MuxError {
    /// Channel ids must be non-empty.
    #[error("multiplexed transport channel ids must not be empty")]
    EmptyId,
    /// The id has already been used (and is permanently retired).
    #[error("multiplexed transport channel id \"{0}\" has already been used")]
    AlreadyUsed(String),
    /// The multiplexer has been disposed; no more channels can be added.
    #[error("cannot add a channel to a disposed multiplexed transport")]
    Disposed,
}

/// The outcome of routing a single inbound envelope. Returned by
/// [`MultiplexedTransport::route_next`] so tests can assert deterministically.
#[derive(Debug, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Delivered to a live channel.
    Delivered {
        /// The channel id the message was delivered to.
        channel_id: String,
    },
    /// Dropped: unknown, retired, or undecodable channel.
    Dropped,
    /// The base transport has closed; the pump is finished.
    Closed,
}

struct Shared<Env, C> {
    base: Arc<dyn MessageTransport<Env, Env>>,
    codec: C,
    /// Live channels: id → inbound sender.
    live: Mutex<HashMap<String, UnboundedSender<JsonRpcMessage>>>,
    /// Every id ever handed out — retired permanently, never reusable.
    used: Mutex<HashSet<String>>,
    disposed: Mutex<bool>,
}

impl<Env, C> Shared<Env, C> {
    /// Remove a channel from the live set (its id stays retired in `used`).
    fn retire_live(&self, id: &str) {
        self.live.lock().unwrap().remove(id);
    }
}

impl<Env, C> Shared<Env, C>
where
    Env: Send + 'static,
    C: MuxCodec<Env>,
{
    fn create_channel(self: &Arc<Self>, id: &str) -> Result<Arc<MuxChannel<Env, C>>, MuxError> {
        if *self.disposed.lock().unwrap() {
            return Err(MuxError::Disposed);
        }
        if id.is_empty() {
            return Err(MuxError::EmptyId);
        }
        {
            let mut used = self.used.lock().unwrap();
            if used.contains(id) {
                return Err(MuxError::AlreadyUsed(id.to_string()));
            }
            used.insert(id.to_string());
        }
        let (tx, rx) = unbounded();
        self.live.lock().unwrap().insert(id.to_string(), tx);
        Ok(Arc::new(MuxChannel {
            shared: self.clone(),
            id: id.to_string(),
            rx: AsyncMutex::new(rx),
            closed: Mutex::new(false),
        }))
    }
}

/// Multiplexes logical JSON-RPC transports over one enveloped base transport.
///
/// `Env` is the base transport's payload; `C` maps it to/from channel-tagged
/// messages. Use [`MultiplexedTransport::canonical`] for the default
/// `$mux`/`ch`/`m` envelope, or [`MultiplexedTransport::with_codec`] for a
/// custom (e.g. flat) envelope.
pub struct MultiplexedTransport<Env, C = MuxEnvelopeCodec> {
    shared: Arc<Shared<Env, C>>,
}

impl<Env> MultiplexedTransport<Env, MuxEnvelopeCodec>
where
    Env: Send + 'static,
    MuxEnvelopeCodec: MuxCodec<Env>,
{
    /// Build a multiplexer over `base` using the canonical [`MuxEnvelope`] codec.
    pub fn canonical(base: Arc<dyn MessageTransport<Env, Env>>) -> Self {
        Self::with_codec(base, MuxEnvelopeCodec)
    }
}

impl<Env, C> MultiplexedTransport<Env, C>
where
    Env: Send + 'static,
    C: MuxCodec<Env> + 'static,
{
    /// Build a multiplexer over `base` with a custom envelope `codec`.
    pub fn with_codec(base: Arc<dyn MessageTransport<Env, Env>>, codec: C) -> Self {
        MultiplexedTransport {
            shared: Arc::new(Shared {
                base,
                codec,
                live: Mutex::new(HashMap::new()),
                used: Mutex::new(HashSet::new()),
                disposed: Mutex::new(false),
            }),
        }
    }

    /// Build a multiplexer and open `ids` up front. Fails (and opens nothing) on
    /// the first empty/duplicate id — mirroring the TS constructor's rejection
    /// of duplicate ids.
    pub fn with_channels(
        base: Arc<dyn MessageTransport<Env, Env>>,
        codec: C,
        ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<(Self, Vec<Arc<MuxChannel<Env, C>>>), MuxError> {
        let mux = Self::with_codec(base, codec);
        let mut channels = Vec::new();
        for id in ids {
            channels.push(mux.add_channel(id.into())?);
        }
        Ok((mux, channels))
    }

    /// Add a logical channel. Returns a [`MuxChannel`] usable as a
    /// [`MessageTransport`]. Errors on an empty id, a retired id, or after
    /// disposal.
    pub fn add_channel(&self, id: impl AsRef<str>) -> Result<Arc<MuxChannel<Env, C>>, MuxError> {
        self.shared.create_channel(id.as_ref())
    }

    /// Route exactly one inbound envelope. Deterministic building block for the
    /// pump and for tests.
    ///
    /// Returning [`RouteOutcome::Closed`] means the base transport has closed and
    /// no further messages will ever arrive. It has **no side effects** — a
    /// caller driving the pump manually MUST call [`dispose`](Self::dispose) on
    /// `Closed` so live channels tear down (their `recv` yields `None`) and any
    /// layered [`Channel`](crate::connection::channel::Channel) fails its pending
    /// calls instead of hanging. [`run`](Self::run) does this automatically.
    pub async fn route_next(&self) -> RouteOutcome {
        let Some(envelope) = self.shared.base.recv().await else {
            return RouteOutcome::Closed;
        };
        let Some((ch, message)) = self.shared.codec.decode(envelope) else {
            return RouteOutcome::Dropped;
        };
        // Take a sender clone under the lock, send outside it.
        let sender = self.shared.live.lock().unwrap().get(&ch).cloned();
        match sender {
            Some(tx) if tx.unbounded_send(message).is_ok() => {
                RouteOutcome::Delivered { channel_id: ch }
            }
            _ => RouteOutcome::Dropped,
        }
    }

    /// Drive inbound routing until the base transport closes. Spawn on your
    /// executor (like [`Channel::run`](crate::connection::channel::Channel::run)).
    ///
    /// When the base closes, this disposes the multiplexer before returning:
    /// every live channel is torn down so its `recv` yields `None`, which lets a
    /// layered [`Channel`](crate::connection::channel::Channel) end its read loop
    /// and fail outstanding calls. Without this, a browser/network disconnect
    /// would leave child channels open forever and pending calls hanging.
    pub async fn run(&self) {
        loop {
            if let RouteOutcome::Closed = self.route_next().await {
                self.dispose();
                break;
            }
        }
    }

    /// Dispose the multiplexer: retire every live channel and refuse new ones.
    /// Used ids stay retired. Idempotent.
    pub fn dispose(&self) {
        {
            let mut disposed = self.shared.disposed.lock().unwrap();
            if *disposed {
                return;
            }
            *disposed = true;
        }
        // Dropping the senders closes each channel's inbound stream (recv → None).
        self.shared.live.lock().unwrap().clear();
    }
}

/// One logical channel of a [`MultiplexedTransport`]. A plain
/// [`MessageTransport`] of [`JsonRpcMessage`]: `send` wraps into an envelope on
/// the base transport, `recv` pulls messages the router delivered.
pub struct MuxChannel<Env, C> {
    shared: Arc<Shared<Env, C>>,
    id: String,
    rx: AsyncMutex<UnboundedReceiver<JsonRpcMessage>>,
    closed: Mutex<bool>,
}

impl<Env, C> MuxChannel<Env, C> {
    /// This channel's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Close this channel: stop sending, drop buffered inbound, and remove it
    /// from the live set. Its id stays permanently retired. Idempotent.
    pub fn dispose(&self) {
        {
            let mut closed = self.closed.lock().unwrap();
            if *closed {
                return;
            }
            *closed = true;
        }
        self.shared.retire_live(&self.id);
        // Close our inbound stream so any in-flight `recv` returns None.
        self.rx.try_lock().map(|mut rx| rx.close());
    }
}

#[async_trait]
impl<Env, C> MessageTransport<JsonRpcMessage, JsonRpcMessage> for MuxChannel<Env, C>
where
    Env: Send + 'static,
    C: MuxCodec<Env> + Send + Sync + 'static,
{
    async fn send(&self, msg: JsonRpcMessage) -> Result<(), TransportError> {
        if *self.closed.lock().unwrap() {
            return Err(TransportError::Closed);
        }
        let envelope = self.shared.codec.encode(&self.id, msg);
        self.shared.base.send(envelope).await
    }

    async fn recv(&self) -> Option<JsonRpcMessage> {
        let mut rx = self.rx.lock().await;
        rx.next().await
    }
}

impl<Env, C> Drop for MuxChannel<Env, C> {
    fn drop(&mut self) {
        // Best-effort retire when the handle is dropped without explicit dispose.
        if !*self.closed.lock().unwrap() {
            self.shared.retire_live(&self.id);
        }
    }
}

/// Lets an `Arc<MuxChannel>` be used directly as a boxed
/// [`MessageTransport`] — e.g. `Channel::new(Box::new(mux.add_channel(id)?), …)`
/// — while the caller keeps its own `Arc` handle for `dispose`/inspection.
#[async_trait]
impl<Env, C> MessageTransport<JsonRpcMessage, JsonRpcMessage> for Arc<MuxChannel<Env, C>>
where
    Env: Send + 'static,
    C: MuxCodec<Env> + Send + Sync + 'static,
{
    async fn send(&self, msg: JsonRpcMessage) -> Result<(), TransportError> {
        (**self).send(msg).await
    }

    async fn recv(&self) -> Option<JsonRpcMessage> {
        (**self).recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::jsonrpc::{JsonRpcNotification, JsonRpcRequest, RequestId};
    use crate::transport::memory::transport_pair_of;
    use serde_json::json;

    fn note(method: &str) -> JsonRpcMessage {
        JsonRpcMessage::Notification(JsonRpcNotification {
            method: method.to_string(),
            params: None,
        })
    }

    // A base transport pair carrying MuxEnvelope. `peer` stands in for the
    // remote end of the wire; we push envelopes into it and they arrive on the
    // multiplexer's base.
    fn canonical_pair() -> (
        MultiplexedTransport<MuxEnvelope>,
        crate::transport::memory::MemoryTransport<MuxEnvelope>,
    ) {
        let (base, peer) = transport_pair_of::<MuxEnvelope>();
        let mux = MultiplexedTransport::canonical(Arc::new(base));
        (mux, peer)
    }

    #[tokio::test]
    async fn adds_and_routes_dynamic_channels_and_drops_after_dispose() {
        let (mux, peer) = canonical_pair();
        let fixed = mux.add_channel("fixed").unwrap();
        let dynamic = mux.add_channel("dynamic").unwrap();

        peer.send(MuxEnvelope::v1("dynamic", note("hello")))
            .await
            .unwrap();
        assert_eq!(
            mux.route_next().await,
            RouteOutcome::Delivered {
                channel_id: "dynamic".to_string()
            }
        );
        assert_eq!(dynamic.recv().await, Some(note("hello")));

        // Dispose the dynamic channel: its later envelopes drop, fixed still works.
        dynamic.dispose();
        peer.send(MuxEnvelope::v1("dynamic", note("late")))
            .await
            .unwrap();
        assert_eq!(mux.route_next().await, RouteOutcome::Dropped);

        peer.send(MuxEnvelope::v1("fixed", note("still-here")))
            .await
            .unwrap();
        assert_eq!(
            mux.route_next().await,
            RouteOutcome::Delivered {
                channel_id: "fixed".to_string()
            }
        );
        assert_eq!(fixed.recv().await, Some(note("still-here")));
        mux.dispose();
    }

    #[tokio::test]
    async fn rejects_duplicate_and_reused_channel_ids() {
        let (mux, _peer) = canonical_pair();
        let _fixed = mux.add_channel("fixed").unwrap();

        assert_eq!(
            mux.add_channel("fixed").map(|_| ()).unwrap_err(),
            MuxError::AlreadyUsed("fixed".to_string())
        );
        let dynamic = mux.add_channel("dynamic").unwrap();
        assert_eq!(
            mux.add_channel("dynamic").map(|_| ()).unwrap_err(),
            MuxError::AlreadyUsed("dynamic".to_string())
        );
        // Retired permanently: still rejected after dispose.
        dynamic.dispose();
        assert_eq!(
            mux.add_channel("dynamic").map(|_| ()).unwrap_err(),
            MuxError::AlreadyUsed("dynamic".to_string())
        );
    }

    #[tokio::test]
    async fn rejects_duplicate_constructor_ids() {
        let (base, _peer) = transport_pair_of::<MuxEnvelope>();
        let err =
            MultiplexedTransport::with_channels(Arc::new(base), MuxEnvelopeCodec, ["same", "same"])
                .map(|_| ())
                .unwrap_err();
        assert_eq!(err, MuxError::AlreadyUsed("same".to_string()));
    }

    #[tokio::test]
    async fn rejects_empty_id() {
        let (mux, _peer) = canonical_pair();
        assert_eq!(
            mux.add_channel("").map(|_| ()).unwrap_err(),
            MuxError::EmptyId
        );
    }

    #[tokio::test]
    async fn drops_unknown_channel_envelopes() {
        let (mux, peer) = canonical_pair();
        let _fixed = mux.add_channel("fixed").unwrap();
        peer.send(MuxEnvelope::v1("unknown", note("x")))
            .await
            .unwrap();
        assert_eq!(mux.route_next().await, RouteOutcome::Dropped);
        mux.dispose();
    }

    #[tokio::test]
    async fn rejects_channels_after_disposal() {
        let (mux, _peer) = canonical_pair();
        mux.dispose();
        assert_eq!(
            mux.add_channel("dynamic").map(|_| ()).unwrap_err(),
            MuxError::Disposed
        );
    }

    #[tokio::test]
    async fn send_wraps_into_envelope_on_base() {
        let (mux, peer) = canonical_pair();
        let ch = mux.add_channel("c1").unwrap();
        let req = JsonRpcMessage::Request(JsonRpcRequest {
            id: RequestId::Number(7),
            method: "ping".to_string(),
            params: None,
        });
        ch.send(req.clone()).await.unwrap();
        // The envelope arrives on the peer end tagged with the channel id.
        assert_eq!(peer.recv().await, Some(MuxEnvelope::v1("c1", req)));
        mux.dispose();
    }

    #[tokio::test]
    async fn base_close_disposes_live_channels_so_child_recv_ends() {
        // Regression: after the base transport closes (disconnect), a live
        // child's pending `recv` must observe end-of-stream (None), not hang.
        let (mux, peer) = canonical_pair();
        let mux = Arc::new(mux);
        let ch = mux.add_channel("a").unwrap();

        let pump = {
            let m = mux.clone();
            tokio::spawn(async move { m.run().await })
        };

        // Close the base by dropping the remote end of the wire.
        drop(peer);

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), ch.recv())
            .await
            .expect("child recv must not hang after base close");
        assert_eq!(got, None);

        // The pump loop exits once the base is closed, and the mux is disposed.
        tokio::time::timeout(std::time::Duration::from_secs(5), pump)
            .await
            .expect("pump must terminate")
            .unwrap();
        assert_eq!(
            mux.add_channel("b").map(|_| ()).unwrap_err(),
            MuxError::Disposed
        );
    }

    // ── flat-envelope codec (CDP-style sessionId beside the JSON-RPC fields) ──
    //
    // Proves the mux is agnostic to envelope shape: the channel id can ride at
    // the top level next to `id`/`method`/`params`, never inside `params`.
    struct FlatSessionCodec;

    impl MuxCodec<serde_json::Value> for FlatSessionCodec {
        fn encode(&self, channel_id: &str, message: JsonRpcMessage) -> serde_json::Value {
            let mut obj = serde_json::to_value(&message).unwrap();
            obj.as_object_mut()
                .unwrap()
                .insert("sessionId".to_string(), json!(channel_id));
            obj
        }

        fn decode(&self, mut envelope: serde_json::Value) -> Option<(String, JsonRpcMessage)> {
            let session = envelope
                .as_object_mut()?
                .remove("sessionId")?
                .as_str()?
                .to_string();
            let message: JsonRpcMessage = serde_json::from_value(envelope).ok()?;
            Some((session, message))
        }
    }

    #[tokio::test]
    async fn flat_envelope_codec_routes_by_top_level_session_id() {
        let (base, peer) = transport_pair_of::<serde_json::Value>();
        let mux = MultiplexedTransport::with_codec(Arc::new(base), FlatSessionCodec);
        let a = mux.add_channel("sessA").unwrap();

        // Inbound: a flat CDP-style message with sessionId at top level.
        peer.send(json!({
            "jsonrpc": "2.0",
            "method": "Target.attached",
            "sessionId": "sessA"
        }))
        .await
        .unwrap();
        assert_eq!(
            mux.route_next().await,
            RouteOutcome::Delivered {
                channel_id: "sessA".to_string()
            }
        );
        assert_eq!(a.recv().await, Some(note("Target.attached")));

        // Unknown session drops.
        peer.send(json!({ "jsonrpc": "2.0", "method": "x", "sessionId": "ghost" }))
            .await
            .unwrap();
        assert_eq!(mux.route_next().await, RouteOutcome::Dropped);

        // Outbound: sessionId is injected beside the JSON-RPC fields, not nested.
        a.send(note("Runtime.enable")).await.unwrap();
        let sent = peer.recv().await.unwrap();
        assert_eq!(sent["sessionId"], json!("sessA"));
        assert_eq!(sent["method"], json!("Runtime.enable"));
        assert!(sent.get("params").is_none());
        mux.dispose();
    }
}

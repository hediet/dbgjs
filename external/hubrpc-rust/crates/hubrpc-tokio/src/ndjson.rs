//! NDJSON message framing over any async byte stream — the north-star transport.
//!
//! One JSON-RPC message per `\n`-delimited line (JSON itself never contains a raw newline, so a
//! line break is an unambiguous frame boundary). Before any RPC, the dialing side writes a single
//! newline-terminated **`hello` preamble** carrying the auth token; the accepting side reads and
//! validates it. This matches the TS Unix-domain-socket / named-pipe transport
//! (`hubrpc/src/node/*`) so a Rust endpoint and a TS endpoint frame bytes identically.
//!
//! Framing is generic over the read/write halves, so the same [`NdjsonTransport`] drives a Unix
//! socket, a named pipe, a TCP connection, or an in-memory duplex pipe. Concrete socket
//! constructors live alongside; the Unix-socket ones are `#[cfg(unix)]`.

use std::io;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tokio::sync::Mutex;

use hubrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};

type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

/// The `hello` preamble line written by the dialing side before any RPC.
///
/// Serializes to `{"hello":1,"token":"…"}` (the `token` field is omitted when absent). The TS
/// hub treats an immediate socket close after this line as an auth failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preamble {
    /// Protocol marker; currently always `1`.
    pub hello: u32,
    /// Auth token, if the endpoint requires one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub token: Option<String>,
}

impl Preamble {
    /// A `hello: 1` preamble carrying `token` (or none).
    pub fn new(token: Option<String>) -> Self {
        Preamble { hello: 1, token }
    }
}

/// A newline-delimited JSON-RPC transport over a split async byte stream.
pub struct NdjsonTransport {
    reader: Mutex<BufReader<BoxRead>>,
    writer: Mutex<BoxWrite>,
}

impl NdjsonTransport {
    /// Build a transport from independent read and write halves.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        NdjsonTransport {
            reader: Mutex::new(BufReader::new(Box::new(reader) as BoxRead)),
            writer: Mutex::new(Box::new(writer) as BoxWrite),
        }
    }

    /// Build a transport from a single bidirectional stream (Unix socket, TCP, duplex pipe, …).
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (r, w): (ReadHalf<S>, WriteHalf<S>) = tokio::io::split(stream);
        NdjsonTransport::new(r, w)
    }

    /// Write the dialer's `hello` preamble. Call once, before any RPC.
    pub async fn write_preamble(&self, preamble: &Preamble) -> io::Result<()> {
        let line = serde_json::to_string(preamble).expect("preamble serializes");
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await
    }

    /// Read and parse the dialer's `hello` preamble (accepting side). Returns `Ok(None)` if the
    /// peer closed the connection before sending it.
    pub async fn read_preamble(&self) -> io::Result<Option<Preamble>> {
        let mut reader = self.reader.lock().await;
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let preamble = serde_json::from_str(line.trim_end_matches(['\n', '\r']))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(preamble))
    }
}

#[async_trait]
impl MessageTransport for NdjsonTransport {
    async fn send(&self, msg: JsonRpcMessage) -> Result<(), TransportError> {
        let line = serde_json::to_string(&msg).map_err(|e| TransportError::Other(e.to_string()))?;
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes()).await.map_err(map_io_err)?;
        w.write_all(b"\n").await.map_err(map_io_err)?;
        w.flush().await.map_err(map_io_err)
    }

    async fn recv(&self) -> Option<JsonRpcMessage> {
        let mut reader = self.reader.lock().await;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.ok()?;
            if n == 0 {
                return None; // EOF
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue; // tolerate blank keep-alive lines
            }
            match serde_json::from_str::<JsonRpcMessage>(trimmed) {
                Ok(msg) => return Some(msg),
                // A malformed line is unrecoverable mid-stream; close the transport.
                Err(_) => return None,
            }
        }
    }
}

fn map_io_err(e: io::Error) -> TransportError {
    if e.kind() == io::ErrorKind::BrokenPipe || e.kind() == io::ErrorKind::UnexpectedEof {
        TransportError::Closed
    } else {
        TransportError::Other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hubrpc::prelude::{JsonRpcNotification, RequestId};

    #[test]
    fn preamble_omits_absent_token() {
        let line = serde_json::to_string(&Preamble::new(None)).unwrap();
        assert_eq!(line, r#"{"hello":1}"#);
        let with = serde_json::to_string(&Preamble::new(Some("t".into()))).unwrap();
        assert_eq!(with, r#"{"hello":1,"token":"t"}"#);
    }

    #[tokio::test]
    async fn frames_multiple_messages_over_duplex() {
        let (a, b) = tokio::io::duplex(4096);
        let writer = NdjsonTransport::from_stream(a);
        let reader = NdjsonTransport::from_stream(b);

        for i in 0..3 {
            let note = JsonRpcMessage::Notification(JsonRpcNotification {
                method: "ping".to_string(),
                params: Some(serde_json::json!({ "n": i })),
            });
            writer.send(note).await.unwrap();
        }

        for i in 0..3 {
            match reader.recv().await.unwrap() {
                JsonRpcMessage::Notification(n) => {
                    assert_eq!(n.method, "ping");
                    assert_eq!(n.params.unwrap()["n"], i);
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn preamble_then_messages_share_one_reader() {
        let (a, b) = tokio::io::duplex(4096);
        let client = NdjsonTransport::from_stream(a);
        let server = NdjsonTransport::from_stream(b);

        client
            .write_preamble(&Preamble::new(Some("tok".into())))
            .await
            .unwrap();
        client
            .send(JsonRpcMessage::Request(hubrpc::prelude::JsonRpcRequest {
                id: RequestId::Number(1),
                method: "m".to_string(),
                params: None,
            }))
            .await
            .unwrap();

        let preamble = server.read_preamble().await.unwrap().unwrap();
        assert_eq!(preamble.token.as_deref(), Some("tok"));
        // The buffered RPC line after the preamble is still readable.
        match server.recv().await.unwrap() {
            JsonRpcMessage::Request(r) => assert_eq!(r.method, "m"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}

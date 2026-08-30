use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use hubrpc::prelude::{MessageTransport, TransportError};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};

use crate::cdp_transport::{ManagedCdpTransport, closed_transport_error};
use crate::session_transport::CdpEnvelope;

const MAX_CDP_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

type Reader = Box<dyn AsyncBufRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

pub struct CdpStdioTransport {
    reader: Mutex<Reader>,
    writer: Mutex<Option<Writer>>,
    close_reason: Arc<Mutex<Option<String>>>,
    closed: watch::Sender<bool>,
}

impl CdpStdioTransport {
    pub fn new(
        reader: impl AsyncBufRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self {
            reader: Mutex::new(Box::new(reader)),
            writer: Mutex::new(Some(Box::new(writer))),
            close_reason: Arc::new(Mutex::new(None)),
            closed: watch::channel(false).0,
        }
    }

    pub fn from_child_stdio(
        stdout: tokio::process::ChildStdout,
        stdin: tokio::process::ChildStdin,
    ) -> Self {
        Self::new(BufReader::new(stdout), stdin)
    }

    async fn close_with(&self, reason: String) {
        let mut close_reason = self.close_reason.lock().await;
        if close_reason.is_none() {
            *close_reason = Some(reason);
        }
        self.closed.send_replace(true);
    }
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for CdpStdioTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        if *self.closed.borrow() {
            return Err(TransportError::Closed);
        }
        let mut json = serde_json::to_vec(&envelope)
            .map_err(|error| closed_transport_error(error.to_string()))?;
        if json.len() > MAX_CDP_MESSAGE_SIZE {
            return Err(closed_transport_error(format!(
                "CDP message exceeds the {MAX_CDP_MESSAGE_SIZE}-byte limit"
            )));
        }
        json.push(b'\n');
        let mut writer = self.writer.lock().await;
        let Some(writer) = writer.as_mut() else {
            return Err(TransportError::Closed);
        };
        if let Err(error) = writer.write_all(&json).await {
            self.close_with(format!("stdio CDP write failed: {error}"))
                .await;
            return Err(closed_transport_error(error.to_string()));
        }
        Ok(())
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        let mut closed = self.closed.subscribe();
        if *closed.borrow() {
            return None;
        }
        let line = {
            let mut reader = self.reader.lock().await;
            tokio::select! {
                changed = closed.changed() => {
                    let _ = changed;
                    return None;
                }
                line = read_bounded_line(reader.as_mut()) => line,
            }
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => {
                self.close_with("stdio CDP stdout closed".into()).await;
                return None;
            }
            Err(error) => {
                self.close_with(format!("stdio CDP read failed: {error}"))
                    .await;
                return None;
            }
        };
        match serde_json::from_slice(&line) {
            Ok(envelope) => Some(envelope),
            Err(error) => {
                self.close_with(format!("invalid CDP JSON on stdout: {error}"))
                    .await;
                None
            }
        }
    }
}

#[async_trait]
impl ManagedCdpTransport for CdpStdioTransport {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.close_reason.clone()
    }

    async fn wait_closed(&self) -> String {
        let mut closed = self.closed.subscribe();
        if !*closed.borrow() {
            let _ = closed.changed().await;
        }
        self.close_reason
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "stdio CDP transport closed".into())
    }

    async fn close(&self) {
        self.close_with("stdio CDP transport closed by debugger service".into())
            .await;
        if let Some(mut writer) = self.writer.lock().await.take()
            && let Err(error) = writer.shutdown().await
        {
            self.close_with(format!("stdio CDP stdin shutdown failed: {error}"))
                .await;
        }
    }
}

async fn read_bounded_line(
    reader: &mut (dyn AsyncBufRead + Send + Unpin),
) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stdout ended before the CDP message newline",
                ))
            };
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len() + end > MAX_CDP_MESSAGE_SIZE + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CDP message exceeds the {MAX_CDP_MESSAGE_SIZE}-byte limit"),
            ));
        }
        line.extend_from_slice(&available[..end]);
        reader.consume(end);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hubrpc::prelude::{JsonRpcMessage, JsonRpcRequest};
    use hubrpc::protocol::jsonrpc::RequestId;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, duplex};

    fn request() -> CdpEnvelope {
        CdpEnvelope {
            session_id: None,
            message: JsonRpcMessage::Request(JsonRpcRequest {
                id: RequestId::Number(1),
                method: "Runtime.enable".into(),
                params: Some(json!({})),
            }),
        }
    }

    #[tokio::test]
    async fn sends_one_compact_json_message_per_line() {
        let (input, _server_output) = duplex(4096);
        let (mut server_input, output) = duplex(4096);
        let transport = CdpStdioTransport::new(BufReader::new(input), output);

        transport.send(request()).await.unwrap();
        let mut bytes = vec![0; 4096];
        let count = server_input.read(&mut bytes).await.unwrap();
        let text = std::str::from_utf8(&bytes[..count]).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.matches('\n').count(), 1);
        assert_eq!(
            serde_json::from_str::<CdpEnvelope>(text.trim_end()).unwrap(),
            request()
        );
    }

    #[tokio::test]
    async fn receives_fragmented_newline_delimited_json() {
        let (input, mut server_output) = duplex(4096);
        let (_server_input, output) = duplex(4096);
        let transport = CdpStdioTransport::new(BufReader::new(input), output);
        let json = serde_json::to_string(&request()).unwrap();

        server_output
            .write_all(&json.as_bytes()[..7])
            .await
            .unwrap();
        server_output
            .write_all(&json.as_bytes()[7..])
            .await
            .unwrap();
        server_output.write_all(b"\n").await.unwrap();

        assert_eq!(transport.recv().await.unwrap(), request());
    }

    #[tokio::test]
    async fn rejects_unterminated_json_at_eof() {
        let (input, mut server_output) = duplex(4096);
        let (_server_input, output) = duplex(4096);
        let transport = CdpStdioTransport::new(BufReader::new(input), output);
        server_output.write_all(b"{\"id\":1}").await.unwrap();
        server_output.shutdown().await.unwrap();

        assert!(transport.recv().await.is_none());
        assert!(
            transport
                .wait_closed()
                .await
                .contains("before the CDP message newline")
        );
    }
}

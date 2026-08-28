use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use hubrpc::prelude::{MessageTransport, TransportError};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::{Mutex, watch};
use tokio::time::timeout;

use crate::cdp::{
    RuntimeCallArgument, RuntimeCallFunctionOnParams, RuntimeEvaluateParams,
    RuntimeReleaseObjectGroupParams,
};
use crate::cdp_runtime::CdpConnection;
use crate::cdp_transport::{ManagedCdpTransport, closed_transport_error};
use crate::session_transport::CdpEnvelope;

const BRIDGE_SOURCE: &str = include_str!("providers/electron_renderer_bridge.js");
const BRIDGE_OBJECT_GROUP: &str = "jsdbg-electron-renderer-bridge";
const MAX_SOCKET_MESSAGE_BYTES: usize = 128 * 1024 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ElectronRendererTarget {
    pub web_contents_id: u64,
    pub process_id: u32,
    #[serde(rename = "type")]
    pub target_type: String,
    pub title: String,
    pub url: String,
}

pub struct ElectronRendererBridge {
    client: Arc<CdpConnection>,
    object_id: String,
    port: u16,
    token: String,
    control: Arc<BridgeControl>,
    disposed: AtomicBool,
}

impl ElectronRendererBridge {
    pub async fn install(client: Arc<CdpConnection>) -> Result<Arc<Self>, TransportError> {
        let token = random_token()?;
        let token_json = serde_json::to_string(&token).map_err(|error| {
            transport_error(format!("failed to serialize bridge token: {error}"))
        })?;
        let expression = format!("({BRIDGE_SOURCE})({token_json})");
        let mut params = RuntimeEvaluateParams::new(expression);
        params.object_group = Some(BRIDGE_OBJECT_GROUP.to_owned());
        params.include_command_line_api = Some(false);
        params.silent = Some(false);
        params.return_by_value = Some(false);
        params.generate_preview = Some(false);
        params.user_gesture = Some(false);
        params.await_promise = Some(true);
        let response = client
            .root()
            .runtime_evaluate(params)
            .await
            .map_err(|error| {
                transport_error(format!("failed to install renderer bridge: {error:?}"))
            })?;
        if let Some(exception) = response.exception_details {
            return Err(transport_error(format!(
                "failed to install renderer bridge: {}",
                exception.text
            )));
        }
        let object_id = response
            .result
            .object_id
            .ok_or_else(|| transport_error("renderer bridge did not return a remote object"))?;
        let endpoint: BridgeEndpoint = call_bridge_object(
            &client,
            &object_id,
            "function() { return this.endpoint(); }",
        )
        .await?;
        let control = match BridgeControl::connect(endpoint.port, &token).await {
            Ok(control) => control,
            Err(error) => {
                let _ = client
                    .root()
                    .runtime_release_object_group(RuntimeReleaseObjectGroupParams {
                        object_group: BRIDGE_OBJECT_GROUP.to_owned(),
                    })
                    .await;
                return Err(error);
            }
        };
        Ok(Arc::new(Self {
            client,
            object_id,
            port: endpoint.port,
            token,
            control,
            disposed: AtomicBool::new(false),
        }))
    }

    pub async fn list_targets(&self) -> Result<Vec<ElectronRendererTarget>, TransportError> {
        self.ensure_open().await?;
        self.call("function() { return this.list(); }").await
    }

    pub async fn target_for_process(
        &self,
        process_id: u32,
    ) -> Result<ElectronRendererTarget, String> {
        self.list_targets()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|target| target.process_id == process_id)
            .ok_or_else(|| format!("Electron renderer bridge did not find process {process_id}"))
    }

    pub async fn attach(
        self: &Arc<Self>,
        target_id: String,
        target: &ElectronRendererTarget,
        force: bool,
    ) -> Result<(Arc<ElectronRendererTransport>, bool), String> {
        self.ensure_open()
            .await
            .map_err(|error| error.to_string())?;
        ElectronRendererTransport::connect(
            self.port,
            &self.token,
            target.web_contents_id,
            target.process_id,
            target_id,
            force,
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub async fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.control.dispose().await;
        let _ = self
            .client
            .root()
            .runtime_release_object_group(RuntimeReleaseObjectGroupParams {
                object_group: BRIDGE_OBJECT_GROUP.to_owned(),
            })
            .await;
    }

    async fn ensure_open(&self) -> Result<(), TransportError> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(closed_transport_error("renderer bridge is disposed"));
        }
        if let Some(reason) = self.control.close_reason() {
            return Err(closed_transport_error(reason));
        }
        Ok(())
    }

    async fn call<T: DeserializeOwned>(
        &self,
        function_declaration: &str,
    ) -> Result<T, TransportError> {
        call_bridge_object(&self.client, &self.object_id, function_declaration).await
    }
}

async fn call_bridge_object<T: DeserializeOwned>(
    client: &CdpConnection,
    object_id: &str,
    function_declaration: &str,
) -> Result<T, TransportError> {
    let mut params = RuntimeCallFunctionOnParams::new(function_declaration.to_owned());
    params.object_id = Some(object_id.to_owned());
    params.arguments = Some(Vec::<RuntimeCallArgument>::new());
    params.silent = Some(false);
    params.return_by_value = Some(true);
    params.generate_preview = Some(false);
    params.user_gesture = Some(false);
    params.await_promise = Some(true);
    let response = client
        .root()
        .runtime_call_function_on(params)
        .await
        .map_err(|error| transport_error(format!("renderer bridge call failed: {error:?}")))?;
    if let Some(exception) = response.exception_details {
        return Err(transport_error(format!(
            "renderer bridge call failed: {}",
            exception.text
        )));
    }
    let value = response
        .result
        .value
        .ok_or_else(|| transport_error("renderer bridge call returned no value"))?;
    serde_json::from_value(value)
        .map_err(|error| transport_error(format!("invalid renderer bridge response: {error}")))
}

#[derive(Debug, Deserialize)]
struct BridgeEndpoint {
    port: u16,
}

struct BridgeControl {
    writer: Mutex<Option<OwnedWriteHalf>>,
    closed_tx: watch::Sender<Option<String>>,
}

impl BridgeControl {
    async fn connect(port: u16, token: &str) -> Result<Arc<Self>, TransportError> {
        let stream = connect_socket(("127.0.0.1", port)).await?;
        let (read_half, mut write_half) = stream.into_split();
        write_json_line(
            &mut write_half,
            &HandshakeRequest {
                token,
                role: "control",
                web_contents_id: None,
                force: false,
            },
        )
        .await?;
        let mut reader = BufReader::new(read_half);
        let response: HandshakeResponse = timeout(SOCKET_TIMEOUT, read_json_line(&mut reader))
            .await
            .map_err(|_| transport_error("renderer bridge control handshake timed out"))??;
        if !response.ready {
            return Err(transport_error(response.error.unwrap_or_else(|| {
                "renderer bridge rejected control handshake".to_owned()
            })));
        }
        let (closed_tx, _) = watch::channel(None);
        let control = Arc::new(Self {
            writer: Mutex::new(Some(write_half)),
            closed_tx,
        });
        tokio::spawn(supervise_control(reader, control.closed_tx.clone()));
        Ok(control)
    }

    fn close_reason(&self) -> Option<String> {
        self.closed_tx.borrow().clone()
    }

    async fn dispose(&self) {
        if self.close_reason().is_none() {
            let write_result = {
                let mut writer = self.writer.lock().await;
                match writer.as_mut() {
                    Some(writer) => write_json_line(writer, &ClientFrame::Dispose).await,
                    None => Ok(()),
                }
            };
            if let Err(error) = write_result {
                set_close_reason(&self.closed_tx, error.to_string());
            } else {
                let mut closed_rx = self.closed_tx.subscribe();
                let wait = async {
                    while closed_rx.borrow().is_none() {
                        if closed_rx.changed().await.is_err() {
                            break;
                        }
                    }
                };
                if timeout(SOCKET_TIMEOUT, wait).await.is_err() {
                    set_close_reason(
                        &self.closed_tx,
                        "renderer bridge dispose acknowledgement timed out",
                    );
                }
            }
        }
        if let Some(mut writer) = self.writer.lock().await.take() {
            let _ = writer.shutdown().await;
        }
    }
}

async fn supervise_control(
    mut reader: BufReader<OwnedReadHalf>,
    closed_tx: watch::Sender<Option<String>>,
) {
    let reason = match read_json_line::<ControlServerFrame, _>(&mut reader).await {
        Ok(ControlServerFrame::Disposed) => "renderer bridge disposed".to_owned(),
        Err(error) => format!("renderer bridge control socket closed: {error}"),
    };
    set_close_reason(&closed_tx, reason);
}

pub struct ElectronRendererTransport {
    process_id: u32,
    target_id: String,
    sender: Mutex<Option<OwnedWriteHalf>>,
    receiver: Mutex<BufReader<OwnedReadHalf>>,
    close_reason: Arc<Mutex<Option<String>>>,
    closed_tx: watch::Sender<Option<String>>,
}

impl ElectronRendererTransport {
    async fn connect(
        port: u16,
        token: &str,
        web_contents_id: u64,
        process_id: u32,
        target_id: String,
        force: bool,
    ) -> Result<(Arc<Self>, bool), TransportError> {
        let stream = connect_socket(("127.0.0.1", port)).await?;
        let (read_half, mut write_half) = stream.into_split();
        write_json_line(
            &mut write_half,
            &HandshakeRequest {
                token,
                role: "renderer",
                web_contents_id: Some(web_contents_id),
                force,
            },
        )
        .await?;
        let mut reader = BufReader::new(read_half);
        let response: HandshakeResponse = timeout(SOCKET_TIMEOUT, read_json_line(&mut reader))
            .await
            .map_err(|_| {
                transport_error(format!(
                    "renderer bridge handshake timed out for process {process_id}"
                ))
            })??;
        if !response.ready {
            return Err(transport_error(response.error.unwrap_or_else(|| {
                format!("renderer bridge rejected process {process_id}")
            })));
        }
        let (closed_tx, _) = watch::channel(None);
        let stolen = response.stolen;
        Ok((
            Arc::new(Self {
                process_id,
                target_id,
                sender: Mutex::new(Some(write_half)),
                receiver: Mutex::new(reader),
                close_reason: Arc::new(Mutex::new(None)),
                closed_tx,
            }),
            stolen,
        ))
    }

    async fn mark_closed(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let mut close_reason = self.close_reason.lock().await;
        if close_reason.is_none() {
            *close_reason = Some(reason.clone());
            self.closed_tx.send_replace(Some(reason));
        }
    }

    async fn current_close_reason(&self) -> Option<String> {
        self.close_reason.lock().await.clone()
    }

    async fn shutdown_writer(&self) {
        if let Some(mut writer) = self.sender.lock().await.take() {
            let _ = writer.shutdown().await;
        }
    }
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for ElectronRendererTransport {
    async fn send(&self, message: CdpEnvelope) -> Result<(), TransportError> {
        if let Some(reason) = self.current_close_reason().await {
            return Err(closed_transport_error(reason));
        }
        let result = {
            let mut sender = self.sender.lock().await;
            let sender = sender.as_mut().ok_or_else(|| {
                closed_transport_error(format!(
                    "Electron renderer transport {} for process {} is closed",
                    self.target_id, self.process_id
                ))
            })?;
            write_json_line(sender, &ClientFrame::Cdp { envelope: message }).await
        };
        if let Err(error) = result {
            self.mark_closed(format!(
                "Electron renderer transport {} for process {} failed to send: {error}",
                self.target_id, self.process_id
            ))
            .await;
            self.shutdown_writer().await;
            return Err(error);
        }
        Ok(())
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        if self.current_close_reason().await.is_some() {
            return None;
        }
        let result = {
            let mut receiver = self.receiver.lock().await;
            read_json_line::<ServerFrame, _>(&mut *receiver).await
        };
        match result {
            Ok(ServerFrame::Cdp { envelope }) => Some(envelope),
            Ok(ServerFrame::Closed { reason }) => {
                self.mark_closed(reason).await;
                self.shutdown_writer().await;
                None
            }
            Err(error) => {
                self.mark_closed(format!(
                    "Electron renderer transport {} for process {} closed: {error}",
                    self.target_id, self.process_id
                ))
                .await;
                self.shutdown_writer().await;
                None
            }
        }
    }
}

#[async_trait]
impl ManagedCdpTransport for ElectronRendererTransport {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.close_reason.clone()
    }

    async fn wait_closed(&self) -> String {
        if let Some(reason) = self.current_close_reason().await {
            return reason;
        }
        let mut closed_rx = self.closed_tx.subscribe();
        loop {
            if let Some(reason) = closed_rx.borrow().clone() {
                return reason;
            }
            if closed_rx.changed().await.is_err() {
                return format!(
                    "Electron renderer transport {} for process {} closed",
                    self.target_id, self.process_id
                );
            }
        }
    }

    async fn close(&self) {
        if self.current_close_reason().await.is_some() {
            self.shutdown_writer().await;
            return;
        }
        let send_result = {
            let mut sender = self.sender.lock().await;
            match sender.as_mut() {
                Some(sender) => write_json_line(sender, &ClientFrame::Close).await,
                None => Ok(()),
            }
        };
        if let Err(error) = send_result {
            self.mark_closed(format!(
                "Electron renderer transport {} for process {} failed to close: {error}",
                self.target_id, self.process_id
            ))
            .await;
        } else {
            let mut closed_rx = self.closed_tx.subscribe();
            let wait = async {
                while closed_rx.borrow().is_none() {
                    if closed_rx.changed().await.is_err() {
                        break;
                    }
                }
            };
            if timeout(SOCKET_TIMEOUT, wait).await.is_err() {
                self.mark_closed(format!(
                    "Electron renderer transport {} for process {} close acknowledgement timed out",
                    self.target_id, self.process_id
                ))
                .await;
            }
        }
        self.shutdown_writer().await;
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HandshakeRequest<'a> {
    token: &'a str,
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    web_contents_id: Option<u64>,
    force: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HandshakeResponse {
    ready: bool,
    error: Option<String>,
    #[serde(default)]
    stolen: bool,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ClientFrame {
    Cdp { envelope: CdpEnvelope },
    Close,
    Dispose,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ServerFrame {
    Cdp { envelope: CdpEnvelope },
    Closed { reason: String },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ControlServerFrame {
    Disposed,
}

async fn connect_socket(address: impl ToSocketAddrs) -> Result<TcpStream, TransportError> {
    let stream = timeout(SOCKET_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| transport_error("renderer bridge socket connection timed out"))?
        .map_err(|error| transport_error(format!("failed to connect renderer bridge: {error}")))?;
    stream.set_nodelay(true).map_err(|error| {
        transport_error(format!("failed to configure renderer bridge: {error}"))
    })?;
    Ok(stream)
}

async fn write_json_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), TransportError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| transport_error(format!("failed to serialize socket message: {error}")))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_SOCKET_MESSAGE_BYTES {
        return Err(transport_error(
            "renderer bridge socket message exceeded 128 MiB",
        ));
    }
    writer.write_all(&bytes).await.map_err(|error| {
        transport_error(format!("renderer bridge socket write failed: {error}"))
    })?;
    writer
        .flush()
        .await
        .map_err(|error| transport_error(format!("renderer bridge socket flush failed: {error}")))
}

async fn read_json_line<T: DeserializeOwned, R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<T, TransportError> {
    let mut bytes = Vec::new();
    let count = reader
        .take((MAX_SOCKET_MESSAGE_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| transport_error(format!("renderer bridge socket read failed: {error}")))?;
    if count == 0 {
        return Err(closed_transport_error(
            "renderer bridge socket closed by peer",
        ));
    }
    if bytes.len() > MAX_SOCKET_MESSAGE_BYTES || bytes.last() != Some(&b'\n') {
        return Err(transport_error(
            "renderer bridge socket message exceeded 128 MiB",
        ));
    }
    bytes.pop();
    serde_json::from_slice(&bytes).map_err(|error| {
        transport_error(format!("invalid renderer bridge socket message: {error}"))
    })
}

fn set_close_reason(closed_tx: &watch::Sender<Option<String>>, reason: impl Into<String>) {
    if closed_tx.borrow().is_none() {
        closed_tx.send_replace(Some(reason.into()));
    }
}

fn random_token() -> Result<String, TransportError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| transport_error(format!("failed to generate bridge token: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn transport_error(message: impl Into<String>) -> TransportError {
    TransportError::Other(message.into())
}

#[cfg(test)]
mod tests {
    use hubrpc::prelude::MessageTransport;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn socket_transport_round_trips_cdp_and_closes_with_acknowledgement() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let handshake: Value = read_json_line(&mut reader).await.unwrap();
            assert_eq!(handshake["token"], "secret");
            assert_eq!(handshake["role"], "renderer");
            assert_eq!(handshake["webContentsId"], 7);
            drop(reader);
            write_json_line(
                &mut stream,
                &HandshakeResponseForTest {
                    ready: true,
                    error: None,
                },
            )
            .await
            .unwrap();

            let mut reader = BufReader::new(&mut stream);
            let frame: Value = read_json_line(&mut reader).await.unwrap();
            assert_eq!(frame["kind"], "cdp");
            assert_eq!(frame["envelope"]["method"], "Runtime.enable");
            drop(reader);
            write_json_line(
                &mut stream,
                &json!({
                    "kind": "cdp",
                    "envelope": { "id": 1, "result": {} }
                }),
            )
            .await
            .unwrap();

            let mut reader = BufReader::new(&mut stream);
            let frame: Value = read_json_line(&mut reader).await.unwrap();
            assert_eq!(frame["kind"], "close");
            drop(reader);
            write_json_line(
                &mut stream,
                &json!({ "kind": "closed", "reason": "released" }),
            )
            .await
            .unwrap();
        });

        let (transport, stolen) = ElectronRendererTransport::connect(
            port,
            "secret",
            7,
            42,
            "renderer-42".to_owned(),
            false,
        )
        .await
        .unwrap();
        assert!(!stolen);
        let request: CdpEnvelope =
            serde_json::from_value(json!({ "id": 1, "method": "Runtime.enable" })).unwrap();
        transport.send(request).await.unwrap();
        let response = transport.recv().await.unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({ "id": 1, "result": {} })
        );
        let receiver = transport.clone();
        let recv_task = tokio::spawn(async move { receiver.recv().await });
        transport.close().await;
        assert!(recv_task.await.unwrap().is_none());
        assert_eq!(transport.wait_closed().await, "released");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn peer_disconnect_wakes_renderer_transport_waiters() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let _: Value = read_json_line(&mut reader).await.unwrap();
            drop(reader);
            write_json_line(
                &mut stream,
                &HandshakeResponseForTest {
                    ready: true,
                    error: None,
                },
            )
            .await
            .unwrap();
        });

        let (transport, _) = ElectronRendererTransport::connect(
            port,
            "secret",
            7,
            42,
            "renderer-42".to_owned(),
            false,
        )
        .await
        .unwrap();
        assert!(transport.recv().await.is_none());
        let reason = timeout(Duration::from_secs(1), transport.wait_closed())
            .await
            .unwrap();
        assert!(reason.contains("closed by peer"), "{reason}");
    }

    #[tokio::test]
    async fn renderer_handshake_rejection_is_reported() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let _: Value = read_json_line(&mut reader).await.unwrap();
            drop(reader);
            write_json_line(
                &mut stream,
                &HandshakeResponseForTest {
                    ready: false,
                    error: Some("bad token"),
                },
            )
            .await
            .unwrap();
        });

        let result = ElectronRendererTransport::connect(
            port,
            "wrong",
            7,
            42,
            "renderer-42".to_owned(),
            false,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("handshake unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("bad token"));
    }

    #[tokio::test]
    async fn renderer_force_handshake_reports_stolen_owner() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let handshake: Value = read_json_line(&mut reader).await.unwrap();
            assert_eq!(handshake["force"], true);
            drop(reader);
            write_json_line(&mut stream, &json!({ "ready": true, "stolen": true }))
                .await
                .unwrap();
        });

        let (_, stolen) = ElectronRendererTransport::connect(
            port,
            "secret",
            7,
            42,
            "renderer-42".to_owned(),
            true,
        )
        .await
        .unwrap();
        assert!(stolen);
    }

    #[tokio::test]
    async fn invalid_renderer_frame_closes_transport_with_diagnostic() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let _: Value = read_json_line(&mut reader).await.unwrap();
            drop(reader);
            write_json_line(
                &mut stream,
                &HandshakeResponseForTest {
                    ready: true,
                    error: None,
                },
            )
            .await
            .unwrap();
            stream.write_all(b"not-json\n").await.unwrap();
        });

        let (transport, _) = ElectronRendererTransport::connect(
            port,
            "secret",
            7,
            42,
            "renderer-42".to_owned(),
            false,
        )
        .await
        .unwrap();
        assert!(transport.recv().await.is_none());
        assert!(
            transport
                .wait_closed()
                .await
                .contains("invalid renderer bridge socket message")
        );
    }

    #[tokio::test]
    async fn control_dispose_waits_for_server_acknowledgement() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let handshake: Value = read_json_line(&mut reader).await.unwrap();
            assert_eq!(handshake["token"], "secret");
            assert_eq!(handshake["role"], "control");
            drop(reader);
            write_json_line(
                &mut stream,
                &HandshakeResponseForTest {
                    ready: true,
                    error: None,
                },
            )
            .await
            .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let frame: Value = read_json_line(&mut reader).await.unwrap();
            assert_eq!(frame["kind"], "dispose");
            drop(reader);
            write_json_line(&mut stream, &json!({ "kind": "disposed" }))
                .await
                .unwrap();
        });

        let control = BridgeControl::connect(port, "secret").await.unwrap();
        control.dispose().await;
        assert_eq!(
            control.close_reason().as_deref(),
            Some("renderer bridge disposed")
        );
        server.await.unwrap();
    }

    #[derive(Serialize)]
    struct HandshakeResponseForTest<'a> {
        ready: bool,
        error: Option<&'a str>,
    }
}

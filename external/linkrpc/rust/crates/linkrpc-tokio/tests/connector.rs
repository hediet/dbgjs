//! M6 connector integration: drive [`connect_to_hub`](linkrpc_tokio::connect_to_hub) end-to-end over
//! both transports — a Unix-domain socket (with `hello` preamble) and a WebSocket (token in the
//! `Authorization` header). Both wrap the same `LinkRpcConnection`, so a single helper serves the
//! pizza interface and both clients exercise the real request/response path.

use std::sync::Arc;

use async_trait::async_trait;
use linkrpc::prelude::*;
use linkrpc_tokio::{connect_to_hub, ConnectOptions, WebSocketTransport};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;

const PIZZA_ID: &str = "com.acme.pizza";
const TOKEN: &str = "s3cret-token";

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PizzaKind {
    Margherita,
    Pepperoni,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderArgs {
    kind: PizzaKind,
    quantity: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderConfirmation {
    order_id: String,
    price_cents: u32,
}

fn subset_of<T: JsonSchema>() -> Value {
    linkrpc::schema::schemars_to_subset(&serde_json::to_value(schema_for!(T)).unwrap())
        .expect("schemars→subset succeeds")
}

fn pizza_interface() -> InterfaceDefinition {
    let order = Member::Request(Box::new(RequestMember {
        params_schema: subset_of::<OrderArgs>(),
        result_schema: subset_of::<OrderConfirmation>(),
        client_stream_schema: None,
        server_stream_schema: None,
        docs: MemberDocs {
            description: Some("Place a new pizza order. Charges money.".to_string()),
            ..Default::default()
        },
    }));
    InterfaceDefinition::new(
        InterfaceInfo::new(PIZZA_ID).with_description("Order pizzas and track delivery."),
        vec![("order".to_string(), order)],
    )
}

struct PizzaShop;

#[async_trait]
impl InterfaceHandler for PizzaShop {
    async fn handle_request(
        &self,
        member: &str,
        params: JsonValue,
        _ctx: CallCtx,
    ) -> Result<JsonValue, JsonRpcError> {
        match member {
            "order" => {
                let args: OrderArgs = serde_json::from_value(params)
                    .map_err(|e| JsonRpcError::new(error_codes::INVALID_PARAMS, e.to_string()))?;
                Ok(serde_json::to_value(OrderConfirmation {
                    order_id: "ord-1".to_string(),
                    price_cents: 1299 * args.quantity,
                })
                .unwrap())
            }
            _ => Err(JsonRpcError::new(error_codes::METHOD_NOT_FOUND, member)),
        }
    }
}

fn serve(transport: Box<dyn MessageTransport>) -> Arc<LinkRpcConnection> {
    let conn = Arc::new(LinkRpcConnection::new(transport));
    conn.register(
        Arc::new(pizza_interface()),
        Arc::new(PizzaShop) as Arc<dyn InterfaceHandler>,
        RegisterOptions::default(),
    )
    .unwrap();
    conn.enable_reflection();
    let run = conn.clone();
    tokio::spawn(async move { run.run().await });
    conn
}

async fn assert_order(client: &LinkRpcConnection) {
    let confirmation = client
        .call_member(
            None,
            PIZZA_ID,
            "order",
            json!({ "kind": "pepperoni", "quantity": 3 }),
        )
        .await
        .expect("order succeeds");
    assert_eq!(confirmation["order_id"], "ord-1");
    assert_eq!(confirmation["price_cents"], 3897);
}

#[cfg(unix)]
#[tokio::test]
async fn connect_to_hub_over_unix_socket_with_preamble() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("linkrpc-m6-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = linkrpc_tokio::unix::UnixHubListener::bind(&path).unwrap();

    let server_path = path.clone();
    let server = tokio::spawn(async move {
        let transport = listener.accept().await.unwrap();
        let preamble = transport
            .read_preamble()
            .await
            .expect("read preamble")
            .expect("preamble present");
        assert_eq!(preamble.token.as_deref(), Some(TOKEN));
        let _conn = serve(Box::new(transport));
        // Keep the server alive until the test drops it.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let _ = std::fs::remove_file(&server_path);
    });

    let endpoint = format!("unix:{}", path.display());
    let client = connect_to_hub(ConnectOptions {
        endpoint: Some(endpoint),
        linkrpc_token: Some(TOKEN.to_string()),
        ..Default::default()
    })
    .await
    .expect("connector dials UDS");
    let client = Arc::new(client);
    let run = client.clone();
    tokio::spawn(async move { run.run().await });

    assert_order(&client).await;
    server.abort();
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn connect_to_hub_over_websocket_with_authorization_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // Capture the Authorization header during the handshake.
        let seen_auth = Arc::new(std::sync::Mutex::new(None::<String>));
        let captured = seen_auth.clone();
        let ws = tokio_tungstenite::accept_hdr_async(
            stream,
            |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
             resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                if let Some(value) = req.headers().get("authorization") {
                    *captured.lock().unwrap() =
                        Some(value.to_str().unwrap_or_default().to_string());
                }
                Ok(resp)
            },
        )
        .await
        .unwrap();

        assert_eq!(
            seen_auth.lock().unwrap().as_deref(),
            Some(format!("Bearer {TOKEN}").as_str())
        );

        let transport = WebSocketTransport::new(ws);
        let _conn = serve(Box::new(transport));
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });

    let endpoint = format!("ws://{addr}/rpc?token={TOKEN}");
    let client = connect_to_hub(ConnectOptions {
        endpoint: Some(endpoint),
        ..Default::default()
    })
    .await
    .expect("connector dials WS");
    let client = Arc::new(client);
    let run = client.clone();
    tokio::spawn(async move { run.run().await });

    assert_order(&client).await;
    server.abort();
}

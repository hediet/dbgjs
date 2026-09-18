//! End-to-end pizza round-trip over a real socket using the NDJSON transport.
//!
//! Uses a TCP loopback connection (works on every OS, including this Windows dev box) to exercise
//! the actual byte framing and the `hello` preamble. The Unix-socket constructors share the exact
//! same [`NdjsonTransport::from_stream`] framing path, so this also covers the north-star UDS link.

use std::sync::Arc;

use async_trait::async_trait;
use linkrpc::prelude::*;
use linkrpc_tokio::{NdjsonTransport, Preamble};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};

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

#[tokio::test]
async fn pizza_round_trips_over_socket_with_preamble() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Server: accept one connection, validate the hello preamble, then serve pizza + reflection.
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let transport = NdjsonTransport::from_stream(stream);

        let preamble = transport
            .read_preamble()
            .await
            .expect("read preamble")
            .expect("preamble present");
        assert_eq!(preamble.token.as_deref(), Some(TOKEN));

        let conn = LinkRpcConnection::new(Box::new(transport));
        conn.register(
            Arc::new(pizza_interface()),
            Arc::new(PizzaShop) as Arc<dyn InterfaceHandler>,
            RegisterOptions::default(),
        )
        .unwrap();
        conn.enable_reflection();
        conn.run().await;
    });

    // Client: dial, write the hello preamble, then drive requests.
    let stream = TcpStream::connect(addr).await.unwrap();
    let transport = NdjsonTransport::from_stream(stream);
    transport
        .write_preamble(&Preamble::new(Some(TOKEN.to_string())))
        .await
        .expect("write preamble");

    let client = LinkRpcConnection::new(Box::new(transport));
    let client_loop = client.clone();
    tokio::spawn(async move { client_loop.run().await });

    // 1) request/response over real socket framing.
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

    // 2) reflection works the same way over the socket.
    let listing = client
        .call_member(None, "hubrpc.directory", "list", json!({}))
        .await
        .expect("directory list");
    let items = listing["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["interfaceId"] == PIZZA_ID));

    // 3) fetch the schema; its id must match.
    let schema = client
        .call_member(
            None,
            "hubrpc.schemas",
            "get",
            json!({ "interfaceId": PIZZA_ID }),
        )
        .await
        .expect("schemas get");
    assert_eq!(schema["schema"]["id"], PIZZA_ID);

    server.abort();
}

#[tokio::test]
async fn recv_returns_none_on_peer_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // Drop immediately to close the connection.
        drop(stream);
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let transport = NdjsonTransport::from_stream(stream);
    server.await.unwrap();

    // The reader half observes EOF and yields None.
    assert!(
        MessageTransport::recv(&transport).await.is_none(),
        "recv should yield None after the peer closes"
    );
}

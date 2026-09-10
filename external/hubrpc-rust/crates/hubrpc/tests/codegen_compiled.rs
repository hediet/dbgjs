//! Compile-validation for generated code.
//!
//! Including the golden output as a module forces the test crate to compile
//! every generated type and the async client, proving the generator emits
//! valid, recursive Rust that references only `hubrpc::prelude` + serde.

// The golden fixture is emitted verbatim by the codegen and compared
// byte-for-byte by the determinism test; `rustfmt.toml` keeps `cargo fmt`
// from reformatting it.
#[path = "codegen/generated_graph.rs"]
mod generated;

use generated::*;

/// A transport stub that returns canned results so the generated async client's
/// method bodies (params encode + result decode) are fully type-checked and
/// exercised end to end.
struct StubCaller;

#[async_trait::async_trait]
impl hubrpc::prelude::RpcCall for StubCaller {
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, hubrpc::prelude::JsonRpcError> {
        // `raw_echo` has a Value result, so echo the params straight back.
        if method.ends_with("raw_echo") {
            return Ok(params);
        }
        if method.ends_with("paint") || method.ends_with("configure") {
            return Ok(serde_json::json!(true));
        }
        // `get_tree` decodes into TreeNode.
        Ok(serde_json::json!({ "value": "root" }))
    }

    async fn notify(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> Result<(), hubrpc::prelude::JsonRpcError> {
        Ok(())
    }
}

#[test]
fn recursive_types_round_trip() {
    // Self-recursive struct with Box indirection through an optional field.
    let node = TreeNode {
        value: "root".into(),
        point: Some(Point { x: 1.0, y: 2.0 }),
        parent: Some(Box::new(TreeNode {
            value: "parent".into(),
            point: None,
            parent: None,
            children: None,
        })),
        children: Some(vec![TreeNode {
            value: "child".into(),
            point: None,
            parent: None,
            children: None,
        }]),
    };
    let json = serde_json::to_value(&node).unwrap();
    let back: TreeNode = serde_json::from_value(json).unwrap();
    assert_eq!(back.value, "root");
    assert_eq!(back.children.unwrap().len(), 1);

    // Mutually recursive structs, each boxing the other.
    let ping = Ping {
        pong: Some(Box::new(Pong {
            ping: Some(Box::new(Ping { pong: None })),
        })),
    };
    let round: Ping = serde_json::from_value(serde_json::to_value(&ping).unwrap()).unwrap();
    assert!(round.pong.is_some());

    // Recursion purely through Vec/Map needs no Box.
    let tree = JsonNode::Variant4(std::collections::HashMap::from([(
        "k".to_string(),
        JsonNode::Variant3(vec![JsonNode::Variant0("v".into())]),
    )]));
    let _: JsonNode = serde_json::from_value(serde_json::to_value(&tree).unwrap()).unwrap();
}

#[test]
fn tagged_union_serializes_with_discriminator() {
    let shape = Shape::Rectangle {
        width: 3.0,
        height: 4.0,
    };
    let json = serde_json::to_value(&shape).unwrap();
    assert_eq!(json["kind"], "rectangle");
    assert_eq!(json["width"], 3.0);

    let json = serde_json::to_value(Shape::Point).unwrap();
    assert_eq!(json["kind"], "point");
}

#[test]
fn string_enum_uses_renames() {
    assert_eq!(serde_json::to_value(Color::Green).unwrap(), "green");
    let c: Color = serde_json::from_value(serde_json::json!("blue")).unwrap();
    assert_eq!(c, Color::Blue);
    // A wire value that maps to the reserved word `Self` is sanitized to a
    // valid identifier while keeping its wire form via the serde rename.
    assert_eq!(serde_json::to_value(Color::Self_).unwrap(), "self");
    let c: Color = serde_json::from_value(serde_json::json!("self")).unwrap();
    assert_eq!(c, Color::Self_);
}

#[test]
fn open_object_flattens_extra() {
    let json = serde_json::json!({ "id": "x", "other": 5 });
    let bag: Bag = serde_json::from_value(json).unwrap();
    assert_eq!(bag.id, "x");
    assert_eq!(bag.extra["other"], 5);
}

#[test]
fn generated_constructors_build_valid_structs() {
    // Required-only struct: `new` takes the one required field.
    let point = Point::new(1.0, 2.0);
    assert_eq!(point.x, 1.0);
    assert_eq!(point.y, 2.0);

    // Required + optional params struct: `new` takes only the required
    // field and defaults the optional one to `None`.
    let cfg = ConfigureParams::new("widget".into());
    assert_eq!(cfg.id, "widget");
    assert_eq!(cfg.label, None);
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json, serde_json::json!({ "id": "widget" }));

    // Empty params struct: niladic `new()`.
    let reset = ResetParams::new();
    let _: serde_json::Value = serde_json::to_value(&reset).unwrap();

    // Open object: `new` takes the required field and defaults the
    // flattened extra map.
    let bag = Bag::new("x".into());
    assert_eq!(bag.id, "x");
    assert!(bag.extra.is_empty());

    // Recursive struct with an optional Box field defaults to None.
    let tree = TreeNode::new("root".into());
    assert_eq!(tree.value, "root");
    assert!(tree.point.is_none());
    assert!(tree.parent.is_none());
    assert!(tree.children.is_none());
}

#[tokio::test]
async fn client_drives_transport() {
    // `root()` addresses members by bare method name (CDP-style channels).
    let client = ComExampleGraphClient::root(StubCaller);
    let tree = client
        .get_tree(GetTreeParams::new("abc".into()))
        .await
        .unwrap();
    assert_eq!(tree.value, "root");

    // Notifications resolve without a result.
    client
        .notify_changed(NotifyChangedParams {
            node: TreeNode::new("n".into()),
        })
        .await
        .unwrap();

    // Typed params + typed bool result.
    let painted = client
        .paint(PaintParams {
            shape: Shape::Point,
            color: Color::Red,
        })
        .await
        .unwrap();
    assert!(painted);

    // `raw_echo` uses the any/any fallback.
    let v = client
        .raw_echo(serde_json::json!({ "hi": true }))
        .await
        .unwrap();
    assert_eq!(v["hi"], true);

    // A params object with required + optional fields, built via its
    // generated `new` constructor (optional `label` defaults to `None`).
    let configured = client
        .configure(ConfigureParams::new("widget".into()))
        .await
        .unwrap();
    assert!(configured);

    // An empty params object, built via its niladic generated `new()`.
    client.reset(ResetParams::new()).await.unwrap();

    // Server notification: only an addressed-name accessor is generated (no
    // send method), and it respects the addressing prefix.
    assert_eq!(client.tree_changed_event_name(), "tree_changed");
    let prefixed = ComExampleGraphClient::with_prefix(StubCaller, "Graph.");
    assert_eq!(prefixed.tree_changed_event_name(), "Graph.tree_changed");
}

use hubrpc::prelude::{compute_interface_hash_value, normalize_json_schema};
use serde_json::{Map, Value, json};

const CODEGEN_KEY: &str = "x-hubrpc-codegen";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolSchemaError {
    #[error("invalid CDP protocol JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("expected {0}")]
    Missing(&'static str),
    #[error("duplicate CDP schema entry: {0}")]
    Duplicate(String),
    #[error("HubRPC schema normalization failed: {0}")]
    Normalize(String),
}

pub fn import_cdp_protocol(
    browser_protocol: &str,
    js_protocol: &str,
) -> Result<Value, ProtocolSchemaError> {
    let documents = [
        serde_json::from_str::<Value>(browser_protocol)?,
        serde_json::from_str::<Value>(js_protocol)?,
    ];
    let mut methods = Map::new();
    let mut schemas = Map::new();
    let mut domain_metadata = Map::new();

    for document in &documents {
        let domains = document["domains"]
            .as_array()
            .ok_or(ProtocolSchemaError::Missing("protocol domains array"))?;
        for domain in domains {
            let domain_name = required_str(domain, "domain")?;
            insert_unique(
                &mut domain_metadata,
                domain_name.to_owned(),
                select_fields(
                    domain,
                    &["description", "experimental", "deprecated", "dependencies"],
                ),
            )?;

            if let Some(types) = domain["types"].as_array() {
                for type_definition in types {
                    let type_name = required_str(type_definition, "id")?;
                    let mut schema = convert_schema(type_definition, domain_name);
                    apply_type_compatibility_overrides(domain_name, type_name, &mut schema);
                    insert_unique(
                        &mut schemas,
                        format!("{domain_name}.{type_name}"),
                        schema,
                    )?;
                }
            }

            if let Some(commands) = domain["commands"].as_array() {
                for command in commands {
                    let member_name = required_str(command, "name")?;
                    let wire_method = format!("{domain_name}.{member_name}");
                    insert_unique(
                        &mut methods,
                        wire_method.clone(),
                        method_schema(command, domain_name, &wire_method, "request", true),
                    )?;
                }
            }

            if let Some(events) = domain["events"].as_array() {
                for event in events {
                    let member_name = required_str(event, "name")?;
                    let wire_method = format!("{domain_name}.{member_name}");
                    insert_unique(
                        &mut methods,
                        wire_method.clone(),
                        method_schema(
                            event,
                            domain_name,
                            &wire_method,
                            "serverNotification",
                            false,
                        ),
                    )?;
                }
            }
        }
    }

    Ok(json!({
        "id": "cdp.protocol",
        "hash": "",
        "description": "Chrome DevTools Protocol imported as one root-addressed HubRPC compatibility interface.",
        "methods": methods,
        "components": {
            "schemas": schemas,
        },
        CODEGEN_KEY: {
            "profile": "cdp",
            "wireAddressing": "root",
            "sessionMultiplexing": "transport",
            "domains": domain_metadata,
            "sourceVersions": documents.iter().map(|document| document["version"].clone()).collect::<Vec<_>>(),
        },
    }))
}

pub fn compute_imported_interface_hash(interface: &Value) -> Result<String, ProtocolSchemaError> {
    let projection = interface_hash_projection(interface)?;
    Ok(compute_interface_hash_value(&projection))
}

pub fn import_typed_cdp_protocol(
    browser_protocol: &str,
    js_protocol: &str,
) -> Result<hubrpc::prelude::HubRpcInterfaceSchema, ProtocolSchemaError> {
    let mut interface = import_cdp_protocol(browser_protocol, js_protocol)?;
    let hash = compute_imported_interface_hash(&interface)?;
    interface["hash"] = Value::String(hash);
    Ok(serde_json::from_value(interface)?)
}

pub fn interface_hash_projection(interface: &Value) -> Result<Value, ProtocolSchemaError> {
    let mut projected = strip_codegen(interface);
    let object = projected
        .as_object_mut()
        .ok_or(ProtocolSchemaError::Missing("interface object"))?;
    object.remove("hash");

    let methods = object
        .get_mut("methods")
        .and_then(Value::as_object_mut)
        .ok_or(ProtocolSchemaError::Missing("interface methods object"))?;
    for method in methods.values_mut() {
        let method = method
            .as_object_mut()
            .ok_or(ProtocolSchemaError::Missing("method object"))?;
        normalize_schema_field(method, "params")?;
        normalize_schema_field(method, "result")?;
        normalize_schema_field(method, "clientStream")?;
        normalize_schema_field(method, "serverStream")?;
    }

    if let Some(schemas) = object
        .get_mut("components")
        .and_then(Value::as_object_mut)
        .and_then(|components| components.get_mut("schemas"))
        .and_then(Value::as_object_mut)
    {
        for schema in schemas.values_mut() {
            *schema = normalize_json_schema(schema)
                .map_err(|error| ProtocolSchemaError::Normalize(error.to_string()))?;
        }
    }

    Ok(projected)
}

fn method_schema(
    raw: &Value,
    domain_name: &str,
    wire_method: &str,
    kind: &str,
    has_result: bool,
) -> Value {
    let mut method = Map::new();
    let mut params = object_from_fields(
        raw["parameters"].as_array(),
        domain_name,
        compatibility_optional_fields(wire_method),
    );
    apply_compatibility_overrides(wire_method, &mut params);
    method.insert("params".into(), params);
    if has_result {
        method.insert(
            "result".into(),
            object_from_fields(raw["returns"].as_array(), domain_name, &[]),
        );
    }
    for key in ["description", "deprecated"] {
        if let Some(value) = raw.get(key) {
            method.insert(key.into(), value.clone());
        }
    }
    method.insert(
        CODEGEN_KEY.into(),
        json!({
            "profile": "cdp",
            "kind": kind,
            "wireMethod": wire_method,
            "domain": domain_name,
            "experimental": raw.get("experimental").cloned().unwrap_or(Value::Bool(false)),
            "redirect": raw.get("redirect").cloned().unwrap_or(Value::Null),
        }),
    );
    Value::Object(method)
}

fn compatibility_optional_fields(wire_method: &str) -> &'static [&'static str] {
    match wire_method {
        "Debugger.scriptParsed" | "Debugger.scriptFailedToParse" => &["buildId"],
        _ => &[],
    }
}

fn apply_compatibility_overrides(wire_method: &str, params: &mut Value) {
    if wire_method == "Debugger.paused" {
        params["properties"]["reason"]
            .as_object_mut()
            .expect("Debugger.paused.reason is an object schema")
            .remove("enum");
    }
}

fn apply_type_compatibility_overrides(domain: &str, name: &str, schema: &mut Value) {
    if domain == "Runtime" && name == "RemoteObject"
        && let Some(subtypes) = schema.pointer_mut("/properties/subtype/enum").and_then(Value::as_array_mut)
    {
        // V8 emits these internal-property subtypes but omits them from the public PDL.
        for subtype in ["internal#location", "internal#scope", "internal#scopeList", "internal#entry"] {
            let subtype = Value::String(subtype.into());
            if !subtypes.contains(&subtype) {
                subtypes.push(subtype);
            }
        }
    }
}

fn object_from_fields(
    fields: Option<&Vec<Value>>,
    domain_name: &str,
    compatibility_optional: &[&str],
) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in fields.into_iter().flatten() {
        let Some(name) = field["name"].as_str() else {
            continue;
        };
        let compatibility_optional = compatibility_optional.contains(&name);
        let schema = if compatibility_optional {
            let mut field = field.clone();
            field["optional"] = Value::Bool(true);
            convert_schema(&field, domain_name)
        } else {
            convert_schema(field, domain_name)
        };
        properties.insert(name.into(), schema);
        if field["optional"] != true && !compatibility_optional {
            required.push(Value::String(name.into()));
        }
    }

    let mut schema = Map::new();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("properties".into(), Value::Object(properties));
    schema.insert("additionalProperties".into(), Value::Bool(false));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    Value::Object(schema)
}

fn convert_schema(raw: &Value, current_domain: &str) -> Value {
    let mut schema = Map::new();
    if let Some(reference) = raw["$ref"].as_str() {
        let qualified = if reference.contains('.') {
            reference.to_owned()
        } else {
            format!("{current_domain}.{reference}")
        };
        schema.insert(
            "$ref".into(),
            Value::String(format!("#/components/schemas/{qualified}")),
        );
    } else {
        match raw["type"].as_str() {
            Some("object") => {
                schema.insert("type".into(), Value::String("object".into()));
                if let Some(properties) = raw["properties"].as_array() {
                    let object = object_from_fields(Some(properties), current_domain, &[]);
                    let object = object.as_object().unwrap();
                    schema.extend(object.clone());
                } else {
                    schema.insert("additionalProperties".into(), Value::Bool(true));
                }
            }
            Some("array") => {
                schema.insert("type".into(), Value::String("array".into()));
                schema.insert(
                    "items".into(),
                    convert_schema(&raw["items"], current_domain),
                );
            }
            Some("any") | None => {}
            Some(primitive) => {
                schema.insert("type".into(), Value::String(primitive.into()));
            }
        }
    }

    if let Some(type_name) = raw["id"].as_str() {
        schema.insert("title".into(), Value::String(type_name.into()));
    }
    for key in ["description", "enum"] {
        if let Some(value) = raw.get(key) {
            schema.insert(key.into(), value.clone());
        }
    }
    schema.insert(
        CODEGEN_KEY.into(),
        json!({
            "cdpType": raw.get("type").cloned().unwrap_or(Value::Null),
            "originalRef": raw.get("$ref").cloned().unwrap_or(Value::Null),
            "optional": raw.get("optional").cloned().unwrap_or(Value::Bool(false)),
            "experimental": raw.get("experimental").cloned().unwrap_or(Value::Bool(false)),
            "deprecated": raw.get("deprecated").cloned().unwrap_or(Value::Bool(false)),
        }),
    );
    Value::Object(schema)
}

fn normalize_schema_field(
    method: &mut Map<String, Value>,
    field: &str,
) -> Result<(), ProtocolSchemaError> {
    if let Some(schema) = method.get_mut(field) {
        *schema = normalize_json_schema(schema)
            .map_err(|error| ProtocolSchemaError::Normalize(error.to_string()))?;
    }
    Ok(())
}

fn strip_codegen(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(strip_codegen).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != CODEGEN_KEY)
                .map(|(key, value)| (key.clone(), strip_codegen(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn select_fields(value: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|key| value.get(*key).map(|value| ((*key).into(), value.clone())))
            .collect(),
    )
}

fn insert_unique(
    map: &mut Map<String, Value>,
    key: String,
    value: Value,
) -> Result<(), ProtocolSchemaError> {
    if map.insert(key.clone(), value).is_some() {
        return Err(ProtocolSchemaError::Duplicate(key));
    }
    Ok(())
}

fn required_str<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, ProtocolSchemaError> {
    value[field]
        .as_str()
        .ok_or(ProtocolSchemaError::Missing(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BROWSER_PROTOCOL: &str =
        include_str!("../../../node_modules/devtools-protocol/json/browser_protocol.json");
    const JS_PROTOCOL: &str =
        include_str!("../../../node_modules/devtools-protocol/json/js_protocol.json");

    fn imported() -> Value {
        import_cdp_protocol(BROWSER_PROTOCOL, JS_PROTOCOL).unwrap()
    }

    #[test]
    fn imports_the_complete_protocol_into_one_interface_document() {
        let interface = imported();
        let typed: hubrpc::prelude::HubRpcInterfaceSchema =
            serde_json::from_value(interface.clone()).unwrap();
        assert!(interface["methods"].as_object().unwrap().len() > 500);
        assert_eq!(
            typed.methods.len(),
            interface["methods"].as_object().unwrap().len()
        );
        assert!(
            interface["components"]["schemas"]
                .as_object()
                .unwrap()
                .len()
                > 500
        );
        assert!(interface["methods"]["Debugger.enable"].is_object());
        assert!(interface["methods"]["Debugger.scriptParsed"].is_object());
        assert_eq!(
            interface["methods"]["Debugger.scriptParsed"]["result"],
            Value::Null
        );
    }

    #[test]
    fn imports_a_hashed_typed_interface_for_generic_code_generation() {
        let typed = import_typed_cdp_protocol(BROWSER_PROTOCOL, JS_PROTOCOL).unwrap();
        assert_eq!(typed.id, "cdp.protocol");
        assert!(!typed.hash.is_empty());
        assert_eq!(hubrpc::prelude::compute_interface_hash(&typed), typed.hash);
        assert_eq!(typed.methods.len(), 896);
        assert_eq!(typed.components.unwrap().schemas.unwrap().len(), 607);
    }

    #[test]
    fn typed_interface_retains_hash_invisible_codegen_extensions() {
        let typed = import_typed_cdp_protocol(BROWSER_PROTOCOL, JS_PROTOCOL).unwrap();
        assert_eq!(
            typed
                .extension(CODEGEN_KEY)
                .and_then(|value| value["profile"].as_str()),
            Some("cdp")
        );
        assert_eq!(
            typed.methods["Debugger.scriptParsed"]
                .extension(CODEGEN_KEY)
                .and_then(|value| value["kind"].as_str()),
            Some("serverNotification")
        );
    }

    #[test]
    fn preserves_recursive_references_and_cdp_codegen_metadata() {
        let interface = imported();
        let node = &interface["components"]["schemas"]["DOM.Node"];
        assert_eq!(node["title"], "Node");
        assert_eq!(
            node["properties"]["children"]["items"]["$ref"],
            "#/components/schemas/DOM.Node"
        );
        assert_eq!(
            node["properties"]["children"]["items"][CODEGEN_KEY]["originalRef"],
            "Node"
        );
    }

    #[test]
    fn converts_cdp_optionality_to_json_schema_required() {
        let interface = imported();
        let params = &interface["methods"]["Debugger.setBreakpointByUrl"]["params"];
        assert_eq!(params["required"], json!(["lineNumber"]));
        assert!(params["properties"]["url"][CODEGEN_KEY]["optional"] == true);
    }

    #[test]
    fn tolerates_build_id_missing_from_older_script_events() {
        let interface = imported();
        for method in ["Debugger.scriptParsed", "Debugger.scriptFailedToParse"] {
            let params = &interface["methods"][method]["params"];
            assert!(
                !params["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("buildId"))
            );
            assert_eq!(
                params["properties"]["buildId"][CODEGEN_KEY]["optional"],
                true
            );
        }
    }

    #[test]
    fn accepts_node_specific_debugger_pause_reasons() {
        let interface = imported();
        let reason = &interface["methods"]["Debugger.paused"]["params"]["properties"]["reason"];
        assert_eq!(reason["type"], "string");
        assert_eq!(reason["enum"], Value::Null);
    }

    #[test]
    fn preserves_open_and_closed_object_semantics() {
        let interface = imported();
        let open = &interface["components"]["schemas"]["Network.Headers"];
        assert_eq!(open["type"], "object");
        assert_eq!(open["additionalProperties"], true);

        let closed = &interface["components"]["schemas"]["DOM.Node"];
        assert_eq!(closed["type"], "object");
        assert_eq!(closed["additionalProperties"], false);
    }

    #[test]
    fn rich_codegen_expressions_do_not_change_the_simple_interface_hash() {
        let interface = imported();
        let expected = compute_imported_interface_hash(&interface).unwrap();
        let mut changed = interface.clone();
        changed["methods"]["Debugger.enable"][CODEGEN_KEY]["generatorHint"] =
            Value::String("generate-a-more-specific-type".into());
        changed[CODEGEN_KEY]["recordingHint"] = Value::String("new".into());
        assert_eq!(compute_imported_interface_hash(&changed).unwrap(), expected);
        assert_eq!(
            compute_interface_hash_value(&interface),
            compute_interface_hash_value(&changed)
        );
    }

    #[test]
    fn rejects_duplicate_protocol_entries_instead_of_overwriting() {
        let duplicate = r#"{
            "version": { "major": "1", "minor": "3" },
            "domains": [
                { "domain": "Duplicate", "commands": [{ "name": "method" }] },
                { "domain": "Duplicate", "commands": [{ "name": "method" }] }
            ]
        }"#;
        assert!(matches!(
            import_cdp_protocol(duplicate, r#"{ "domains": [] }"#),
            Err(ProtocolSchemaError::Duplicate(entry)) if entry == "Duplicate"
        ));
    }
}

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cdp::{RuntimeInternalPropertyDescriptor, RuntimePropertyDescriptor};
use crate::debugger_driver::DebuggerDriver;
use crate::debugger_engine::{ScriptKey, SessionKey};
use crate::service_api::SourceLocation;
use crate::source_location::ResolvedSourcePosition;
use crate::source_view::Position;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectSourceSnapshot {
    pub locations: Vec<ObjectLocationSnapshot>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectLocationSnapshot {
    pub origin: String,
    pub kind: String,
    pub script_id: String,
    pub position: ResolvedSourcePosition,
}

impl ObjectSourceSnapshot {
    pub fn merge(&mut self, other: Self) {
        for location in other.locations {
            if !self.locations.contains(&location) {
                self.locations.push(location);
            }
        }
        for diagnostic in other.diagnostics {
            if !self.diagnostics.contains(&diagnostic) {
                self.diagnostics.push(diagnostic);
            }
        }
    }

    pub fn has_conflicting_locations(&self) -> bool {
        self.locations.iter().enumerate().any(|(index, left)| {
            self.locations[index + 1..].iter().any(|right| {
                left.kind == right.kind
                    && (left.script_id != right.script_id
                        || left.position.generated != right.position.generated
                        || (left.position.mapping == right.position.mapping
                            && left.position.resolved != right.position.resolved))
            })
        })
    }
}

pub(crate) fn unresolved_position(
    script_id: &str,
    position: Position,
    diagnostic: String,
) -> ResolvedSourcePosition {
    let generated = SourceLocation {
        source_url: format!("script:{script_id}"),
        line: position.line.saturating_add(1),
        column: position.column.saturating_add(1),
    };
    ResolvedSourcePosition {
        resolved: generated.clone(),
        generated,
        breadcrumb: None,
        mapping: "generated".into(),
        diagnostic: Some(diagnostic),
    }
}

/// A per-inspection budget: no per-property fan-out or persistent remote handles.
pub(crate) struct LiveSourceInspector {
    remaining: usize,
    deadline: Instant,
    cache: BTreeMap<String, ObjectSourceSnapshot>,
}

impl Default for LiveSourceInspector {
    fn default() -> Self {
        Self {
            remaining: 8,
            deadline: Instant::now() + Duration::from_millis(750),
            cache: BTreeMap::new(),
        }
    }
}

impl LiveSourceInspector {
    pub fn take_request(&mut self) -> bool {
        if self.remaining == 0 || self.remaining_time().is_zero() {
            return false;
        }
        self.remaining -= 1;
        true
    }

    pub fn remaining_time(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub async fn inspect(
        &mut self,
        driver: &mut DebuggerDriver,
        session: &SessionKey,
        object_id: &str,
        known: Option<(
            &[RuntimePropertyDescriptor],
            &[RuntimeInternalPropertyDescriptor],
        )>,
    ) -> ObjectSourceSnapshot {
        if let Some(cached) = self.cache.get(object_id) {
            return cached.clone();
        }
        let mut result = ObjectSourceSnapshot::default();
        let mut queue = VecDeque::from([(object_id.to_owned(), "function", false)]);
        let mut visited = BTreeSet::new();
        while let Some((id, kind, prototype)) = queue.pop_front() {
            if !visited.insert(id.clone()) {
                continue;
            }
            if prototype {
                if !self.take_request() {
                    result
                        .diagnostics
                        .push("live constructor lookup budget exhausted".into());
                    break;
                }
                let constructor = tokio::time::timeout(
                    self.remaining_time(),
                    driver.raw_cdp_request("Runtime.callFunctionOn", serde_json::json!({
                        "objectId": id,
                        "functionDeclaration": "function() { for (let p = this, n = 0; p && n < 4; p = Object.getPrototypeOf(p), n++) { const d = Object.getOwnPropertyDescriptor(p, 'constructor'); if (d) return Object.getOwnPropertyDescriptor(d, 'value')?.value; } }",
                        "objectGroup": "dbgjs-object-locations",
                        "throwOnSideEffect": true,
                        "silent": true,
                        "returnByValue": false,
                    })),
                ).await;
                match constructor {
                    Ok(Ok(value)) if value.get("exceptionDetails").is_none() => {
                        if value
                            .pointer("/result/type")
                            .and_then(serde_json::Value::as_str)
                            == Some("function")
                            && let Some(id) = value
                                .pointer("/result/objectId")
                                .and_then(serde_json::Value::as_str)
                        {
                            queue.push_back((id.to_owned(), "constructor", false));
                        }
                    }
                    Ok(Ok(_)) => result
                        .diagnostics
                        .push("constructor lookup was rejected by the side-effect guard".into()),
                    Ok(Err(error)) => result
                        .diagnostics
                        .push(format!("constructor lookup unavailable: {error:?}")),
                    Err(_) => result
                        .diagnostics
                        .push("constructor lookup timed out".into()),
                }
                continue;
            }
            let descriptors = if id == object_id {
                known.map(|(properties, internal)| (properties.to_vec(), internal.to_vec()))
            } else {
                None
            };
            let (_properties, internal) = if let Some(descriptors) = descriptors {
                descriptors
            } else {
                if self.remaining < 2 || self.remaining_time().is_zero() {
                    result
                        .diagnostics
                        .push("live location lookup budget exhausted".into());
                    break;
                }
                self.remaining -= 2;
                let owned = tokio::time::timeout(
                    self.remaining_time(),
                    driver.raw_cdp_request(
                        "Runtime.callFunctionOn",
                        serde_json::json!({
                            "objectId": id,
                            "functionDeclaration": "function() { return this; }",
                            "objectGroup": "dbgjs-object-locations",
                            "silent": true,
                            "returnByValue": false,
                        }),
                    ),
                )
                .await;
                let (owned_id, is_function) = match owned {
                    Ok(Ok(value)) => match value
                        .pointer("/result/objectId")
                        .and_then(serde_json::Value::as_str)
                    {
                        Some(id) => (
                            id.to_owned(),
                            value
                                .pointer("/result/type")
                                .and_then(serde_json::Value::as_str)
                                == Some("function"),
                        ),
                        None => {
                            result.diagnostics.push("live location lookup could not retain object in its temporary group".into());
                            continue;
                        }
                    },
                    Ok(Err(error)) => {
                        result
                            .diagnostics
                            .push(format!("live location lookup unavailable: {error:?}"));
                        continue;
                    }
                    Err(_) => {
                        result
                            .diagnostics
                            .push("live location lookup timed out".into());
                        break;
                    }
                };
                if !is_function {
                    // Avoid fetching arbitrary (possibly huge) data properties just to find a prototype.
                    match tokio::time::timeout(
                        self.remaining_time(),
                        driver.raw_cdp_request("Runtime.callFunctionOn", serde_json::json!({
                            "objectId": owned_id,
                            "functionDeclaration": "function() { return Object.getPrototypeOf(this); }",
                            "objectGroup": "dbgjs-object-locations",
                            "throwOnSideEffect": true,
                            "silent": true,
                            "returnByValue": false,
                        })),
                    ).await {
                        Ok(Ok(value)) if value.get("exceptionDetails").is_none() => {
                            if let Some(id) = value.pointer("/result/objectId").and_then(serde_json::Value::as_str) {
                                queue.push_back((id.to_owned(), "constructor", true));
                            }
                        }
                        Ok(Ok(_)) => result.diagnostics.push("prototype lookup was rejected by the side-effect guard".into()),
                        Ok(Err(error)) => result.diagnostics.push(format!("prototype lookup unavailable: {error:?}")),
                        Err(_) => result.diagnostics.push("prototype lookup timed out".into()),
                    }
                    continue;
                }
                match tokio::time::timeout(
                    self.remaining_time(),
                    driver.client().runtime().get_properties(
                        owned_id,
                        Some(true),
                        None,
                        Some(false),
                        Some(true),
                    ),
                )
                .await
                {
                    Ok(Ok(response)) if response.exception_details.is_none() => (
                        response.result,
                        response.internal_properties.unwrap_or_default(),
                    ),
                    Ok(Ok(response)) => {
                        result.diagnostics.push(format!(
                            "live location properties unavailable: {:?}",
                            response.exception_details
                        ));
                        continue;
                    }
                    Ok(Err(error)) => {
                        result
                            .diagnostics
                            .push(format!("live location properties unavailable: {error:?}"));
                        continue;
                    }
                    Err(_) => {
                        result
                            .diagnostics
                            .push("live location lookup timed out".into());
                        break;
                    }
                }
            };
            for property in &internal {
                if property.name == "[[FunctionLocation]]" {
                    match parse_function_location(property) {
                        Ok((script_id, position)) => {
                            let script = ScriptKey {
                                session: session.clone(),
                                script_id: script_id.clone(),
                            };
                            let mut acquisition_error = None;
                            if !self.remaining_time().is_zero() {
                                let control = crate::source_search::SearchControl::with_deadline(
                                    self.deadline,
                                );
                                match driver
                                    .acquire_script_source(script.clone(), Some(&control))
                                    .await
                                {
                                    Ok(()) => {}
                                    Err(error) => acquisition_error = Some(error.to_string()),
                                }
                            }
                            let mut resolved = driver
                                .resolve_generated_position(&script, position)
                                .unwrap_or_else(|| {
                                    unresolved_position(
                                        &script_id,
                                        position,
                                        "script source has not been resolved".into(),
                                    )
                                });
                            if let Some(error) = acquisition_error {
                                resolved.diagnostic = Some(error);
                            }
                            result.locations.push(ObjectLocationSnapshot {
                                origin: "live".into(),
                                kind: kind.into(),
                                script_id,
                                position: resolved,
                            });
                        }
                        Err(error) => result.diagnostics.push(error),
                    }
                }
            }
            if let Some(target) = internal
                .iter()
                .find(|p| p.name == "[[TargetFunction]]")
                .and_then(|p| p.value.as_ref())
                .and_then(|v| v.object_id.as_ref())
            {
                queue.push_back((
                    target.clone(),
                    if kind == "constructor" {
                        kind
                    } else {
                        "boundTarget"
                    },
                    false,
                ));
            } else if kind == "function"
                && result.locations.is_empty()
                && let Some(proto) = internal
                    .iter()
                    .find(|p| p.name == "[[Prototype]]")
                    .and_then(|p| p.value.as_ref())
                    .and_then(|v| v.object_id.as_ref())
            {
                queue.push_back((proto.clone(), "constructor", true));
            }
        }
        match driver
            .client()
            .runtime()
            .release_object_group("dbgjs-object-locations".into())
            .await
        {
            Ok(_) => {}
            Err(error) => result.diagnostics.push(format!(
                "failed to release location lookup objects: {error:?}"
            )),
        }
        self.cache.insert(object_id.to_owned(), result.clone());
        result
    }
}

fn parse_function_location(
    property: &RuntimeInternalPropertyDescriptor,
) -> Result<(String, Position), String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Location {
        script_id: String,
        line_number: u32,
        column_number: u32,
    }
    let value = property
        .value
        .as_ref()
        .and_then(|value| value.value.clone())
        .ok_or_else(|| "[[FunctionLocation]] has no location value".to_owned())?;
    let location: Location = serde_json::from_value(value)
        .map_err(|error| format!("invalid [[FunctionLocation]]: {error}"))?;
    Ok((
        location.script_id,
        Position {
            line: location.line_number,
            column: location.column_number,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::{RuntimeRemoteObject, RuntimeRemoteObjectType};

    fn evidence(origin: &str, line: u32) -> ObjectLocationSnapshot {
        ObjectLocationSnapshot {
            origin: origin.into(),
            kind: "function".into(),
            script_id: "7".into(),
            position: unresolved_position("7", Position { line, column: 0 }, "fixture".into()),
        }
    }

    #[test]
    fn object_sources_preserve_conflicting_evidence_without_duplicate_entries() {
        let mut source = ObjectSourceSnapshot {
            locations: vec![evidence("heapSnapshot", 4)],
            diagnostics: vec![],
        };
        let live = ObjectSourceSnapshot {
            locations: vec![evidence("live", 5)],
            diagnostics: vec!["fixture".into()],
        };
        source.merge(live.clone());
        source.merge(live);
        assert_eq!(source.locations.len(), 2);
        assert_eq!(source.diagnostics.len(), 1);
        assert!(source.has_conflicting_locations());
        source.locations[1].position = source.locations[0].position.clone();
        assert!(!source.has_conflicting_locations());
        source.locations[1].position.resolved.line += 1;
        assert!(source.has_conflicting_locations());
        source.locations[1].position = source.locations[0].position.clone();
        source.locations[1].kind = "constructor".into();
        source.locations[1].position.generated.line = 90;
        assert!(!source.has_conflicting_locations());
    }

    #[test]
    fn function_locations_validate_protocol_values_without_losing_zero_based_positions() {
        let mut property = RuntimeInternalPropertyDescriptor::new("[[FunctionLocation]]".into());
        let mut value = RuntimeRemoteObject::new(RuntimeRemoteObjectType::Object);
        value.value = Some(serde_json::json!({"scriptId":"7", "lineNumber":0, "columnNumber":12}));
        property.value = Some(value.clone());
        assert_eq!(
            parse_function_location(&property).unwrap(),
            (
                "7".into(),
                Position {
                    line: 0,
                    column: 12
                }
            )
        );
        value.value = Some(serde_json::json!({"scriptId":"7", "lineNumber":-1, "columnNumber":12}));
        property.value = Some(value);
        assert!(
            parse_function_location(&property)
                .unwrap_err()
                .contains("invalid")
        );
    }

    #[test]
    fn object_location_lookup_budget_is_bounded() {
        let mut inspector = LiveSourceInspector::default();
        for _ in 0..8 {
            assert!(inspector.take_request());
        }
        assert!(!inspector.take_request());
        let mut expired = LiveSourceInspector::default();
        expired.deadline = Instant::now() - Duration::from_secs(1);
        assert!(!expired.take_request());
    }

    #[test]
    fn function_locations_accept_v8_internal_subtypes() {
        let property: RuntimeInternalPropertyDescriptor =
            serde_json::from_value(serde_json::json!({
                "name": "[[FunctionLocation]]",
                "value": {
                    "type":"object", "subtype":"internal#location",
                    "value":{"scriptId":"7","lineNumber":0,"columnNumber":12},
                    "description":"Object"
                }
            }))
            .unwrap();
        assert_eq!(
            parse_function_location(&property).unwrap(),
            (
                "7".into(),
                Position {
                    line: 0,
                    column: 12
                }
            )
        );
        for subtype in ["internal#scope", "internal#scopeList", "internal#entry"] {
            let remote: RuntimeRemoteObject = serde_json::from_value(serde_json::json!({
                "type":"object", "subtype":subtype, "objectId":"test",
            }))
            .unwrap();
            assert_eq!(serde_json::to_value(remote).unwrap()["subtype"], subtype);
        }
    }
}

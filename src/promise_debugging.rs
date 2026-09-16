use crate::cdp::{RuntimeInternalPropertyDescriptor, RuntimeRemoteObject};
use crate::heap_graph::{AnalysisError, HeapGraph, NodeIndex};
use crate::service_api::{
    PromiseClassification, PromiseOrigin, PromiseSnapshot, PromiseState, ValuePreviewSnapshot,
};

pub const DEFAULT_VALUE_PREVIEW_LENGTH: u32 = 120;
pub const DEFAULT_PROMISE_PREVIEW_LENGTH: u32 = DEFAULT_VALUE_PREVIEW_LENGTH;
pub const DEFAULT_PROMISE_LIMIT: u32 = 100;

const PROMISE_STATE_PROPERTIES: [&str; 2] = ["[[PromiseState]]", "[[PromiseStatus]]"];
const PROMISE_RESULT_PROPERTIES: [&str; 2] = ["[[PromiseResult]]", "[[PromiseValue]]"];

pub fn inspect_live_promise(
    object_id: String,
    internal_properties: Vec<RuntimeInternalPropertyDescriptor>,
    max_preview_length: u32,
) -> PromiseSnapshot {
    let state_property = internal_properties
        .iter()
        .find(|property| PROMISE_STATE_PROPERTIES.contains(&property.name.as_str()));
    let state = state_property
        .and_then(|property| property.value.as_ref())
        .and_then(remote_object_text)
        .map(promise_state)
        .unwrap_or(PromiseState::Unknown);
    let result_property = internal_properties
        .iter()
        .find(|property| PROMISE_RESULT_PROPERTIES.contains(&property.name.as_str()));
    let settlement = matches!(state, PromiseState::Fulfilled | PromiseState::Rejected)
        .then(|| {
            result_property
                .and_then(|property| property.value.as_ref())
                .map(|value| remote_value_snapshot(value, max_preview_length))
        })
        .flatten();

    PromiseSnapshot {
        reference: Some(object_id),
        origin: PromiseOrigin::Live,
        state,
        settlement,
        retained: None,
        classification: PromiseClassification::Indeterminate,
    }
}

pub fn has_live_promise_evidence(
    internal_properties: &[RuntimeInternalPropertyDescriptor],
) -> bool {
    internal_properties
        .iter()
        .any(|property| PROMISE_STATE_PROPERTIES.contains(&property.name.as_str()))
}

pub fn inspect_heap_promises(
    graph: &HeapGraph,
    capture_id: &str,
    state_filter: Option<PromiseState>,
    limit: u32,
    max_preview_length: u32,
) -> Result<(Vec<PromiseSnapshot>, u64), AnalysisError> {
    let dominators = graph.dominators()?;
    let mut promises = Vec::new();
    let mut total_promises = 0_u64;
    for summary in graph.node_summaries() {
        let summary = summary?;
        if !is_promise_node(summary.node_type, summary.raw_name) {
            continue;
        }
        let retained = dominators.immediate_dominator(summary.index).is_some();
        if !retained {
            continue;
        }

        let mut state = PromiseState::Unknown;
        let mut settlement = None;
        for reference in graph.outgoing_references(summary.index)? {
            let Some(name) = reference.name else {
                continue;
            };
            if PROMISE_STATE_PROPERTIES.contains(&name) {
                let target = graph.node_summary(reference.target)?;
                let observed = target.string_value.unwrap_or(target.raw_name);
                state = promise_state(observed);
            } else if PROMISE_RESULT_PROPERTIES.contains(&name) {
                settlement = Some(heap_value_snapshot(
                    graph,
                    capture_id,
                    reference.target,
                    max_preview_length,
                )?);
            }
        }
        if state_filter.is_some_and(|filter| filter != state) {
            continue;
        }
        if !matches!(state, PromiseState::Fulfilled | PromiseState::Rejected) {
            settlement = None;
        }
        total_promises = total_promises.saturating_add(1);
        if promises.len() < limit as usize {
            promises.push(PromiseSnapshot {
                reference: Some(format!("{capture_id}#{}", summary.heap_object_id)),
                origin: PromiseOrigin::HeapSnapshot,
                state,
                settlement,
                retained: Some(true),
                classification: PromiseClassification::Indeterminate,
            });
        }
    }
    Ok((promises, total_promises))
}

fn is_promise_node(node_type: &str, name: &str) -> bool {
    matches!(node_type, "object" | "native")
        && matches!(name, "Promise" | "JSPromise" | "system / JSPromise")
}

fn promise_state(value: &str) -> PromiseState {
    match value {
        "pending" => PromiseState::Pending,
        "fulfilled" => PromiseState::Fulfilled,
        "rejected" => PromiseState::Rejected,
        _ => PromiseState::Unknown,
    }
}

fn remote_object_text(value: &RuntimeRemoteObject) -> Option<&str> {
    value
        .value
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .or(value.description.as_deref())
}

pub fn remote_value_snapshot(
    value: &RuntimeRemoteObject,
    max_preview_length: u32,
) -> ValuePreviewSnapshot {
    let kind = serde_json::to_value(&value.r#type)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    let preview = value
        .value
        .as_ref()
        .and_then(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .or_else(|| serde_json::to_string(value).ok())
        })
        .or_else(|| value.unserializable_value.clone())
        .or_else(|| value.description.clone());
    let (preview, truncated) = preview
        .map(|preview| bounded_text(&preview, max_preview_length))
        .map_or((None, false), |(preview, truncated)| {
            (Some(preview), truncated)
        });
    ValuePreviewSnapshot {
        kind,
        preview,
        truncated,
        reference: value.object_id.clone(),
        source: Default::default(),
    }
}

fn heap_value_snapshot(
    graph: &HeapGraph,
    capture_id: &str,
    node: NodeIndex,
    max_preview_length: u32,
) -> Result<ValuePreviewSnapshot, AnalysisError> {
    let summary = graph.node_summary(node)?;
    let preview = summary.string_value.unwrap_or(summary.raw_name);
    let (preview, truncated) = bounded_text(preview, max_preview_length);
    Ok(ValuePreviewSnapshot {
        kind: summary.node_type.to_owned(),
        preview: Some(preview),
        truncated,
        reference: Some(format!("{capture_id}#{}", summary.heap_object_id)),
        source: Default::default(),
    })
}

fn bounded_text(value: &str, max_length: u32) -> (String, bool) {
    let max_length = max_length as usize;
    let end = value
        .char_indices()
        .nth(max_length)
        .map_or(value.len(), |(index, _)| index);
    (value[..end].to_owned(), end < value.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::{RuntimeRemoteObject, RuntimeRemoteObjectType};
    use crate::heap_graph::parse_heap_graph;

    fn internal(name: &str, value: RuntimeRemoteObject) -> RuntimeInternalPropertyDescriptor {
        let mut property = RuntimeInternalPropertyDescriptor::new(name.to_owned());
        property.value = Some(value);
        property
    }

    fn string_value(value: &str) -> RuntimeRemoteObject {
        let mut object = RuntimeRemoteObject::new(RuntimeRemoteObjectType::String);
        object.value = Some(serde_json::Value::String(value.to_owned()));
        object
    }

    #[test]
    fn classifies_live_states_from_engine_evidence() {
        for (raw, expected) in [
            ("pending", PromiseState::Pending),
            ("fulfilled", PromiseState::Fulfilled),
            ("rejected", PromiseState::Rejected),
            ("future-engine-state", PromiseState::Unknown),
        ] {
            let snapshot = inspect_live_promise(
                "live:1".to_owned(),
                vec![internal("[[PromiseState]]", string_value(raw))],
                20,
            );
            assert_eq!(snapshot.state, expected);
            assert_eq!(
                snapshot.classification,
                PromiseClassification::Indeterminate
            );
            assert!(
                serde_json::to_value(&snapshot)
                    .unwrap()
                    .get("evidence")
                    .is_none()
            );
        }
    }

    #[test]
    fn bounds_live_settlement_previews_without_losing_reference() {
        let mut reason = string_value("rejection message");
        reason.object_id = Some("reason:1".to_owned());
        let snapshot = inspect_live_promise(
            "promise:1".to_owned(),
            vec![
                internal("[[PromiseState]]", string_value("rejected")),
                internal("[[PromiseResult]]", reason),
            ],
            9,
        );
        assert_eq!(
            snapshot.settlement.as_ref().unwrap().preview.as_deref(),
            Some("rejection")
        );
        assert_eq!(
            snapshot.settlement.as_ref().unwrap().reference.as_deref(),
            Some("reason:1")
        );
    }

    #[test]
    fn missing_or_unrecognized_evidence_stays_unknown() {
        let snapshot = inspect_live_promise("live:2".to_owned(), Vec::new(), 20);
        assert_eq!(snapshot.state, PromiseState::Unknown);
        assert!(snapshot.settlement.is_none());
    }

    #[test]
    fn analyzes_retained_rejected_and_pending_promises_offline() {
        let graph = parse_heap_graph(
            snapshot_fixture(
                "0,0,1,0,2, 1,1,3,4,2, 2,2,5,7,0, 1,1,7,4,1, 2,3,9,12,0, 1,1,11,4,1",
                "0,4,5, 0,5,15, 0,6,10, 0,4,15, 0,6,20, 0,4,25",
                r#""root","Promise","rejected","pending","cached","inflight","[[PromiseState]]""#,
            )
            .as_bytes(),
        )
        .unwrap();

        let (rejected, rejected_total) =
            inspect_heap_promises(&graph, "offline", Some(PromiseState::Rejected), 10, 20).unwrap();
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected_total, 1);
        assert_eq!(rejected[0].reference.as_deref(), Some("offline#3"));
        assert_eq!(rejected[0].retained, Some(true));

        let (pending, pending_total) =
            inspect_heap_promises(&graph, "offline", Some(PromiseState::Pending), 10, 20).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending_total, 1);
        assert_eq!(pending[0].reference.as_deref(), Some("offline#7"));

        let (bounded, total) = inspect_heap_promises(&graph, "offline", None, 1, 20).unwrap();
        assert_eq!(bounded.len(), 1);
        assert_eq!(total, 2);
    }

    fn snapshot_fixture(nodes: &str, edges: &str, strings: &str) -> String {
        let node_count = nodes.split(',').count() / 5;
        let edge_count = edges.split(',').count() / 3;
        format!(
            r#"{{
              "snapshot": {{
                "meta": {{
                  "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                  "node_types": [["synthetic", "object", "string"], "string", "number", "number", "number"],
                  "edge_fields": ["type", "name_or_index", "to_node"],
                  "edge_types": [["property", "element", "weak"], "string_or_number", "node"],
                  "location_fields": ["object_index", "script_id", "line", "column"]
                }},
                "node_count": {node_count},
                "edge_count": {edge_count}
              }},
              "nodes": [{nodes}],
              "edges": [{edges}],
              "locations": [],
              "strings": [{strings}]
            }}"#
        )
    }
}

//! Bounded, snapshot-only descriptions. No live evaluation or accessor calls.

use crate::capture::heap::heap_graph::{AnalysisError, HeapGraph, NodeIndex};
use crate::api::value::{DescribedProperty, ValueDescription};

const STRING_CHARS: usize = 20;
const NAME_CHARS: usize = 32;
const MAX_PROPERTIES: usize = 3;
const MAX_SCANNED_EDGES: usize = 64;
const MAX_PREVIEW_CHARS: usize = 512;

/// Heap edges describe captured data, not the current state of a materialized live object.
pub(crate) fn heap_description(graph: &HeapGraph, node: NodeIndex, capture_id: Option<&str>) -> Result<ValueDescription, AnalysisError> {
    let mut value = heap_atom(graph, node, capture_id)?;
    value.origin = Some("heapSnapshot".into());
    if !matches!(value.kind.as_str(), "object" | "array") { return Ok(value); }
    let mut scanned = 0;
    for edge in graph.outgoing_references(node)?.take(MAX_SCANNED_EDGES) {
        scanned += 1;
        if !matches!(edge.edge_type, "property" | "element") || edge.name == Some("__proto__") { continue; }
        if value.properties.len() == MAX_PROPERTIES { value.truncated = true; break; }
        if graph.node_summary(edge.target)?.raw_name == "system / AccessorPair" {
            continue;
        }
        let child = heap_atom(graph, edge.target, capture_id)?;
        value.properties.push(DescribedProperty {
            name: edge.name.map(|name| name.chars().take(STRING_CHARS).collect()).unwrap_or_else(|| edge.name_or_index.to_string()),
            value: child,
        });
    }
    value.truncated |= scanned == MAX_SCANNED_EDGES && graph.node_summary(node)?.outgoing_references > scanned;
    // A heap snapshot omits some immediate primitives and array holes. Never claim exact JSON.
    value.incomplete = true;
    Ok(value)
}

fn heap_atom(graph: &HeapGraph, node: NodeIndex, capture_id: Option<&str>) -> Result<ValueDescription, AnalysisError> {
    let summary = graph.node_summary(node)?;
    let kind = match summary.node_type {
        "closure" => "function",
        "object" if summary.raw_name == "Array" => "array",
        "concatenated string" | "sliced string" => "string",
        other => other,
    };
    let mut value = ValueDescription::new(kind);
    value.summary = Some(atom(graph, node)?);
    value.reference = capture_id.map(|capture| format!("{capture}#{}", summary.heap_object_id));
    value.identity = value.reference.clone();
    if let Some(text) = graph.reconstructed_string(node, Some(STRING_CHARS))? {
        value.value = Some(serde_json::Value::String(text.value));
        value.truncated = text.truncated || !text.exact_prefix;
    } else if matches!(kind, "object" | "array") {
        value.incomplete = true;
    }
    Ok(value)
}

pub(crate) fn heap_preview(graph: &HeapGraph, node: NodeIndex) -> Result<String, AnalysisError> {
    Ok(heap_description(graph, node, None)?.inline(MAX_PREVIEW_CHARS))
}

fn atom(graph: &HeapGraph, node: NodeIndex) -> Result<String, AnalysisError> {
    let summary = graph.node_summary(node)?;
    if let Some(value) = graph.reconstructed_string(node, Some(STRING_CHARS))? {
        let text = escaped(&value.value, STRING_CHARS, value.truncated);
        return Ok(if value.exact_prefix {
            format!("\"{text}\"")
        } else {
            format!("~\"{text}\"")
        });
    }
    let name = escaped(summary.raw_name, NAME_CHARS, false);
    Ok(match summary.node_type {
        "closure" => format!(
            "function {}",
            if name.is_empty() {
                "(anonymous)"
            } else {
                &name
            }
        ),
        "object" if summary.raw_name == "Array" => "Array [...]".to_owned(),
        "object" => format!("{name} {{...}}"),
        "array" => "Array [...]".to_owned(),
        _ if name.is_empty() => escaped(summary.node_type, NAME_CHARS, false),
        _ => name,
    })
}

fn escaped(value: &str, max_chars: usize, truncated: bool) -> String {
    let mut chars = value.chars();
    let mut result = chars
        .by_ref()
        .take(max_chars)
        .flat_map(char::escape_default)
        .collect::<String>();
    if truncated || chars.next().is_some() {
        result.push_str("...");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph(nodes: &[[usize; 5]], edges: &[[usize; 3]], strings: &[&str]) -> HeapGraph {
        let snapshot = json!({
            "snapshot": {
                "meta": {
                    "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                    "node_types": [["object", "string", "closure", "array", "hidden", "number", "concatenated string", "sliced string"], "string", "number", "number", "number"],
                    "edge_fields": ["type", "name_or_index", "to_node"],
                    "edge_types": [["property", "element", "internal", "weak"], "string_or_number", "node"]
                },
                "node_count": nodes.len(),
                "edge_count": edges.len()
            },
            "nodes": nodes.iter().flatten().collect::<Vec<_>>(),
            "edges": edges.iter().flatten().collect::<Vec<_>>(),
            "strings": strings
        });
        HeapGraph::parse(snapshot.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn heap_preview_strings_escape_and_truncate_unicode_safely() {
        let graph = graph(&[[1, 0, 1, 0, 0]], &[], &["😀\n\"abcdefghijklmnopqrstu"]);
        assert_eq!(
            heap_preview(&graph, NodeIndex(0)).unwrap(),
            "\"\\u{1f600}\\n\\\"abcdefghijklmnopq...\""
        );
    }

    #[test]
    fn heap_preview_array_names_do_not_override_the_node_type() {
        let graph = graph(
            &[[1, 0, 1, 0, 0], [2, 0, 3, 0, 0], [2, 1, 5, 0, 0]],
            &[],
            &["Array", ""],
        );
        assert_eq!(heap_preview(&graph, NodeIndex(0)).unwrap(), "\"Array\"");
        assert_eq!(
            heap_preview(&graph, NodeIndex(1)).unwrap(),
            "function Array"
        );
        assert_eq!(
            heap_preview(&graph, NodeIndex(2)).unwrap(),
            "function (anonymous)"
        );
    }

    #[test]
    fn heap_preview_objects_are_shallow_and_skip_accessors_and_prototypes() {
        let graph = graph(
            &[
                [0, 0, 1, 0, 5],
                [1, 1, 3, 0, 0],
                [4, 2, 5, 0, 0],
                [2, 3, 7, 0, 0],
            ],
            &[[0, 4, 5], [0, 5, 0], [0, 6, 10], [0, 7, 0], [0, 8, 15]],
            &[
                "Widget",
                "hello",
                "system / AccessorPair",
                "run",
                "title",
                "__proto__",
                "getter",
                "self",
                "method",
            ],
        );
        assert_eq!(
            heap_preview(&graph, NodeIndex(0)).unwrap(),
            "Widget {\"title\": \"hello\", \"self\": Widget {...}, \"method\": function run}"
        );
    }

    #[test]
    fn heap_preview_arrays_show_only_a_few_elements() {
        let graph = graph(
            &[[0, 0, 1, 0, 4], [5, 1, 3, 0, 0]],
            &[[1, 0, 5], [1, 2, 5], [1, 4, 5], [1, 6, 5]],
            &["Array", "42"],
        );
        assert_eq!(
            heap_preview(&graph, NodeIndex(0)).unwrap(),
            "Array [0: 42, 2: 42, 4: 42, ...]"
        );
    }

    #[test]
    fn heap_preview_bounds_property_scans_even_for_internal_edges() {
        let edges = vec![[2, 1, 0]; MAX_SCANNED_EDGES + 1];
        let graph = graph(&[[0, 0, 1, 0, edges.len()]], &edges, &["Object", "hidden"]);
        assert_eq!(heap_preview(&graph, NodeIndex(0)).unwrap(), "Object {...}");
    }

    #[test]
    fn heap_preview_bounds_escaped_output_and_function_names() {
        let long = "😀".repeat(1000);
        let graph = graph(
            &[[0, 0, 1, 0, 3], [2, 1, 3, 0, 0]],
            &[[0, 1, 5]; 3],
            &["Object", &long],
        );
        let preview = heap_preview(&graph, NodeIndex(0)).unwrap();
        assert!(preview.chars().count() <= MAX_PREVIEW_CHARS);
        assert!(preview.ends_with("...}"));
        assert!(
            heap_preview(&graph, NodeIndex(1))
                .unwrap()
                .starts_with("function \\u{1f600}")
        );
    }

    #[test]
    fn heap_preview_reconstructs_concatenated_strings() {
        let graph = graph(
            &[[6, 0, 1, 0, 2], [1, 1, 3, 0, 0], [1, 2, 5, 0, 0]],
            &[[2, 3, 5], [2, 4, 10]],
            &[
                "(concatenated string)",
                "hello ",
                "world",
                "first",
                "second",
            ],
        );
        assert_eq!(
            heap_preview(&graph, NodeIndex(0)).unwrap(),
            "\"hello world\""
        );
    }

    #[test]
    fn heap_preview_marks_uncertain_slices_and_terminates_string_cycles() {
        let graph = graph(
            &[[7, 0, 1, 0, 1], [1, 1, 3, 0, 0], [6, 2, 5, 0, 2]],
            &[[2, 3, 5], [2, 4, 10], [2, 5, 10]],
            &[
                "(sliced string)",
                "backing text",
                "(concatenated string)",
                "parent",
                "first",
                "second",
            ],
        );
        assert_eq!(
            heap_preview(&graph, NodeIndex(0)).unwrap(),
            "~\"backing text...\""
        );
        let cyclic = heap_preview(&graph, NodeIndex(2)).unwrap();
        assert!(cyclic.contains("..."));
        assert!(cyclic.chars().count() <= MAX_PREVIEW_CHARS);
    }
}

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use crate::service::context_source_model::{ContextSourceModel, SourceContributionId};
use crate::capture::heap::heap_graph::{AnalysisError, HeapGraph, NodeIndex};
use crate::debugger::object_inspection::{ObjectLocationSnapshot, ObjectSourceSnapshot, unresolved_position};
use crate::api::service_api::{HeapMappingSnapshot, HeapScriptSnapshot};
use crate::source::source_view::{
    GeneratedSourceInput, Position, ResolutionPolicy, ResolvedSourceView, appears_minified,
};

#[derive(Default)]
pub(crate) struct HeapSourceResolver {
    scripts: BTreeMap<String, Result<ResolvedSourceView, String>>,
    symbol_indexes: BTreeMap<String, crate::source::source_location::SymbolIndexCache>,
}

impl HeapSourceResolver {
    pub fn inspect(
        &mut self,
        graph: &HeapGraph,
        mapping: &HeapMappingSnapshot,
        node: NodeIndex,
    ) -> Result<ObjectSourceSnapshot, AnalysisError> {
        let mut result = ObjectSourceSnapshot::default();
        if !matches!(graph.node_summary(node)?.node_type, "closure" | "object") {
            return Ok(result);
        }
        let root_kind = if graph.node_summary(node)?.node_type == "closure" {
            "function"
        } else {
            "constructor"
        };
        let mut queue = VecDeque::from([(node, root_kind, false)]);
        let mut visited = BTreeSet::new();
        let mut remaining_edges = 64;
        while let Some((node, kind, prototype)) = queue.pop_front() {
            if !visited.insert(node) {
                continue;
            }
            if visited.len() > 4 {
                result
                    .diagnostics
                    .push("heap location traversal limit reached".into());
                break;
            }
            for location in graph.locations_for_node(node)? {
                let script_id = location.script_id.to_string();
                let position = Position {
                    line: location.line,
                    column: location.column,
                };
                let resolved = self.resolve(mapping, &script_id, position);
                result.locations.push(ObjectLocationSnapshot {
                    origin: "heapSnapshot".into(),
                    kind: kind.into(),
                    script_id,
                    position: resolved,
                });
            }
            let mut bound_target = None;
            let mut object_prototype = None;
            let mut constructor = None;
            for reference in graph.outgoing_references(node)? {
                if remaining_edges == 0 {
                    result
                        .diagnostics
                        .push("heap location edge scan limit reached".into());
                    break;
                }
                remaining_edges -= 1;
                match (reference.edge_type, reference.name) {
                    ("internal", Some("bound_function")) => bound_target = Some(reference.target),
                    ("property", Some("__proto__")) if result.locations.is_empty() => {
                        object_prototype = Some(reference.target);
                    }
                    ("property", Some("constructor")) if prototype => {
                        if graph.node_summary(reference.target)?.node_type == "closure" {
                            constructor = Some(reference.target);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(target) = bound_target {
                queue.push_back((
                    target,
                    if kind == "constructor" {
                        kind
                    } else {
                        "boundTarget"
                    },
                    false,
                ));
            } else if let Some(constructor) = constructor {
                queue.push_back((constructor, "constructor", false));
            } else if let Some(prototype) = object_prototype {
                queue.push_back((prototype, "constructor", true));
            }
        }
        Ok(result)
    }

    pub fn resolve(
        &mut self,
        mapping: &HeapMappingSnapshot,
        script_id: &str,
        position: Position,
    ) -> crate::source::source_location::ResolvedSourcePosition {
        let Some(script) = mapping
            .scripts
            .iter()
            .find(|script| script.script_id == script_id)
        else {
            return unresolved_position(
                script_id,
                position,
                "capture has no script mapping metadata".into(),
            );
        };
        let view = self
            .scripts
            .entry(script_id.to_owned())
            .or_insert_with(|| stored_source_view(script));
        match view {
            Ok(view) => {
                let mut resolved = crate::source::source_location::resolve_source_position(
                    view,
                    &script.url,
                    script.source_map_url.as_deref(),
                    position,
                    self.symbol_indexes.entry(script_id.to_owned()).or_default(),
                );
                if let Some(diagnostic) = &script.diagnostic {
                    resolved.diagnostic = Some(diagnostic.clone());
                }
                resolved
            }
            Err(error) => {
                let mut resolved = unresolved_position(
                    script_id,
                    position,
                    script.diagnostic.as_ref().unwrap_or(error).clone(),
                );
                resolved.generated.source_url = script.url.clone();
                resolved.resolved = resolved.generated.clone();
                resolved
            }
        }
    }
}

fn stored_source_view(script: &HeapScriptSnapshot) -> Result<ResolvedSourceView, String> {
    if script.source_map.is_none() {
        if let Some(url) = &script.source_map_url {
            return Err(format!(
                "source map '{url}' unavailable for this view; generated location retained"
            ));
        }
    }
    if let Some(map) = &script.source_map {
        sourcemap::decode_slice(map.as_bytes())
            .map_err(|error| format!("invalid source map: {error}"))?;
    }
    let content = script.generated_source.as_deref().unwrap_or("");
    let mut view = ResolvedSourceView::new(
        ResolutionPolicy::PreferSourcesContent,
        Arc::new(ContextSourceModel::new()),
        SourceContributionId::new("heap-object-locations"),
        BTreeMap::new(),
    );
    view.add_generated(GeneratedSourceInput {
        url: &script.url,
        content,
        source_map: script.source_map.as_ref().map(|map| map.as_bytes()),
        source_map_url: script.source_map_url.as_deref(),
        minified: script.source_map.is_none() && appears_minified(&script.url, content),
    })
    .map_err(|error| error.to_string())?;
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::heap::heap_graph::parse_heap_graph;
    use crate::api::service_api::HeapMappingStatus;

    fn mapping() -> HeapMappingSnapshot {
        HeapMappingSnapshot {
            connection_generation: 1,
            hydration_duration_micros: 0,
            scripts: vec![HeapScriptSnapshot {
                script_id: "7".into(),
                url: "https://example.test/assets/app.min.js".into(),
                hash: "fixture".into(),
                provenance: Default::default(),
                source_map_url: Some("https://example.test/assets/app.min.js.map".into()),
                generated_source: Some("function a() { return 1; }".into()),
                source_map: Some(serde_json::json!({
                    "version":3, "file":"app.min.js", "sources":["../src/deep/path/models/provider.ts"],
                    "sourcesContent":["function provideModels() { return 1; }"], "names":[], "mappings":"AAAA"
                }).to_string()),
                mapping_status: HeapMappingStatus::Mapped,
                diagnostic: None,
            }],
        }
    }

    fn bound_graph() -> HeapGraph {
        let data = serde_json::json!({
            "snapshot": {
                "meta": {
                    "node_fields":["type","name","id","self_size","edge_count"],
                    "node_types":[["closure"],"string","number","number","number"],
                    "edge_fields":["type","name_or_index","to_node"],
                    "edge_types":[["internal"],"string_or_number","node"],
                    "location_fields":["object_index","script_id","line","column"]
                },
                "node_count":2,"edge_count":1
            },
            "nodes":[0,0,1,24,1, 0,1,3,24,0],
            "edges":[0,2,5],
            "strings":["native_bind","a","bound_function"],
            "locations":[5,7,0,0]
        });
        parse_heap_graph(data.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn heap_object_sources_follow_bound_functions_and_resolve_full_authored_paths() {
        let graph = bound_graph();
        let mut resolver = HeapSourceResolver::default();
        let source = resolver.inspect(&graph, &mapping(), NodeIndex(0)).unwrap();
        assert_eq!(source.locations.len(), 1);
        let location = &source.locations[0];
        assert_eq!(location.kind, "boundTarget");
        assert_eq!(location.origin, "heapSnapshot");
        assert_eq!(
            location.position.resolved.source_url,
            "https://example.test/src/deep/path/models/provider.ts"
        );
        assert_eq!(
            location.position.generated.source_url,
            "https://example.test/assets/app.min.js"
        );
        assert_eq!(location.position.resolved.line, 1);
        assert_eq!(
            location.position.breadcrumb.as_deref(),
            Some("provideModels")
        );
        assert_eq!(resolver.scripts.len(), 1);
        resolver.inspect(&graph, &mapping(), NodeIndex(0)).unwrap();
        assert_eq!(resolver.scripts.len(), 1);
    }

    #[test]
    fn heap_object_sources_keep_raw_positions_when_mapping_is_missing_or_invalid() {
        let graph = bound_graph();
        let mut missing = mapping();
        missing.scripts.clear();
        let source = HeapSourceResolver::default()
            .inspect(&graph, &missing, NodeIndex(1))
            .unwrap();
        assert_eq!(source.locations[0].position.resolved.source_url, "script:7");
        assert!(source.locations[0].position.diagnostic.is_some());
        let mut unavailable = mapping();
        unavailable.scripts[0].source_map = None;
        unavailable.scripts[0].generated_source = None;
        unavailable.scripts[0].mapping_status = HeapMappingStatus::NotAttempted;
        let source = HeapSourceResolver::default()
            .inspect(&graph, &unavailable, NodeIndex(1))
            .unwrap();
        assert_eq!(
            source.locations[0].position.resolved.source_url,
            unavailable.scripts[0].url
        );
        assert!(source.locations[0].position.diagnostic
            .as_deref().unwrap().contains("unavailable"));
        let mut invalid = mapping();
        invalid.scripts[0].source_map = Some("{invalid".into());
        let source = HeapSourceResolver::default()
            .inspect(&graph, &invalid, NodeIndex(1))
            .unwrap();
        assert_eq!(
            source.locations[0].position.resolved.source_url,
            invalid.scripts[0].url
        );
        assert!(
            source.locations[0]
                .position
                .diagnostic
                .as_ref()
                .unwrap()
                .contains("invalid")
        );
    }

    #[test]
    fn heap_object_sources_map_even_without_authored_content() {
        let mut mapping = mapping();
        let mut map: serde_json::Value =
            serde_json::from_str(mapping.scripts[0].source_map.as_ref().unwrap()).unwrap();
        map.as_object_mut().unwrap().remove("sourcesContent");
        mapping.scripts[0].source_map = Some(map.to_string());
        let source = HeapSourceResolver::default()
            .inspect(&bound_graph(), &mapping, NodeIndex(1))
            .unwrap();
        assert_eq!(
            source.locations[0].position.resolved.source_url,
            "https://example.test/src/deep/path/models/provider.ts"
        );
        assert!(source.locations[0].position.breadcrumb.is_none());
    }
}

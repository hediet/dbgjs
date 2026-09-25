use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::service_api::SourceLocation;
pub use crate::source_effects::SymbolIndexCache;
use crate::source_view::{Position, ResolvedSourceView, SourceKind, canonical_source_uri};

/// Best available source coordinates. Both locations use 1-based lines and
/// UTF-16 columns; URLs are never shortened for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSourcePosition {
    pub generated: SourceLocation,
    pub resolved: SourceLocation,
    pub breadcrumb: Option<String>,
    pub mapping: String,
    pub diagnostic: Option<String>,
}

/// Resolves a 0-based generated position using the view's current projection.
/// Retain one symbol cache per immutable source view to reuse parsed breadcrumbs.
pub fn resolve_source_position(
    view: &ResolvedSourceView,
    generated_url: &str,
    source_map_url: Option<&str>,
    position: Position,
    symbol_indexes: &SymbolIndexCache,
) -> ResolvedSourcePosition {
    resolve_source_position_with_breadcrumb(
        view,
        generated_url,
        source_map_url,
        position,
        |url, content, line, column| symbol_indexes.breadcrumb(url, content, line, column, None),
    )
}

/// The callback receives the logical source URL, content, and 1-based position.
pub(crate) fn resolve_source_position_with_breadcrumb(
    view: &ResolvedSourceView,
    generated_url: &str,
    source_map_url: Option<&str>,
    position: Position,
    mut breadcrumb: impl FnMut(&str, &str, u32, u32) -> Option<String>,
) -> ResolvedSourcePosition {
    let generated = location(generated_url.to_owned(), position);
    let (logical_url, resolved_position, kind) = view
        .preferred_generated_location(generated_url, position)
        .unwrap_or_else(|| (generated_url.to_owned(), position, SourceKind::Identity));
    let (mapping, resolved_url) = match kind {
        SourceKind::Authored => (
            "authored",
            canonical_source_uri(source_map_url, &logical_url).display(),
        ),
        SourceKind::FormattedFallback => (
            "formatted",
            canonical_source_uri(None, &logical_url).display(),
        ),
        SourceKind::Identity => ("generated", generated_url.to_owned()),
    };
    let resolved = location(resolved_url, resolved_position);
    let content = if kind == SourceKind::Identity {
        (|| {
            let snapshot = view.source_snapshot(view.generated_snapshot(generated_url)?)?;
            view.content_store().get(snapshot.content_hash()?)
        })()
    } else {
        view.text(&logical_url).ok()
    };
    let diagnostic = (kind == SourceKind::Authored && content.is_none())
        .then(|| "Mapped authored source content is unavailable".to_owned());
    let breadcrumb = content
        .as_deref()
        .and_then(|content| breadcrumb(&logical_url, content, resolved.line, resolved.column));
    ResolvedSourcePosition {
        generated,
        resolved,
        breadcrumb,
        mapping: mapping.to_owned(),
        diagnostic,
    }
}

fn location(source_url: String, position: Position) -> SourceLocation {
    SourceLocation {
        source_url,
        line: position.line.saturating_add(1),
        column: position.column.saturating_add(1),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use sourcemap::SourceMapBuilder;

    use super::*;
    use crate::context_source_model::{ContextSourceModel, SourceContributionId};
    use crate::source_view::{GeneratedSourceInput, ResolutionPolicy};

    const GENERATED: &str = "file:///workspace/packages/deeply/nested/application/dist/bundle.js";
    const MAP: &str = "file:///workspace/packages/deeply/nested/application/dist/bundle.js.map";
    const AUTHORED: &str = "../src/features/objects/fixture.ts";
    const CONTENT: &str = "class Example { method() { return 1; } }";

    fn view(input: GeneratedSourceInput<'_>) -> ResolvedSourceView {
        let mut view = ResolvedSourceView::new(
            ResolutionPolicy::PreferSourcesContent,
            Arc::new(ContextSourceModel::new()),
            SourceContributionId::new("source-position-test"),
            BTreeMap::new(),
        );
        view.add_generated(input).unwrap();
        view
    }

    fn map(content: Option<&str>) -> Vec<u8> {
        let mut builder = SourceMapBuilder::new(Some(GENERATED));
        let source = builder.add_source(AUTHORED);
        builder.set_source_contents(source, content);
        builder.add(0, 4, 0, 29, Some(AUTHORED), None, false);
        let mut bytes = Vec::new();
        builder.into_sourcemap().to_writer(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn authored_location_uses_full_canonical_url_and_shared_breadcrumbs() {
        let map = map(Some(CONTENT));
        let view = view(GeneratedSourceInput {
            url: GENERATED,
            content: "run();",
            source_map: Some(&map),
            source_map_url: Some(MAP),
            minified: false,
        });
        let cache = SymbolIndexCache::default();
        for _ in 0..3 {
            let resolved = resolve_source_position(
                &view,
                GENERATED,
                Some(MAP),
                Position { line: 0, column: 4 },
                &cache,
            );
            assert_eq!(resolved.generated.source_url, GENERATED);
            assert_eq!((resolved.generated.line, resolved.generated.column), (1, 5));
            assert_eq!(
                resolved.resolved.source_url,
                "file:///workspace/packages/deeply/nested/application/src/features/objects/fixture.ts"
            );
            assert_eq!((resolved.resolved.line, resolved.resolved.column), (1, 30));
            assert_eq!(resolved.mapping, "authored");
            assert_eq!(resolved.breadcrumb.as_deref(), Some("Example.method"));
            assert_eq!(resolved.diagnostic, None);
            let json = serde_json::to_value(&resolved).unwrap();
            assert_eq!(json["generated"]["sourceUrl"], GENERATED);
            assert_eq!(
                serde_json::from_value::<ResolvedSourcePosition>(json).unwrap(),
                resolved
            );
        }
    }

    #[test]
    fn missing_authored_content_preserves_mapping_without_breadcrumb() {
        let map = map(None);
        let view = view(GeneratedSourceInput {
            url: GENERATED,
            content: "run();",
            source_map: Some(&map),
            source_map_url: Some(MAP),
            minified: true,
        });
        let resolved = resolve_source_position_with_breadcrumb(
            &view,
            GENERATED,
            Some(MAP),
            Position { line: 0, column: 4 },
            |_, _, _, _| panic!("unavailable content must not be parsed"),
        );
        assert_eq!(resolved.mapping, "authored");
        assert_eq!(
            resolved.resolved.source_url,
            "file:///workspace/packages/deeply/nested/application/src/features/objects/fixture.ts"
        );
        assert_eq!((resolved.resolved.line, resolved.resolved.column), (1, 30));
        assert_eq!(resolved.breadcrumb, None);
        assert!(resolved.diagnostic.unwrap().contains("unavailable"));
    }

    #[test]
    fn cached_source_map_resolves_authored_position_without_generated_source_bytes() {
        let map = map(Some(CONTENT));
        let view = view(GeneratedSourceInput {
            url: GENERATED,
            content: "",
            source_map: Some(&map),
            source_map_url: Some(MAP),
            minified: false,
        });
        let resolved = resolve_source_position(
            &view,
            GENERATED,
            Some(MAP),
            Position { line: 0, column: 4 },
            &SymbolIndexCache::default(),
        );
        assert_eq!(resolved.mapping, "authored");
        assert_eq!(resolved.resolved.line, 1);
        assert_eq!(resolved.resolved.column, 30);
    }

    #[test]
    fn formatted_location_matches_the_current_view_projection() {
        let source = "function example(){const text='};';return text;}";
        let view = view(GeneratedSourceInput {
            url: GENERATED,
            content: source,
            source_map: None,
            source_map_url: None,
            minified: true,
        });
        let position = Position {
            line: 0,
            column: source.find("return").unwrap() as u32,
        };
        let projected = view.forward(GENERATED, position).remove(0);
        let cache = SymbolIndexCache::default();
        let resolved = resolve_source_position(&view, GENERATED, None, position, &cache);
        assert_eq!(resolved.mapping, "formatted");
        assert_eq!(
            resolved.resolved.source_url,
            format!("{GENERATED}?formatted")
        );
        assert_eq!(resolved.resolved.line, projected.position.line + 1);
        assert_eq!(resolved.resolved.column, projected.position.column + 1);
        assert!(resolved.resolved.line > 1);
        assert_eq!(resolved.breadcrumb.as_deref(), Some("example"));
        assert_eq!(resolved.diagnostic, None);
    }

    #[test]
    fn absent_mapping_preserves_raw_coordinates_and_generated_breadcrumb() {
        let map = map(Some(CONTENT));
        let view = view(GeneratedSourceInput {
            url: GENERATED,
            content: "function generated(){return 1;}",
            source_map: Some(&map),
            source_map_url: Some(MAP),
            minified: false,
        });
        let cache = SymbolIndexCache::default();
        let resolved = resolve_source_position(
            &view,
            GENERATED,
            Some(MAP),
            Position { line: 0, column: 0 },
            &cache,
        );
        assert_eq!(resolved.mapping, "generated");
        assert_eq!(resolved.generated, resolved.resolved);
        assert_eq!(resolved.resolved.source_url, GENERATED);
        assert_eq!((resolved.resolved.line, resolved.resolved.column), (1, 1));
        assert_eq!(resolved.breadcrumb.as_deref(), Some("generated"));
    }

    #[test]
    fn generated_fallback_keeps_full_windows_path_and_utf16_coordinates() {
        let url = r"D:\workspace\packages\deeply\nested\application\fixture.js";
        let view = view(GeneratedSourceInput {
            url,
            content: "const text = '😀';",
            source_map: None,
            source_map_url: None,
            minified: false,
        });
        let resolved = resolve_source_position(
            &view,
            url,
            None,
            Position {
                line: 7,
                column: 23,
            },
            &SymbolIndexCache::default(),
        );
        assert_eq!(resolved.generated, resolved.resolved);
        assert_eq!(resolved.resolved.source_url, url);
        assert_eq!((resolved.resolved.line, resolved.resolved.column), (8, 24));
        assert_eq!(resolved.mapping, "generated");
    }
}

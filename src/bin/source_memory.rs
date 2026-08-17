use std::collections::BTreeMap;
use std::sync::Arc;

use cdp_client::content_store::ContentStore;
use cdp_client::source_view::{
    GeneratedSourceInput, Position, ResolutionPolicy, ResolvedSourceView,
};
use sourcemap::SourceMapBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = "export const value = 42;\n".repeat(8_000);
    let map = synthetic_map(&source, 50_000)?;
    let store = Arc::new(ContentStore::default());
    let mut view = ResolvedSourceView::new(
        ResolutionPolicy::PreferSourcesContent,
        store,
        BTreeMap::new(),
    );

    for url in ["first.bundle.js", "second.bundle.js"] {
        view.add_generated(GeneratedSourceInput {
            url,
            content: "x",
            source_map: Some(&map),
            minified: false,
        })?;
    }

    println!("before reverse index:\n{:#?}", view.memory_report());
    let _ = view.reverse(
        "src/shared.ts",
        Position {
            line: 499,
            column: 99,
        },
    );
    println!("after reverse index:\n{:#?}", view.memory_report());
    Ok(())
}

fn synthetic_map(source_content: &str, token_count: u32) -> Result<Vec<u8>, sourcemap::Error> {
    let mut builder = SourceMapBuilder::new(Some("bundle.js"));
    let source_id = builder.add_source("src/shared.ts");
    builder.set_source_contents(source_id, Some(source_content));
    for index in 0..token_count {
        builder.add(
            index / 100,
            index % 100,
            index / 100,
            index % 100,
            Some("src/shared.ts"),
            None,
            false,
        );
    }
    let mut encoded = Vec::new();
    builder.into_sourcemap().to_writer(&mut encoded)?;
    Ok(encoded)
}

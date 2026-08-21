use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::sync::Arc;
use std::time::Instant;

use cdp_client::content_store::ContentStore;
use cdp_client::source_view::{
    GeneratedSourceInput, Position, ResolutionPolicy, ResolvedSourceView,
};
use sourcemap::SourceMapBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = env::args().nth(1) {
        return benchmark_map(&path);
    }

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

    fn benchmark_map(path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let started = Instant::now();
        let map = fs::read(path)?;
        let read = started.elapsed();

        let started = Instant::now();
        let decoded = sourcemap::decode_slice(&map)?;
        let decode = started.elapsed();
        let (sources, tokens) = match decoded {
            sourcemap::DecodedMap::Regular(map) => (map.get_source_count(), map.get_token_count()),
            sourcemap::DecodedMap::Index(index) => {
                let flattened = index.flatten()?;
                (flattened.get_source_count(), flattened.get_token_count())
            }
            sourcemap::DecodedMap::Hermes(_) => (0, 0),
        };

        let store = Arc::new(ContentStore::default());
        let mut view = ResolvedSourceView::new(
            ResolutionPolicy::PreferSourcesContent,
            store,
            BTreeMap::new(),
        );
        let started = Instant::now();
        view.add_generated(GeneratedSourceInput {
            url: "benchmark.js",
            content: "",
            source_map: Some(&map),
            minified: false,
        })?;
        let build = started.elapsed();

        println!(
            "{} bytes, {sources} sources, {tokens} tokens\nread: {read:.3?}\ndecode: {decode:.3?}\nview: {build:.3?}",
            map.len()
        );
        Ok(())
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

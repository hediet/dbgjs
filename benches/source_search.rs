use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use cdp_client::content_store::ContentHash;
use cdp_client::source_search::{
    SearchControl, SearchDocument, SearchQuery, SourceIdentity, search,
};

const LOGICAL_SOURCES: usize = 10_000;
const UNIQUE_CONTENTS: usize = 100;
const ITERATIONS: usize = 10;

fn main() {
    let contents = (0..UNIQUE_CONTENTS)
        .map(|index| {
            Arc::<str>::from(format!(
                "export function validateUser{index}(value: unknown) {{\n  return value != null;\n}}\n"
            ))
        })
        .collect::<Vec<_>>();
    let documents = (0..LOGICAL_SOURCES)
        .map(|index| {
            let content = contents[index % UNIQUE_CONTENTS].clone();
            SearchDocument {
                identity: SourceIdentity {
                    path: format!("src/generated/source-{index:05}.ts"),
                    connection_id: Some(format!("connection-{}", index % 4)),
                    target_id: Some(format!("target-{}", index % 32)),
                    kind: "authored".to_owned(),
                    provenance: "benchmark fixture".to_owned(),
                },
                content_hash: ContentHash::of_bytes(content.as_bytes()),
                content,
            }
        })
        .collect::<Vec<_>>();
    let query = SearchQuery {
        pattern: r"validateUser\d+\(".to_owned(),
        regex: true,
        case_sensitive: true,
        max_results: 200,
        context_lines: 1,
    };

    let warmup = search(documents.clone(), &query, &SearchControl::default()).unwrap();
    assert_eq!(warmup.searched_sources, LOGICAL_SOURCES as u32);
    assert_eq!(warmup.searched_contents, UNIQUE_CONTENTS as u32);
    assert_eq!(warmup.total_matches, LOGICAL_SOURCES as u64);

    let started = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(search(
            documents.clone(),
            black_box(&query),
            &SearchControl::default(),
        ))
        .unwrap();
    }
    let elapsed = started.elapsed();
    let per_iteration = elapsed / ITERATIONS as u32;
    println!(
        "source-search: {LOGICAL_SOURCES} logical / {UNIQUE_CONTENTS} unique, \
         {ITERATIONS} iterations in {elapsed:?}, {per_iteration:?}/iteration"
    );
}

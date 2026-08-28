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
const MANY_HIT_LINES: usize = 10_000;
const MANY_HIT_RESULTS: usize = 5_000;
const MANY_HIT_ITERATIONS: usize = 3;
const RARE_EOF_SMALL_LINES: usize = 20_000;
const RARE_EOF_LARGE_LINES: usize = RARE_EOF_SMALL_LINES * 4;
const RARE_EOF_ITERATIONS: usize = 5;

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

    benchmark_many_hit_materialization();
    benchmark_rare_eof_fallback();
}

fn benchmark_many_hit_materialization() {
    let content = Arc::<str>::from(
        (0..MANY_HIT_LINES)
            .map(|index| format!("line {index:05}: needle payload\r\n"))
            .collect::<String>(),
    );
    let document = SearchDocument {
        identity: SourceIdentity {
            path: "src/generated/large.ts".to_owned(),
            connection_id: None,
            target_id: None,
            kind: "authored".to_owned(),
            provenance: "benchmark fixture".to_owned(),
        },
        content_hash: ContentHash::of_bytes(content.as_bytes()),
        content: content.clone(),
    };
    let query = SearchQuery {
        pattern: "needle".to_owned(),
        regex: false,
        case_sensitive: true,
        max_results: MANY_HIT_RESULTS,
        context_lines: 2,
    };

    let indexed_warmup = search(vec![document.clone()], &query, &SearchControl::default()).unwrap();
    let repeated_warmup =
        repeated_line_collection(&content, "needle", MANY_HIT_RESULTS, query.context_lines);
    assert_eq!(indexed_warmup.hits.len(), repeated_warmup.len());

    let indexed_started = Instant::now();
    for _ in 0..MANY_HIT_ITERATIONS {
        let result = search(
            black_box(vec![document.clone()]),
            black_box(&query),
            &SearchControl::default(),
        )
        .unwrap();
        black_box(result);
    }
    let indexed = indexed_started.elapsed();

    let repeated_started = Instant::now();
    for _ in 0..MANY_HIT_ITERATIONS {
        black_box(repeated_line_collection(
            black_box(&content),
            black_box("needle"),
            MANY_HIT_RESULTS,
            query.context_lines,
        ));
    }
    let repeated = repeated_started.elapsed();
    println!(
        "many-hit-materialization: {MANY_HIT_LINES} lines / {MANY_HIT_RESULTS} hits, \
         {MANY_HIT_ITERATIONS} iterations: indexed {indexed:?}, \
         repeated-lines baseline {repeated:?}, {:.1}x faster",
        repeated.as_secs_f64() / indexed.as_secs_f64()
    );
}

fn repeated_line_collection(
    content: &str,
    pattern: &str,
    max_results: usize,
    context_lines: usize,
) -> Vec<(String, Vec<String>, Vec<String>)> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(pattern))
        .take(max_results)
        .map(|(index, _)| {
            let lines = content.lines().collect::<Vec<_>>();
            let before_start = index.saturating_sub(context_lines);
            let after_end = index
                .saturating_add(context_lines)
                .saturating_add(1)
                .min(lines.len());
            (
                lines[index].to_owned(),
                lines[before_start..index]
                    .iter()
                    .map(|line| (*line).to_owned())
                    .collect(),
                lines[index.saturating_add(1)..after_end]
                    .iter()
                    .map(|line| (*line).to_owned())
                    .collect(),
            )
        })
        .collect()
}

fn benchmark_rare_eof_fallback() {
    let small = rare_eof_elapsed(RARE_EOF_SMALL_LINES);
    let large = rare_eof_elapsed(RARE_EOF_LARGE_LINES);
    let growth = large.as_secs_f64() / small.as_secs_f64();
    println!(
        "newline-fallback-rare-eof: {RARE_EOF_SMALL_LINES} lines {small:?}, \
         {RARE_EOF_LARGE_LINES} lines {large:?}, {growth:.2}x time for 4x input"
    );
    assert!(
        growth < 8.0,
        "newline fallback grew {growth:.2}x for 4x input; expected near-linear behavior"
    );
}

fn rare_eof_elapsed(lines: usize) -> std::time::Duration {
    let content = Arc::<str>::from(format!(
        "{}rare-marker",
        "ordinary payload\r\n".repeat(lines)
    ));
    let document = SearchDocument {
        identity: SourceIdentity {
            path: "src/generated/rare-eof.ts".to_owned(),
            connection_id: None,
            target_id: None,
            kind: "authored".to_owned(),
            provenance: "benchmark fixture".to_owned(),
        },
        content_hash: ContentHash::of_bytes(content.as_bytes()),
        content,
    };
    let query = SearchQuery {
        pattern: "rare-marker|never\\r?\\nmatches".to_owned(),
        regex: true,
        case_sensitive: true,
        max_results: 10,
        context_lines: 0,
    };
    let warmup = search(vec![document.clone()], &query, &SearchControl::default()).unwrap();
    assert_eq!(warmup.total_matches, 1);

    let started = Instant::now();
    for _ in 0..RARE_EOF_ITERATIONS {
        black_box(search(
            black_box(vec![document.clone()]),
            black_box(&query),
            &SearchControl::default(),
        ))
        .unwrap();
    }
    started.elapsed()
}

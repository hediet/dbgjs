use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks};
use rayon::prelude::*;

use crate::content_store::ContentHash;

const MAX_SEARCH_THREADS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceIdentity {
    pub path: String,
    pub connection_id: Option<String>,
    pub target_id: Option<String>,
    pub kind: String,
    pub provenance: String,
}

#[derive(Clone)]
pub struct SearchDocument {
    pub identity: SourceIdentity,
    pub content_hash: ContentHash,
    pub content: Arc<str>,
}

#[derive(Clone)]
pub struct HydratedSource {
    pub path: String,
    pub kind: String,
    pub provenance: String,
    pub content_hash: ContentHash,
    pub content: Arc<str>,
}

#[derive(Clone, Default)]
pub struct HydratedSourceBatch {
    pub sources: Vec<HydratedSource>,
    pub skipped_sources: u32,
}

#[derive(Clone, Debug)]
pub struct SearchQuery {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub max_results: usize,
    pub context_lines: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchProgress {
    pub searched_contents: usize,
    pub total_contents: usize,
}

pub trait SearchProgressObserver: Send + Sync {
    fn report(&self, progress: SearchProgress);
}

impl<F> SearchProgressObserver for F
where
    F: Fn(SearchProgress) + Send + Sync,
{
    fn report(&self, progress: SearchProgress) {
        self(progress);
    }
}

#[derive(Clone)]
pub struct SearchControl {
    cancelled: Arc<AtomicBool>,
    pub deadline: Option<Instant>,
    pub progress: Option<Arc<dyn SearchProgressObserver>>,
}

impl Default for SearchControl {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: None,
            progress: None,
        }
    }
}

impl SearchControl {
    pub fn with_deadline(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
            ..Self::default()
        }
    }

    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn check(&self) -> Result<(), SearchError> {
        self.interruption().map_or(Ok(()), Err)
    }

    fn interruption(&self) -> Option<SearchError> {
        if self.cancelled.load(Ordering::Acquire) {
            Some(SearchError::Cancelled)
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some(SearchError::DeadlineExceeded)
        } else {
            None
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SearchError {
    #[error("source search was cancelled")]
    Cancelled,
    #[error("source search deadline exceeded")]
    DeadlineExceeded,
    #[error("invalid source search pattern: {0}")]
    InvalidPattern(String),
    #[error("source search failed: {0}")]
    Search(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub identity: SourceIdentity,
    pub content_hash: ContentHash,
    pub line: u32,
    pub column: u32,
    pub match_length: u32,
    pub text: String,
    pub before_context: Vec<String>,
    pub after_context: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub total_matches: u64,
    pub searched_sources: u32,
    pub searched_contents: u32,
}

struct UniqueContent {
    content: Arc<str>,
    identities: BTreeSet<SourceIdentity>,
}

#[derive(Clone, Copy, Debug)]
struct MatchLocation {
    line: u32,
    column: u32,
    length: u32,
}

struct ContentMatches {
    hash: ContentHash,
    content: Arc<str>,
    identities: BTreeSet<SourceIdentity>,
    locations: Vec<MatchLocation>,
    total: u64,
}

pub fn search(
    documents: Vec<SearchDocument>,
    query: &SearchQuery,
    control: &SearchControl,
) -> Result<SearchResult, SearchError> {
    validate(query)?;
    if let Some(interruption) = control.interruption() {
        return Err(interruption);
    }

    let matcher = build_matcher(query)?;
    let mut unique = BTreeMap::<ContentHash, UniqueContent>::new();
    for document in documents {
        let entry = unique
            .entry(document.content_hash)
            .or_insert_with(|| UniqueContent {
                content: document.content,
                identities: BTreeSet::new(),
            });
        entry.identities.insert(document.identity);
    }
    let searched_sources = unique
        .values()
        .map(|content| content.identities.len())
        .sum::<usize>();
    let total_contents = unique.len();
    let completed = Mutex::new(0_usize);
    let mut searched = search_pool().install(|| {
        unique
            .into_par_iter()
            .map(|(hash, content)| {
                let result = search_content(hash, content, &matcher, query.max_results, control);
                let mut searched_contents = completed.lock().unwrap();
                *searched_contents += 1;
                if let Some(observer) = &control.progress {
                    observer.report(SearchProgress {
                        searched_contents: *searched_contents,
                        total_contents,
                    });
                }
                (hash, result)
            })
            .collect::<Vec<_>>()
    });
    searched.sort_by_key(|(hash, _)| *hash);
    if let Some(interruption) = control.interruption() {
        return Err(interruption);
    }

    let mut contents = Vec::with_capacity(searched.len());
    for (_, result) in searched {
        contents.push(result?);
    }
    let total_matches = contents.iter().fold(0_u64, |total, content| {
        total.saturating_add(
            content
                .total
                .saturating_mul(content.identities.len() as u64),
        )
    });

    let mut identities = contents
        .iter()
        .flat_map(|content| {
            content
                .identities
                .iter()
                .map(move |identity| (identity, content))
        })
        .collect::<Vec<_>>();
    identities.sort_by(
        |(left_identity, left_content), (right_identity, right_content)| {
            (*left_identity, left_content.hash).cmp(&(*right_identity, right_content.hash))
        },
    );
    let mut hits = Vec::with_capacity(
        query
            .max_results
            .min(usize::try_from(total_matches).unwrap_or(usize::MAX)),
    );
    for (identity, content) in identities {
        for location in content
            .locations
            .iter()
            .take(query.max_results.saturating_sub(hits.len()))
        {
            hits.push(materialize_hit(
                identity,
                content,
                *location,
                query.context_lines,
            ));
        }
        if hits.len() == query.max_results {
            break;
        }
    }
    Ok(SearchResult {
        hits,
        total_matches,
        searched_sources: searched_sources.min(u32::MAX as usize) as u32,
        searched_contents: total_contents.min(u32::MAX as usize) as u32,
    })
}

pub fn validate(query: &SearchQuery) -> Result<(), SearchError> {
    if query.pattern.is_empty() {
        return Err(SearchError::InvalidPattern(
            "pattern must not be empty".to_owned(),
        ));
    }
    if query.max_results == 0 {
        return Err(SearchError::InvalidPattern(
            "max_results must be positive".to_owned(),
        ));
    }
    build_matcher(query).map(|_| ())
}

fn build_matcher(query: &SearchQuery) -> Result<RegexMatcher, SearchError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .case_insensitive(!query.case_sensitive)
        .fixed_strings(!query.regex);
    builder
        .build(&query.pattern)
        .map_err(|error| SearchError::InvalidPattern(error.to_string()))
}

fn search_content(
    hash: ContentHash,
    content: UniqueContent,
    matcher: &RegexMatcher,
    max_results: usize,
    control: &SearchControl,
) -> Result<ContentMatches, SearchError> {
    if let Some(interruption) = control.interruption() {
        return Err(interruption);
    }
    let mut locations = Vec::with_capacity(max_results.min(64));
    let mut total = 0_u64;
    let mut matcher_error = None;
    let mut interrupted = None;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::none())
        .build();
    let search_result = searcher.search_slice(
        matcher,
        content.content.as_bytes(),
        sinks::UTF8(|line_number, line| {
            if let Some(reason) = control.interruption() {
                interrupted = Some(reason);
                return Ok(false);
            }
            let line = line.strip_suffix('\n').unwrap_or(line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Err(error) = matcher.find_iter(line.as_bytes(), |matched| {
                total = total.saturating_add(1);
                if locations.len() < max_results {
                    locations.push(MatchLocation {
                        line: line_number as u32,
                        column: matched.start() as u32 + 1,
                        length: matched.end().saturating_sub(matched.start()) as u32,
                    });
                }
                true
            }) {
                matcher_error = Some(error.to_string());
                return Ok(false);
            }
            Ok(true)
        }),
    );
    if let Some(interruption) = interrupted {
        return Err(interruption);
    }
    if let Some(error) = matcher_error {
        return Err(SearchError::Search(error));
    }
    search_result.map_err(|error| SearchError::Search(error.to_string()))?;
    Ok(ContentMatches {
        hash,
        content: content.content,
        identities: content.identities,
        locations,
        total,
    })
}

fn materialize_hit(
    identity: &SourceIdentity,
    content: &ContentMatches,
    location: MatchLocation,
    context_lines: usize,
) -> SearchHit {
    let lines = content.content.lines().collect::<Vec<_>>();
    let index = location.line.saturating_sub(1) as usize;
    let before_start = index.saturating_sub(context_lines);
    let after_end = (index + context_lines + 1).min(lines.len());
    SearchHit {
        identity: identity.clone(),
        content_hash: content.hash,
        line: location.line,
        column: location.column,
        match_length: location.length,
        text: lines.get(index).copied().unwrap_or_default().to_owned(),
        before_context: lines[before_start..index]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
        after_context: lines[index.saturating_add(1)..after_end]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
    }
}

fn search_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let available = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        rayon::ThreadPoolBuilder::new()
            .num_threads(available.min(MAX_SEARCH_THREADS))
            .thread_name(|index| format!("source-search-{index}"))
            .build()
            .expect("source search thread pool should build")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn document(path: &str, content: Arc<str>) -> SearchDocument {
        SearchDocument {
            identity: SourceIdentity {
                path: path.to_owned(),
                connection_id: None,
                target_id: None,
                kind: "authored".to_owned(),
                provenance: "fixture".to_owned(),
            },
            content_hash: ContentHash::of_bytes(content.as_bytes()),
            content,
        }
    }

    fn query(pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_owned(),
            regex: false,
            case_sensitive: true,
            max_results: 200,
            context_lines: 0,
        }
    }

    #[test]
    fn preserves_literal_regex_case_and_context_semantics() {
        let content: Arc<str> = "before\nNeedle needle\nneedle(42)\nafter".into();
        let mut options = query(r"needle\(\d+\)");
        options.regex = true;
        options.case_sensitive = false;
        options.context_lines = 1;

        let result = search(
            vec![document("src/app.ts", content)],
            &options,
            &SearchControl::default(),
        )
        .unwrap();

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].line, 3);
        assert_eq!(result.hits[0].column, 1);
        assert_eq!(result.hits[0].match_length, 10);
        assert_eq!(result.hits[0].before_context, ["Needle needle"]);
        assert_eq!(result.hits[0].after_context, ["after"]);

        let literal = search(
            vec![document("src/app.ts", "needle(42)".into())],
            &query("needle(42)"),
            &SearchControl::default(),
        )
        .unwrap();
        assert_eq!(literal.hits.len(), 1);

        let mut line_oriented = query(r"\n");
        line_oriented.regex = true;
        assert!(
            search(
                vec![document("src/app.ts", "first\nsecond".into())],
                &line_oriented,
                &SearchControl::default(),
            )
            .unwrap()
            .hits
            .is_empty()
        );
    }

    #[test]
    fn searches_equal_content_once_and_deterministically_fans_out() {
        let shared: Arc<str> = "const shared = 1;\nshared();".into();
        let mut endpoint_copy = document("a.ts", shared.clone());
        endpoint_copy.identity.connection_id = Some("browser".to_owned());
        endpoint_copy.identity.target_id = Some("page-2".to_owned());
        endpoint_copy.identity.provenance = "source-map sourcesContent".to_owned();
        let result = search(
            vec![
                document("z.ts", shared.clone()),
                document("a.ts", shared),
                endpoint_copy,
                document("b.ts", "shared elsewhere".into()),
            ],
            &query("shared"),
            &SearchControl::default(),
        )
        .unwrap();

        assert_eq!(result.searched_contents, 2);
        assert_eq!(result.searched_sources, 4);
        assert_eq!(result.total_matches, 7);
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| (hit.identity.path.as_str(), hit.line))
                .collect::<Vec<_>>(),
            [
                ("a.ts", 1),
                ("a.ts", 2),
                ("a.ts", 1),
                ("a.ts", 2),
                ("b.ts", 1),
                ("z.ts", 1),
                ("z.ts", 2),
            ]
        );
        assert_eq!(
            result.hits[2].identity.connection_id.as_deref(),
            Some("browser")
        );
        assert_eq!(
            result.hits[2].identity.provenance,
            "source-map sourcesContent"
        );
    }

    #[test]
    fn applies_budget_after_deterministic_fan_out_and_counts_omissions() {
        let shared: Arc<str> = "hit hit hit".into();
        let mut options = query("hit");
        options.max_results = 2;
        let result = search(
            vec![document("b.ts", shared.clone()), document("a.ts", shared)],
            &options,
            &SearchControl::default(),
        )
        .unwrap();

        assert_eq!(result.hits.len(), 2);
        assert!(result.hits.iter().all(|hit| hit.identity.path == "a.ts"));
        assert_eq!(result.total_matches - result.hits.len() as u64, 4);
    }

    #[test]
    fn exposes_cancellation_deadline_and_progress_as_independent_controls() {
        let cancelled = SearchControl::default();
        cancelled.cancel();
        assert_eq!(
            search(vec![], &query("x"), &cancelled),
            Err(SearchError::Cancelled)
        );

        let expired = SearchControl::with_deadline(Instant::now());
        assert_eq!(
            search(vec![], &query("x"), &expired),
            Err(SearchError::DeadlineExceeded)
        );

        let reports = Arc::new(AtomicUsize::new(0));
        let observer_reports = reports.clone();
        let control = SearchControl {
            progress: Some(Arc::new(move |_progress: SearchProgress| {
                observer_reports.fetch_add(1, Ordering::Relaxed);
            })),
            ..SearchControl::default()
        };
        search(
            vec![document("a.ts", "x".into()), document("b.ts", "y".into())],
            &query("x"),
            &control,
        )
        .unwrap();
        assert_eq!(reports.load(Ordering::Relaxed), 2);
    }
}

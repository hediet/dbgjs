use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use grep_matcher::{LineTerminator, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;

use crate::content_store::ContentHash;

const MAX_SEARCH_THREADS: usize = 8;
const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

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
    #[cfg(test)]
    cancel_after_bytes: Option<usize>,
}

impl Default for SearchControl {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: None,
            progress: None,
            #[cfg(test)]
            cancel_after_bytes: None,
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
    line_ranges: Vec<Range<usize>>,
    identities: BTreeSet<SourceIdentity>,
    locations: Vec<MatchLocation>,
    total: u64,
}

struct CompiledMatcher {
    candidate: RegexMatcher,
    exact: RegexMatcher,
}

struct InterruptibleReader<'a> {
    bytes: &'a [u8],
    position: usize,
    control: &'a SearchControl,
    interruption: Option<SearchError>,
}

impl<'a> InterruptibleReader<'a> {
    fn new(bytes: &'a [u8], control: &'a SearchControl) -> Self {
        Self {
            bytes,
            position: 0,
            control,
            interruption: None,
        }
    }
}

impl Read for InterruptibleReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(interruption) = self.control.interruption() {
            self.interruption = Some(interruption);
            return Err(io::Error::other("source search interrupted"));
        }
        if self.position == self.bytes.len() || buffer.is_empty() {
            return Ok(0);
        }

        let length = buffer
            .len()
            .min(CANCELLATION_CHECK_BYTES)
            .min(self.bytes.len() - self.position);
        buffer[..length]
            .copy_from_slice(&self.bytes[self.position..self.position.saturating_add(length)]);
        self.position += length;

        #[cfg(test)]
        if self
            .control
            .cancel_after_bytes
            .is_some_and(|limit| self.position >= limit)
        {
            self.control.cancel();
        }
        Ok(length)
    }
}

struct ContentMatchSink<'a> {
    matcher: &'a RegexMatcher,
    content: &'a [u8],
    control: &'a SearchControl,
    max_results: usize,
    locations: &'a mut Vec<MatchLocation>,
    total: &'a mut u64,
    matcher_error: &'a mut Option<String>,
    interrupted: &'a mut Option<SearchError>,
}

impl Sink for ContentMatchSink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched_line: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if let Some(reason) = self.control.interruption() {
            *self.interrupted = Some(reason);
            return Ok(false);
        }
        let Ok(line_start) = usize::try_from(matched_line.absolute_byte_offset()) else {
            *self.matcher_error = Some("source offset exceeds platform limits".to_owned());
            return Ok(false);
        };
        let mut line_length = matched_line.bytes().len();
        if matched_line.bytes().last() == Some(&b'\n') {
            line_length -= 1;
            if line_length > 0 && matched_line.bytes()[line_length - 1] == b'\r' {
                line_length -= 1;
            }
        }
        let line_end = line_start.saturating_add(line_length);
        let line_number = matched_line.line_number().unwrap_or(1) as u32;
        if let Err(error) = self
            .matcher
            .find_iter_at(self.content, line_start, |matched| {
                if matched.start() > line_end || matched.end() > line_end {
                    return false;
                }
                *self.total = self.total.saturating_add(1);
                if self.locations.len() < self.max_results {
                    self.locations.push(MatchLocation {
                        line: line_number,
                        column: matched.start().saturating_sub(line_start) as u32 + 1,
                        length: matched.end().saturating_sub(matched.start()) as u32,
                    });
                }
                true
            })
        {
            *self.matcher_error = Some(error.to_string());
            return Ok(false);
        }
        Ok(true)
    }
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

fn build_matcher(query: &SearchQuery) -> Result<CompiledMatcher, SearchError> {
    let mut exact_builder = RegexMatcherBuilder::new();
    exact_builder
        .crlf(true)
        .line_terminator(None)
        .multi_line(true)
        .case_insensitive(!query.case_sensitive)
        .fixed_strings(!query.regex);
    let exact = exact_builder
        .build(&query.pattern)
        .map_err(|error| SearchError::InvalidPattern(error.to_string()))?;

    let mut line_builder = RegexMatcherBuilder::new();
    line_builder
        .crlf(true)
        .multi_line(true)
        .case_insensitive(!query.case_sensitive)
        .fixed_strings(!query.regex);
    let candidate = line_builder.build(&query.pattern).unwrap_or_else(|_| {
        RegexMatcherBuilder::new()
            .crlf(true)
            .multi_line(true)
            .build("")
            .expect("the match-all candidate regex is valid")
    });
    Ok(CompiledMatcher { candidate, exact })
}

fn search_content(
    hash: ContentHash,
    content: UniqueContent,
    matcher: &CompiledMatcher,
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
        .line_terminator(LineTerminator::crlf())
        .binary_detection(BinaryDetection::none())
        .build();
    let mut reader = InterruptibleReader::new(content.content.as_bytes(), control);
    let search_result = searcher.search_reader(
        &matcher.candidate,
        &mut reader,
        ContentMatchSink {
            matcher: &matcher.exact,
            content: content.content.as_bytes(),
            control,
            max_results,
            locations: &mut locations,
            total: &mut total,
            matcher_error: &mut matcher_error,
            interrupted: &mut interrupted,
        },
    );
    if let Some(interruption) = reader.interruption {
        return Err(interruption);
    }
    if let Some(interruption) = interrupted {
        return Err(interruption);
    }
    if let Some(error) = matcher_error {
        return Err(SearchError::Search(error));
    }
    search_result.map_err(|error| SearchError::Search(error.to_string()))?;
    let line_ranges = if locations.is_empty() {
        Vec::new()
    } else {
        build_line_ranges(&content.content, control)?
    };
    Ok(ContentMatches {
        hash,
        content: content.content,
        line_ranges,
        identities: content.identities,
        locations,
        total,
    })
}

fn build_line_ranges(
    content: &str,
    control: &SearchControl,
) -> Result<Vec<Range<usize>>, SearchError> {
    let bytes = content.as_bytes();
    let mut ranges = Vec::new();
    let mut line_start = 0;
    for chunk_start in (0..bytes.len()).step_by(CANCELLATION_CHECK_BYTES) {
        control.check()?;
        let chunk_end = (chunk_start + CANCELLATION_CHECK_BYTES).min(bytes.len());
        for newline in bytes[chunk_start..chunk_end]
            .iter()
            .enumerate()
            .filter_map(|(offset, byte)| (*byte == b'\n').then_some(chunk_start + offset))
        {
            let line_end = if newline > line_start && bytes[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            };
            ranges.push(line_start..line_end);
            line_start = newline + 1;
        }
    }
    if line_start < bytes.len() {
        ranges.push(line_start..bytes.len());
    }
    control.check()?;
    Ok(ranges)
}

fn materialize_hit(
    identity: &SourceIdentity,
    content: &ContentMatches,
    location: MatchLocation,
    context_lines: usize,
) -> SearchHit {
    let index = location.line.saturating_sub(1) as usize;
    let before_start = index.saturating_sub(context_lines);
    let after_end = index
        .saturating_add(context_lines)
        .saturating_add(1)
        .min(content.line_ranges.len());
    let line = |index: usize| {
        content
            .line_ranges
            .get(index)
            .and_then(|range| content.content.get(range.clone()))
            .unwrap_or_default()
    };
    SearchHit {
        identity: identity.clone(),
        content_hash: content.hash,
        line: location.line,
        column: location.column,
        match_length: location.length,
        text: line(index).to_owned(),
        before_context: (before_start..index).map(line).map(str::to_owned).collect(),
        after_context: (index.saturating_add(1)..after_end)
            .map(line)
            .map(str::to_owned)
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
    fn preserves_crlf_endings_and_absolute_regex_anchors() {
        let mut crlf = query(r"foo$");
        crlf.regex = true;
        let crlf_result = search(
            vec![document("crlf.ts", "foo\r\nbar\r\n".into())],
            &crlf,
            &SearchControl::default(),
        )
        .unwrap();
        assert_eq!(
            crlf_result
                .hits
                .iter()
                .map(|hit| (hit.line, hit.column, hit.match_length))
                .collect::<Vec<_>>(),
            [(1, 1, 3)]
        );

        let mut anchored = query(r"\A|foo|\z");
        anchored.regex = true;
        let anchored_result = search(
            vec![document("anchors.ts", "head\nfoo\ntail".into())],
            &anchored,
            &SearchControl::default(),
        )
        .unwrap();
        assert_eq!(
            anchored_result
                .hits
                .iter()
                .map(|hit| (hit.line, hit.column, hit.match_length))
                .collect::<Vec<_>>(),
            [(1, 1, 0), (2, 1, 3), (3, 5, 0)]
        );
    }

    #[test]
    fn falls_back_to_all_lines_for_newline_capable_patterns() {
        let content: Arc<str> = "head\r\nfoo\r\nbar".into();

        let mut alternative = query(r"foo|\n");
        alternative.regex = true;
        let result = search(
            vec![document("alternative.ts", content.clone())],
            &alternative,
            &SearchControl::default(),
        )
        .unwrap();
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| (hit.line, hit.column, hit.match_length))
                .collect::<Vec<_>>(),
            [(2, 1, 3)]
        );

        let mut multiline = query(r"foo\r?\nbar");
        multiline.regex = true;
        assert!(
            search(
                vec![document("multiline.ts", content.clone())],
                &multiline,
                &SearchControl::default(),
            )
            .unwrap()
            .hits
            .is_empty()
        );

        let mut anchored = query(r"\A|\n|foo|\z");
        anchored.regex = true;
        let result = search(
            vec![document("anchored.ts", content)],
            &anchored,
            &SearchControl::default(),
        )
        .unwrap();
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| (hit.line, hit.column, hit.match_length))
                .collect::<Vec<_>>(),
            [(1, 1, 0), (2, 1, 3), (3, 4, 0)]
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

    #[test]
    fn cancellation_interrupts_a_no_match_scan() {
        let control = SearchControl {
            cancel_after_bytes: Some(CANCELLATION_CHECK_BYTES),
            ..SearchControl::default()
        };
        let content = Arc::<str>::from("x".repeat(CANCELLATION_CHECK_BYTES * 4));

        assert_eq!(
            search(
                vec![document("large.ts", content)],
                &SearchQuery {
                    pattern: r"absent|\n".to_owned(),
                    regex: true,
                    ..query("absent")
                },
                &control
            ),
            Err(SearchError::Cancelled)
        );
    }

    #[test]
    fn indexes_crlf_and_empty_lines_once_for_many_hit_contexts() {
        let mut options = query("hit");
        options.context_lines = 1;
        let result = search(
            vec![document(
                "many.ts",
                "first\r\nhit one\r\n\r\nhit two\n".into(),
            )],
            &options,
            &SearchControl::default(),
        )
        .unwrap();

        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.hits[0].before_context, ["first"]);
        assert_eq!(result.hits[0].after_context, [""]);
        assert_eq!(result.hits[1].before_context, [""]);
        assert!(result.hits[1].after_context.is_empty());
    }
}

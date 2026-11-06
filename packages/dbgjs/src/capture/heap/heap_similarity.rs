//! Finds groups of identical and near-identical flat strings in a heap graph,
//! for example the same file read repeatedly without interning.
//!
//! Each distinct text is split into content-defined chunks with a Gear rolling
//! hash, so an insertion or a changed prefix only changes the chunks around the
//! edit instead of shifting every later block. MinHash signatures over the chunk
//! sets are banded (locality-sensitive hashing) to find candidate pairs without
//! comparing every pair of strings. Candidates are verified with the
//! byte-weighted Jaccard similarity of their chunk sets.
//!
//! Only flat `string` nodes are candidates: concatenated and sliced strings
//! share the memory of their parts, so counting them would count bytes twice.
//!
//! A snapshot may store only a prefix of a string. A prefix says nothing about
//! the rest (different files often share a license header), so such strings
//! are grouped only with copies of the same stored prefix and the same size.

use std::collections::{HashMap, HashSet};

use super::heap_graph::{AnalysisError, HeapGraph, NodeIndex};

const AVERAGE_CHUNK_BYTES: usize = 64;
const MIN_CHUNK_BYTES: usize = 16;
const MAX_CHUNK_BYTES: usize = 512;
const MINHASH_BANDS: usize = 32;
const MINHASH_ROWS: usize = 4;
const MINHASH_SIZE: usize = MINHASH_BANDS * MINHASH_ROWS;
/// Bounds the work for buckets of many strings that share a band, for example
/// thousands of responses with the same prefix.
const MAX_BUCKET_COMPARISONS: usize = 16;
/// Upper bound of V8's per-string header and alignment, used to tell a string
/// cut off by `--heap-snapshot-string-limit` from a complete one.
const STRING_HEADER_SLACK_BYTES: u64 = 32;

#[derive(Clone, Copy, Debug)]
pub struct SimilarStringOptions {
    /// Strings with a smaller shallow size are ignored.
    pub min_size: u64,
    /// Minimum byte-weighted Jaccard similarity of two chunk sets, in `0..=1`.
    pub threshold: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimilarStringAnalysis {
    pub candidate_count: u64,
    /// Candidates whose text the snapshot stores only partially.
    pub incomplete_text_count: u64,
    /// Sorted by redundant size, largest first.
    pub clusters: Vec<SimilarStringCluster>,
}

/// Strings that are identical or similar to each other. Contains at least two
/// strings.
#[derive(Clone, Debug, PartialEq)]
pub struct SimilarStringCluster {
    /// The representative (the largest string) comes first, then the other
    /// variants by decreasing similarity to it.
    pub variants: Vec<StringVariant>,
    pub total_size: u64,
    /// Estimated size if every chunk of text were stored once.
    pub unique_size: u64,
}

impl SimilarStringCluster {
    pub fn redundant_size(&self) -> u64 {
        self.total_size.saturating_sub(self.unique_size)
    }

    pub fn string_count(&self) -> u64 {
        self.variants.iter().map(|variant| variant.nodes.len() as u64).sum()
    }

    pub fn text_incomplete(&self) -> bool {
        self.variants.iter().any(|variant| variant.text_incomplete)
    }
}

/// Strings with identical stored text and shallow size.
#[derive(Clone, Debug, PartialEq)]
pub struct StringVariant {
    pub nodes: Vec<NodeIndex>,
    pub shallow_size: u64,
    /// Similarity to the cluster's representative; `1.0` for the representative.
    pub similarity: f64,
    /// The snapshot stores only a prefix of this text; it is grouped only with
    /// copies of the same prefix and size.
    pub text_incomplete: bool,
}

pub fn find_similar_strings(
    graph: &HeapGraph,
    options: SimilarStringOptions,
) -> Result<SimilarStringAnalysis, AnalysisError> {
    let texts = distinct_texts(graph, options.min_size)?;
    let candidate_count = texts.iter().map(|text| text.nodes.len() as u64).sum();
    let incomplete_text_count = texts
        .iter()
        .filter(|text| text.text_incomplete)
        .map(|text| text.nodes.len() as u64)
        .sum();
    let fingerprints: Vec<_> = texts.iter().map(|text| Fingerprint::new(text.text)).collect();

    let mut components = UnionFind::new(texts.len());
    let mut compared = HashSet::new();
    for bucket in lsh_buckets(&fingerprints).into_values() {
        for (position, &right) in bucket.iter().enumerate() {
            for &left in &bucket[position.saturating_sub(MAX_BUCKET_COMPARISONS)..position] {
                if components.find(left) != components.find(right)
                    && compared.insert((left, right))
                    && !texts[left].text_incomplete
                    && !texts[right].text_incomplete
                    && fingerprints[left].similarity(&fingerprints[right]) >= options.threshold
                {
                    components.union(left, right);
                }
            }
        }
    }

    let mut members = HashMap::<usize, Vec<usize>>::new();
    for index in 0..texts.len() {
        members.entry(components.find(index)).or_default().push(index);
    }
    let mut clusters: Vec<_> = members
        .into_values()
        .filter(|members| members.len() > 1 || texts[members[0]].nodes.len() > 1)
        .map(|members| cluster(&texts, &fingerprints, members))
        .collect();
    clusters.sort_by(|left, right| {
        right
            .redundant_size()
            .cmp(&left.redundant_size())
            .then(right.total_size.cmp(&left.total_size))
            .then(left.variants[0].nodes[0].cmp(&right.variants[0].nodes[0]))
    });
    Ok(SimilarStringAnalysis {
        candidate_count,
        incomplete_text_count,
        clusters,
    })
}

struct DistinctText<'a> {
    text: &'a str,
    shallow_size: u64,
    nodes: Vec<NodeIndex>,
    text_incomplete: bool,
}

fn distinct_texts(graph: &HeapGraph, min_size: u64) -> Result<Vec<DistinctText<'_>>, AnalysisError> {
    let mut texts = Vec::<DistinctText<'_>>::new();
    let mut by_key = HashMap::<(&str, u64), usize>::new();
    for summary in graph.node_summaries() {
        let summary = summary?;
        let Some(text) = summary.string_value else {
            continue;
        };
        if summary.node_type != "string" || summary.shallow_size < min_size || text.is_empty() {
            continue;
        }
        let index = *by_key.entry((text, summary.shallow_size)).or_insert_with(|| {
            texts.push(DistinctText {
                text,
                shallow_size: summary.shallow_size,
                nodes: Vec::new(),
                text_incomplete: stored_text_is_incomplete(text, summary.shallow_size),
            });
            texts.len() - 1
        });
        texts[index].nodes.push(summary.index);
    }
    Ok(texts)
}

/// V8 stores at most `--heap-snapshot-string-limit` characters (1024 by
/// default) of a string. A shallow size well above the stored characters
/// reveals the cut. Off-heap (external) strings cannot be checked this way.
fn stored_text_is_incomplete(text: &str, shallow_size: u64) -> bool {
    let units = text.encode_utf16().count() as u64;
    let bytes_per_unit = if text.chars().any(|character| u32::from(character) > 0xFF) {
        2
    } else {
        1
    };
    shallow_size > units * bytes_per_unit + STRING_HEADER_SLACK_BYTES
}

fn cluster(texts: &[DistinctText<'_>], fingerprints: &[Fingerprint], mut members: Vec<usize>) -> SimilarStringCluster {
    members.sort_by(|&left, &right| {
        texts[right]
            .shallow_size
            .cmp(&texts[left].shallow_size)
            .then(texts[left].nodes[0].cmp(&texts[right].nodes[0]))
    });
    let representative = &fingerprints[members[0]];
    let mut variants: Vec<_> = members
        .iter()
        .map(|&index| StringVariant {
            nodes: texts[index].nodes.clone(),
            shallow_size: texts[index].shallow_size,
            similarity: representative.similarity(&fingerprints[index]),
            text_incomplete: texts[index].text_incomplete,
        })
        .collect();
    variants[1..].sort_by(|left, right| right.similarity.total_cmp(&left.similarity));

    let mut seen = HashSet::new();
    let mut unique_size = 0_u64;
    for &index in &members {
        let fingerprint = &fingerprints[index];
        let novel_bytes: u64 = fingerprint
            .chunks
            .iter()
            .filter(|chunk| seen.insert(chunk.hash))
            .map(|chunk| u64::from(chunk.len))
            .sum();
        unique_size += scale(texts[index].shallow_size, novel_bytes, fingerprint.total_bytes);
    }
    SimilarStringCluster {
        total_size: variants
            .iter()
            .map(|variant| variant.shallow_size * variant.nodes.len() as u64)
            .sum(),
        unique_size,
        variants,
    }
}

fn scale(size: u64, numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return size;
    }
    u64::try_from(u128::from(size) * u128::from(numerator) / u128::from(denominator))
        .expect("numerator does not exceed denominator")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chunk {
    hash: u64,
    len: u32,
}

struct Fingerprint {
    /// Distinct chunks, sorted by hash.
    chunks: Vec<Chunk>,
    total_bytes: u64,
    minhash: [u64; MINHASH_SIZE],
}

impl Fingerprint {
    fn new(text: &str) -> Self {
        let mut chunks = content_defined_chunks(text.as_bytes());
        chunks.sort_by_key(|chunk| chunk.hash);
        chunks.dedup_by_key(|chunk| chunk.hash);
        let mut minhash = [u64::MAX; MINHASH_SIZE];
        for chunk in &chunks {
            for (row, value) in minhash.iter_mut().enumerate() {
                *value = (*value).min(mix64(chunk.hash ^ MINHASH_SEEDS[row]));
            }
        }
        Self {
            total_bytes: chunks.iter().map(|chunk| u64::from(chunk.len)).sum(),
            chunks,
            minhash,
        }
    }

    /// Byte-weighted Jaccard similarity of the two chunk sets.
    fn similarity(&self, other: &Self) -> f64 {
        let (mut left, mut right) = (0, 0);
        let mut shared = 0_u64;
        while left < self.chunks.len() && right < other.chunks.len() {
            let (a, b) = (self.chunks[left], other.chunks[right]);
            match a.hash.cmp(&b.hash) {
                std::cmp::Ordering::Less => left += 1,
                std::cmp::Ordering::Greater => right += 1,
                std::cmp::Ordering::Equal => {
                    shared += u64::from(a.len);
                    left += 1;
                    right += 1;
                }
            }
        }
        let union = self.total_bytes + other.total_bytes - shared;
        if union == 0 { 1.0 } else { shared as f64 / union as f64 }
    }
}

/// Groups fingerprints whose MinHash signatures agree on all rows of at least
/// one band. Two texts with similarity `s` share a band with probability
/// `1 - (1 - s^4)^32`: 0.87 at `s = 0.5`, above 0.999 at `s = 0.8`.
fn lsh_buckets(fingerprints: &[Fingerprint]) -> HashMap<u64, Vec<usize>> {
    let mut buckets = HashMap::<u64, Vec<usize>>::new();
    for (index, fingerprint) in fingerprints.iter().enumerate() {
        for (band, rows) in fingerprint.minhash.chunks_exact(MINHASH_ROWS).enumerate() {
            let key = rows
                .iter()
                .fold(mix64(band as u64), |key, &row| mix64(key ^ row));
            buckets.entry(key).or_default().push(index);
        }
    }
    buckets.retain(|_, members| members.len() > 1);
    buckets
}

/// Splits at positions where the Gear rolling hash has its top bits clear, so
/// boundaries depend on the preceding bytes only and survive insertions. The
/// top bits depend on the last 64 bytes; the low bits only on the last few.
fn content_defined_chunks(bytes: &[u8]) -> Vec<Chunk> {
    const MASK: u64 = !(u64::MAX >> AVERAGE_CHUNK_BYTES.trailing_zeros());
    let mut chunks = Vec::with_capacity(bytes.len() / AVERAGE_CHUNK_BYTES + 1);
    let mut start = 0;
    let mut rolling = 0_u64;
    for (index, &byte) in bytes.iter().enumerate() {
        rolling = (rolling << 1).wrapping_add(GEAR[usize::from(byte)]);
        let len = index + 1 - start;
        if (len >= MIN_CHUNK_BYTES && rolling & MASK == 0) || len >= MAX_CHUNK_BYTES {
            chunks.push(chunk(&bytes[start..=index]));
            start = index + 1;
        }
    }
    if start < bytes.len() {
        chunks.push(chunk(&bytes[start..]));
    }
    chunks
}

fn chunk(bytes: &[u8]) -> Chunk {
    let hash = bytes
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    Chunk {
        hash: mix64(hash),
        len: u32::try_from(bytes.len()).expect("chunks are at most MAX_CHUNK_BYTES"),
    }
}

/// The SplitMix64 finalizer.
const fn mix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

const fn splitmix_table<const N: usize>(seed: u64) -> [u64; N] {
    let mut table = [0; N];
    let mut index = 0;
    while index < N {
        table[index] = mix64(seed.wrapping_add((index as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)));
        index += 1;
    }
    table
}

const GEAR: [u64; 256] = splitmix_table(0x6765_6172);
const MINHASH_SEEDS: [u64; MINHASH_SIZE] = splitmix_table(0x6d69_6e68);

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn find(&mut self, mut index: usize) -> usize {
        while self.parent[index] != index {
            self.parent[index] = self.parent[self.parent[index]];
            index = self.parent[index];
        }
        index
    }

    fn union(&mut self, left: usize, right: usize) {
        let (left, right) = (self.find(left), self.find(right));
        if left != right {
            self.parent[left.max(right)] = left.min(right);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::heap::heap_graph::parse_heap_graph;

    /// Builds a graph whose nodes are `(type, text, shallow size)`; a synthetic
    /// root precedes them.
    fn graph(nodes: &[(&str, &str, u64)]) -> HeapGraph {
        let mut strings = vec!["root"];
        let mut fields = vec!["0,0,1,0,0".to_owned()];
        for (index, (node_type, text, size)) in nodes.iter().enumerate() {
            let node_type = ["synthetic", "string", "concatenated string"]
                .iter()
                .position(|candidate| candidate == node_type)
                .unwrap();
            strings.push(*text);
            fields.push(format!("{node_type},{},{},{size},0", index + 1, (index + 2) * 2));
        }
        let strings = serde_json::to_string(&strings).unwrap();
        let strings = &strings[1..strings.len() - 1];
        let json = format!(
            r#"{{"snapshot":{{"meta":{{
                "node_fields":["type","name","id","self_size","edge_count"],
                "node_types":[["synthetic","string","concatenated string"],"string","number","number","number"],
                "edge_fields":["type","name_or_index","to_node"],
                "edge_types":[["property"],"string_or_number","node"]}},
              "node_count":{},"edge_count":0}},
              "nodes":[{}],"edges":[],"strings":[{strings}]}}"#,
            fields.len(),
            fields.join(","),
        );
        parse_heap_graph(json.as_bytes()).unwrap()
    }

    fn file(lines: std::ops::Range<u32>) -> String {
        lines.map(|line| format!("line {line} of a moderately long file\n")).collect()
    }

    fn summary(analysis: &SimilarStringAnalysis) -> Vec<Vec<(Vec<u32>, String)>> {
        analysis
            .clusters
            .iter()
            .map(|cluster| {
                cluster
                    .variants
                    .iter()
                    .map(|variant| {
                        (
                            variant.nodes.iter().map(|node| node.0).collect(),
                            format!("{:.2}", variant.similarity),
                        )
                    })
                    .collect()
            })
            .collect()
    }

    const OPTIONS: SimilarStringOptions = SimilarStringOptions {
        min_size: 1024,
        threshold: 0.8,
    };

    #[test]
    fn groups_exact_copies_and_edited_copies() {
        let original = file(0..400);
        let inserted = format!("{}INSERTED LINE\n{}", file(0..200), file(200..400));
        let prefixed = format!("// header\n{original}");
        let unrelated = file(1000..1400).replace("line", "row");
        let size = |text: &str| text.len() as u64 + 16;
        let graph = graph(&[
            ("string", &original, size(&original)),
            ("string", &original, size(&original)),
            ("string", &inserted, size(&inserted)),
            ("string", &prefixed, size(&prefixed)),
            ("string", &unrelated, size(&unrelated)),
            ("concatenated string", &original, 32),
            ("string", "too small", 32),
        ]);
        let analysis = find_similar_strings(&graph, OPTIONS).unwrap();
        assert_eq!(
            (analysis.candidate_count, analysis.incomplete_text_count),
            (5, 0)
        );
        assert_eq!(
            summary(&analysis),
            vec![vec![
                (vec![3], "1.00".to_owned()),
                (vec![1, 2], "0.97".to_owned()),
                (vec![4], "0.96".to_owned()),
            ]]
        );
        let cluster = &analysis.clusters[0];
        assert_eq!(cluster.string_count(), 4);
        assert!(cluster.unique_size < cluster.total_size / 3);
    }

    #[test]
    fn exact_duplicates_form_a_cluster_and_dissimilar_strings_do_not() {
        let first = file(0..100);
        let second = file(100..200);
        let size = first.len() as u64 + 16;
        let graph = graph(&[
            ("string", &first, size),
            ("string", &second, size),
            ("string", &second, size),
        ]);
        let analysis = find_similar_strings(&graph, OPTIONS).unwrap();
        assert_eq!(summary(&analysis), vec![vec![(vec![2, 3], "1.00".to_owned())]]);
        assert_eq!(analysis.clusters[0].redundant_size(), size);
    }

    #[test]
    fn groups_partially_stored_strings_only_when_prefix_and_size_match() {
        let prefix = file(0..40);
        let graph = graph(&[
            ("string", &prefix[..1024], 86_912),
            ("string", &prefix[..1024], 86_900),
            ("string", &prefix[..1024], 86_912),
        ]);
        let analysis = find_similar_strings(&graph, OPTIONS).unwrap();
        assert_eq!(analysis.incomplete_text_count, 3);
        assert_eq!(summary(&analysis), vec![vec![(vec![1, 3], "1.00".to_owned())]]);
        assert!(analysis.clusters[0].text_incomplete());
        assert!(!stored_text_is_incomplete(&prefix[..1024], 1024 + 24));
        assert!(!stored_text_is_incomplete("Ω".repeat(600).as_str(), 1200 + 24));
        assert!(stored_text_is_incomplete("ü".repeat(600).as_str(), 1200 + 24));
    }
}

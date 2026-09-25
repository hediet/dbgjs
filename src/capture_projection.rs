use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use sourcemap::{DecodedMap, SourceMap, decode_slice};

use crate::service_api::{
    CaptureScriptProvenance, CoverageSnapshot, CpuProfileSnapshot, SourceLocation,
};

struct AvailableMap {
    generated: String,
    checkpoints: Vec<(u32, usize, u32, u32)>,
    map_url: String,
    map: SourceMap,
}

pub(crate) struct VerifiedLocalSources {
    pub generated: String,
    pub map_url: String,
    pub map_bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct CachedSourceMap {
    pub map_url: String,
    pub map_bytes: Vec<u8>,
}

fn local_file(url: &str) -> Option<PathBuf> {
    if let Ok(url) = url::Url::parse(url) {
        return (url.scheme() == "file")
            .then(|| url.to_file_path().ok())
            .flatten();
    }
    let path = PathBuf::from(url);
    path.is_absolute().then_some(path)
}

fn resolved_map_url(url: &str, map_ref: Option<&str>) -> Result<String, String> {
    let map_ref = map_ref.ok_or_else(|| {
        format!("{url}: source map reference unavailable; raw measurements retained")
    })?;
    if map_ref.starts_with("data:") {
        return Err(format!(
            "{url}: inline source map was not persisted; raw measurements retained"
        ));
    }
    if url::Url::parse(url).is_err() && PathBuf::from(map_ref).is_absolute() {
        return Ok(map_ref.to_owned());
    }
    url::Url::parse(map_ref)
        .or_else(|_| url::Url::parse(url).and_then(|base| base.join(map_ref)))
        .map(|url| url.to_string())
        .map_err(|_| format!("{url}: source map URL {map_ref} cannot be resolved"))
}

pub(crate) fn load_cached_source_map(
    generated_url: &str,
    map_ref: Option<&str>,
    script_hash: &str,
) -> Result<CachedSourceMap, String> {
    if script_hash.is_empty() {
        return Err(format!(
            "{generated_url}: script identity unavailable; raw measurements retained"
        ));
    }
    let map_url = resolved_map_url(generated_url, map_ref)?;
    let map_bytes = crate::cdp_runtime::read_source_map_cache_for_view(script_hash, &map_url)
        .ok_or_else(|| format!(
            "{generated_url}: verified source map cache entry for {map_url} is unavailable; raw measurements retained"
        ))?;
    Ok(CachedSourceMap { map_url, map_bytes })
}

pub(crate) fn recover_source_map_for_view(
    url: &str,
    map_ref: Option<&str>,
    generated_sha256: Option<&str>,
    script_hash: &str,
) -> Result<(Vec<u8>, String), String> {
    let local_error = if let Some(hash) = generated_sha256 {
        match load_verified_local_sources(url, map_ref, Some(hash)) {
            Ok(sources) => return Ok((sources.map_bytes, sources.map_url)),
            Err(error) => Some(error),
        }
    } else {
        None
    };
    match load_cached_source_map(url, map_ref, script_hash) {
        Ok(cached) => Ok((cached.map_bytes, cached.map_url)),
        Err(error) => Err(match local_error {
            Some(local_error) => format!("{local_error}; {error}"),
            None => error,
        }),
    }
}

fn load_map(
    provenance: Option<&CaptureScriptProvenance>,
    url: &str,
) -> Result<AvailableMap, String> {
    let Some(provenance) = provenance else {
        return Err(format!(
            "{url}: script provenance unavailable; raw measurements retained"
        ));
    };
    if provenance.url != url {
        return Err(format!(
            "{url}: captured script URL differs from measurement; raw measurements retained"
        ));
    }
    let sources = load_verified_local_sources(
        url,
        provenance.source_map_url.as_deref(),
        provenance.source_sha256.as_deref(),
    )?;
    let map = match decode_slice(&sources.map_bytes).map_err(|error| error.to_string())? {
        DecodedMap::Regular(map) => map,
        DecodedMap::Index(index) => index.flatten().map_err(|error| error.to_string())?,
        DecodedMap::Hermes(_) => {
            return Err(format!(
                "{url}: unsupported Hermes source map; raw measurements retained"
            ));
        }
    };
    let VerifiedLocalSources {
        generated, map_url, ..
    } = sources;
    let mut checkpoints = vec![(0, 0, 0, 0)];
    let (mut units, mut line, mut column) = (0_u32, 0_u32, 0_u32);
    for (byte, ch) in generated.char_indices() {
        if units.saturating_sub(checkpoints.last().unwrap().0) >= 1024 {
            checkpoints.push((units, byte, line, column));
        }
        units = units.saturating_add(ch.len_utf16() as u32);
        if ch == '\n' {
            line += 1;
            column = 0;
        } else {
            column += ch.len_utf16() as u32;
        }
    }
    Ok(AvailableMap {
        generated,
        checkpoints,
        map_url,
        map,
    })
}

pub(crate) fn load_verified_local_sources(
    url: &str,
    map_ref: Option<&str>,
    source_sha256: Option<&str>,
) -> Result<VerifiedLocalSources, String> {
    let source = local_file(url).ok_or_else(|| {
        format!("{url}: generated source is unavailable locally; raw measurements retained")
    })?;
    let bytes = fs::read(&source).map_err(|error| {
        format!("{url}: generated source unavailable ({error}); raw measurements retained")
    })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if source_sha256 != Some(actual.as_str()) {
        return Err(format!(
            "{url}: generated source identity unavailable or changed; raw measurements retained"
        ));
    }
    let generated = String::from_utf8(bytes)
        .map_err(|_| format!("{url}: generated source is not UTF-8; raw measurements retained"))?;
    let map_url = resolved_map_url(url, map_ref)?;
    let path = local_file(&map_url).ok_or_else(|| {
        format!("{url}: source map {map_url} is unavailable locally; raw measurements retained")
    })?;
    let generated_directory = fs::canonicalize(&source)
        .map_err(|error| format!("{url}: generated source path unavailable ({error})"))?
        .parent()
        .ok_or_else(|| format!("{url}: generated source has no parent directory"))?
        .to_owned();
    let map_path = fs::canonicalize(&path)
        .map_err(|error| format!("{url}: source map {map_url} unavailable ({error})"))?;
    if !map_path.starts_with(&generated_directory) {
        return Err(format!(
            "{url}: source map {map_url} is outside generated script directory; raw measurements retained"
        ));
    }
    let map_bytes = fs::read(map_path).map_err(|error| {
        format!("{url}: source map {map_url} unavailable ({error}); raw measurements retained")
    })?;
    Ok(VerifiedLocalSources {
        generated,
        map_url,
        map_bytes,
    })
}

impl AvailableMap {
    fn location(&self, line: u32, column: u32) -> Option<SourceLocation> {
        let token = self.map.lookup_token(line, column)?;
        if token.get_dst_line() != line {
            return None;
        }
        let source = token.get_source()?;
        let source_url =
            crate::source_view::canonical_source_uri(Some(&self.map_url), source).display();
        Some(SourceLocation {
            source_url,
            line: token.get_src_line().saturating_add(1),
            column: token.get_src_col().saturating_add(1),
        })
    }

    fn generated_position(&self, offset: u32) -> Option<(u32, u32)> {
        let checkpoint = self
            .checkpoints
            .partition_point(|(units, ..)| *units <= offset);
        let &(mut units, byte, mut line, mut column) = &self.checkpoints[checkpoint - 1];
        for ch in self.generated[byte..].chars() {
            if units >= offset {
                break;
            }
            units += ch.len_utf16() as u32;
            if ch == '\n' {
                line += 1;
                column = 0;
            } else {
                column += ch.len_utf16() as u32;
            }
        }
        (units == offset).then_some((line, column))
    }

    fn offset(&self, offset: u32) -> Option<SourceLocation> {
        let (line, column) = self.generated_position(offset)?;
        self.location(line, column)
    }
}

pub(crate) fn project_stored_coverage(snapshot: &mut CoverageSnapshot) {
    for source in &mut snapshot.sources {
        for function in &mut source.functions {
            if function.effective_ranges.is_empty() {
                function.effective_ranges =
                    crate::target_debugger::effective_coverage_ranges(&function.ranges);
            }
        }
        match load_map(source.provenance.as_ref(), &source.generated_url) {
            Ok(map) => {
                for function in &mut source.functions {
                    function.generated_location = map
                        .generated_position(function.root_start_offset)
                        .map(|(line, column)| SourceLocation {
                            source_url: source.generated_url.clone(),
                            line: line.saturating_add(1),
                            column: column.saturating_add(1),
                        });
                    for range in &mut function.ranges {
                        range.authored_start = map.offset(range.start_offset);
                        range.authored_end = map.offset(range.end_offset.saturating_sub(1));
                    }
                    for range in &mut function.effective_ranges {
                        range.authored_start = map.offset(range.start_offset);
                        range.authored_end = map.offset(range.end_offset.saturating_sub(1));
                    }
                    function.authored_location = function
                        .effective_ranges
                        .iter()
                        .find_map(|range| range.authored_start.clone());
                }
                let authored = source
                    .functions
                    .iter()
                    .filter_map(|function| {
                        function
                            .authored_location
                            .as_ref()
                            .map(|location| location.source_url.clone())
                    })
                    .collect::<std::collections::BTreeSet<_>>();
                if authored.len() == 1 {
                    source.associated_authored_source = authored.into_iter().next();
                }
            }
            Err(message) => {
                if source
                    .functions
                    .iter()
                    .all(|function| function.authored_location.is_none())
                {
                    snapshot.projection_diagnostics.push(message);
                }
            }
        }
    }
}

pub(crate) fn project_stored_cpu(
    snapshot: &mut CpuProfileSnapshot,
    source_path: Option<&str>,
) -> Result<(), crate::target_debugger::TargetDebuggerError> {
    if snapshot.samples.is_empty() && !snapshot.functions.is_empty() {
        // Legacy snapshots may have derived functions but no raw sample stream.
        if let Some(path) = source_path {
            snapshot.functions.retain(|function| {
                function
                    .authored_location
                    .as_ref()
                    .unwrap_or(&function.generated_location)
                    .source_url
                    .starts_with(path)
            });
            if snapshot.functions.is_empty() {
                snapshot.projection_diagnostics.push(format!(
                    "no persisted functions matched source prefix {path:?}; legacy capture has no raw samples to regroup"
                ));
            }
        }
        return Ok(());
    }
    let mut maps = BTreeMap::new();
    for node in &snapshot.nodes {
        let frame = &node.call_frame;
        if frame.script_id.is_empty() || maps.contains_key(&frame.script_id) {
            continue;
        }
        let result = load_map(snapshot.script_provenance.get(&frame.script_id), &frame.url);
        if let Err(message) = &result {
            if node.authored_location.is_none() {
                snapshot.projection_diagnostics.push(message.clone());
            }
        }
        maps.insert(frame.script_id.clone(), result);
    }
    for node in &mut snapshot.nodes {
        if snapshot
            .script_provenance
            .get(&node.call_frame.script_id)
            .is_some_and(|provenance| provenance.url != node.call_frame.url)
        {
            snapshot.projection_diagnostics.push(format!(
                "script {} changed URL within CPU profile; node {} retains generated location",
                node.call_frame.script_id, node.id
            ));
        }
        if source_path.is_some_and(|path| {
            node.authored_location
                .as_ref()
                .is_some_and(|location| !location.source_url.starts_with(path))
        }) {
            node.authored_location = None;
        }
        if snapshot
            .script_provenance
            .get(&node.call_frame.script_id)
            .is_some_and(|provenance| provenance.url == node.call_frame.url)
            && let Some(Ok(map)) = maps.get(&node.call_frame.script_id)
            && let (Ok(line), Ok(column)) = (
                u32::try_from(node.call_frame.line_number),
                u32::try_from(node.call_frame.column_number),
            )
        {
            let authored = map.location(line, column);
            if source_path.is_none_or(|path| {
                authored
                    .as_ref()
                    .is_some_and(|location| location.source_url.starts_with(path))
            }) {
                node.authored_location = authored;
            } else {
                node.authored_location = None;
            }
        }
    }
    crate::target_debugger::aggregate_cpu_profile(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> CaptureScriptProvenance {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/capture_projection/bundle.js");
        let bytes = fs::read(&path).unwrap();
        CaptureScriptProvenance {
            url: url::Url::from_file_path(path).unwrap().to_string(),
            source_map_url: Some("bundle.js.map".into()),
            source_sha256: Some(format!("{:x}", Sha256::digest(bytes))),
        }
    }

    #[test]
    fn shared_local_loader_returns_verified_view_inputs_without_persisting_inline_maps() {
        let provenance = fixture();
        let source = load_verified_local_sources(
            &provenance.url,
            provenance.source_map_url.as_deref(),
            provenance.source_sha256.as_deref(),
        )
        .unwrap();
        assert_eq!(source.generated, "a\nb\n");
        assert!(source.map_bytes.starts_with(b"{\"version\":3"));
        let error = load_verified_local_sources(
            &provenance.url,
            Some("data:application/json,{}"),
            provenance.source_sha256.as_deref(),
        )
        .err()
        .unwrap();
        assert!(error.contains("inline source map"));
        assert_eq!(
            resolved_map_url("/project/bundle.js", Some("/project/bundle.js.map")).unwrap(),
            "/project/bundle.js.map"
        );
        let unrelated =
            url::Url::from_file_path(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
                .unwrap()
                .to_string();
        let error = load_verified_local_sources(
            &provenance.url,
            Some(&unrelated),
            provenance.source_sha256.as_deref(),
        )
        .err()
        .unwrap();
        assert!(
            error.contains("outside generated script directory"),
            "{error}"
        );
        let (map_bytes, map_url) = recover_source_map_for_view(
            &provenance.url,
            provenance.source_map_url.as_deref(),
            provenance.source_sha256.as_deref(),
            "",
        )
        .unwrap();
        assert!(map_bytes.starts_with(b"{\"version\":3"));
        assert_eq!(map_url, source.map_url);
        let error = recover_source_map_for_view(
            &provenance.url,
            provenance.source_map_url.as_deref(),
            Some("stale-generated-source-hash"),
            "",
        )
        .err()
        .unwrap();
        assert!(error.contains("identity unavailable or changed"));
        assert!(error.contains("script identity unavailable"));
        assert!(
            load_cached_source_map(&provenance.url, provenance.source_map_url.as_deref(), "")
                .unwrap_err()
                .contains("script identity unavailable")
        );
        assert!(
            load_cached_source_map(
                &provenance.url,
                Some("data:application/json,{}"),
                "cdp-hash"
            )
            .unwrap_err()
            .contains("inline source map")
        );
    }

    fn coverage() -> CoverageSnapshot {
        let provenance = fixture();
        serde_json::from_value(json!({
            "timestampMicros": 10,
            "sources": [{
                "scriptId": "1", "generatedUrl": provenance.url,
                "provenance": provenance,
                "functions": [{
                    "name": "a", "blockCoverage": true, "rootStartOffset": 0, "rootEndOffset": 1,
                    "ranges": [{"startOffset": 0, "endOffset": 1, "count": 2}]
                }, {
                    "name": "b", "blockCoverage": true, "rootStartOffset": 2, "rootEndOffset": 3,
                    "ranges": [{"startOffset": 2, "endOffset": 3, "count": 1}]
                }]
            }]
        }))
        .unwrap()
    }

    fn cpu() -> CpuProfileSnapshot {
        let mut provenance = fixture();
        provenance.source_map_url = Some("same-source.js.map".into());
        serde_json::from_value(json!({
            "captureId": "cpu", "samplingIntervalMicros": null,
            "startTimeMicros": 0.0, "endTimeMicros": 30.0,
            "nodes": [{
                "id": 1, "callFrame": {"functionName": "(root)", "scriptId": "", "url": "", "lineNumber": -1, "columnNumber": -1},
                "hitCount": null, "children": [2,3], "deoptReason": null, "positionTicks": [],
                "authoredLocation": null, "breadcrumb": null, "selfTimeMicros": 0, "totalTimeMicros": 0, "sampleCount": 0
            },{
                "id": 2, "callFrame": {"functionName": "work", "scriptId": "1", "url": provenance.url, "lineNumber": 0, "columnNumber": 0},
                "hitCount": null, "children": [], "deoptReason": null, "positionTicks": [],
                "authoredLocation": null, "breadcrumb": null, "selfTimeMicros": 0, "totalTimeMicros": 0, "sampleCount": 0
            },{
                "id": 3, "callFrame": {"functionName": "work", "scriptId": "1", "url": provenance.url, "lineNumber": 1, "columnNumber": 0},
                "hitCount": null, "children": [], "deoptReason": null, "positionTicks": [],
                "authoredLocation": null, "breadcrumb": null, "selfTimeMicros": 0, "totalTimeMicros": 0, "sampleCount": 0
            }],
            "samples": [2,3], "timeDeltasMicros": [10,20],
            "scriptProvenance": {"1": provenance}
        })).unwrap()
    }

    #[test]
    fn raw_coverage_supports_two_views_without_changing_payload() {
        let raw = coverage();
        let payload = serde_json::to_vec(&raw).unwrap();
        let hash = Sha256::digest(&payload);
        let mut authored: CoverageSnapshot = serde_json::from_slice(&payload).unwrap();
        project_stored_coverage(&mut authored);
        assert_eq!(
            authored.sources[0].functions[0]
                .authored_location
                .as_ref()
                .unwrap()
                .line,
            1
        );
        assert_eq!(
            authored.sources[0].functions[1]
                .authored_location
                .as_ref()
                .unwrap()
                .line,
            1
        );
        let mut selected = authored.clone();
        crate::coverage_filter::filter_coverage(&mut selected, None, Some("**/a.ts")).unwrap();
        assert_eq!(selected.sources[0].functions.len(), 1);
        assert_eq!(selected.sources[0].functions[0].ranges[0].count, 2);
        assert_eq!(Sha256::digest(serde_json::to_vec(&raw).unwrap()), hash);
    }

    #[test]
    fn exclusion_applies_to_raw_cumulative_ranges_before_projection() {
        let selected = coverage();
        let mut baseline = selected.clone();
        baseline.sources[0].functions.pop();
        let mut delta = crate::target_debugger::exclude_coverage(selected, &baseline);
        assert_eq!(delta.sources[0].functions.len(), 1);
        project_stored_coverage(&mut delta);
        assert_eq!(delta.sources[0].functions[0].name, "b");
        assert_eq!(delta.sources[0].functions[0].ranges[0].count, 1);
    }

    #[test]
    fn cpu_view_regroups_same_samples_by_selected_source_policy() {
        let raw = cpu();
        let payload = serde_json::to_vec(&raw).unwrap();
        let mut authored: CpuProfileSnapshot = serde_json::from_slice(&payload).unwrap();
        project_stored_cpu(&mut authored, None).unwrap();
        assert_eq!(
            authored
                .functions
                .iter()
                .filter(|f| f.name == "work")
                .count(),
            1
        );
        let work = authored
            .functions
            .iter()
            .find(|f| f.name == "work")
            .unwrap();
        assert_eq!(work.self_time_micros, 30);
        assert_eq!(work.sample_count, 2);
        let mut selected = raw.clone();
        project_stored_cpu(&mut selected, Some("file:///not-selected")).unwrap();
        assert_eq!(
            selected
                .functions
                .iter()
                .filter(|f| f.name == "work")
                .count(),
            2
        );
        assert_eq!(
            selected
                .functions
                .iter()
                .filter(|f| f.name == "work")
                .filter(|f| f.authored_location.is_none())
                .count(),
            2
        );
        assert_eq!(serde_json::to_vec(&raw).unwrap(), payload);
        assert_eq!(authored.samples, raw.samples);
        assert_eq!(authored.time_deltas_micros, raw.time_deltas_micros);
    }

    #[test]
    fn missing_or_mismatched_source_keeps_measurements_and_reports_unavailable() {
        let mut raw = coverage();
        raw.sources[0].provenance.as_mut().unwrap().source_sha256 = None;
        project_stored_coverage(&mut raw);
        assert_eq!(raw.sources[0].functions[0].ranges[0].count, 2);
        assert!(raw.sources[0].functions[0].authored_location.is_none());
        assert!(raw.projection_diagnostics[0].contains("identity unavailable"));
        let mut profile = cpu();
        profile.script_provenance.clear();
        project_stored_cpu(&mut profile, None).unwrap();
        assert_eq!(profile.samples, vec![2, 3]);
        assert!(!profile.projection_diagnostics.is_empty());

        let mut mismatched = cpu();
        mismatched.nodes[2].call_frame.url = "file:///different.js".into();
        project_stored_cpu(&mut mismatched, None).unwrap();
        assert!(mismatched.nodes[2].authored_location.is_none());
        assert!(
            mismatched
                .projection_diagnostics
                .iter()
                .any(|message| message.contains("changed URL"))
        );
    }

    #[test]
    fn legacy_cpu_functions_without_raw_samples_remain_readable() {
        let mut profile = cpu();
        project_stored_cpu(&mut profile, None).unwrap();
        let functions = profile.functions.clone();
        profile.nodes.clear();
        profile.samples.clear();
        profile.time_deltas_micros.clear();
        let mut restored: CpuProfileSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        project_stored_cpu(&mut restored, None).unwrap();
        assert_eq!(restored.functions, functions);
        project_stored_cpu(&mut restored, Some("file:///no-such-source")).unwrap();
        assert!(restored.functions.is_empty());
        assert!(restored.projection_diagnostics[0].contains("no raw samples"));
    }

    #[test]
    fn generated_offsets_use_utf16_columns_and_reject_half_surrogates() {
        let provenance = fixture();
        let mut map = load_map(Some(&provenance), &provenance.url).unwrap();
        map.generated = "😀a".into();
        map.checkpoints = vec![(0, 0, 0, 0), (2, 4, 0, 2)];
        assert_eq!(map.generated_position(1), None);
        assert_eq!(map.generated_position(2), Some((0, 2)));
        assert_eq!(map.generated_position(3), Some((0, 3)));
    }
}

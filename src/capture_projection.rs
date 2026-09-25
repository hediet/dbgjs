use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use reqwest::redirect::Policy;
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

const MAX_VIEW_RESOURCE_BYTES: usize = crate::cdp_runtime::MAX_VIEW_SOURCE_MAP_BYTES;

pub(crate) type PreparedViewSources = BTreeMap<(String, String), VerifiedLocalSources>;

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
    crate::debugger_engine::local_script_file_path(url)
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
    if !map_ref.contains("://") && local_file(map_ref).is_some() {
        return Ok(map_ref.to_owned());
    }
    if let Ok(absolute) = url::Url::parse(map_ref)
        && absolute.scheme().len() > 1
    {
        return Ok(absolute.to_string());
    }
    if !url.contains("://") && let Some(source) = local_file(url) {
        return source
            .parent()
            .map(|directory| directory.join(map_ref).to_string_lossy().into_owned())
            .ok_or_else(|| format!("{url}: source map URL {map_ref} cannot be resolved"));
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
    let generated = if map_ref.is_none() {
        generated_sha256.and_then(|hash| load_verified_generated_file(url, Some(hash)).ok())
    } else {
        None
    };
    let map_ref = map_ref.or_else(|| generated.as_deref().and_then(source_mapping_reference));
    if let Some(reference) = map_ref.filter(|reference| reference.starts_with("data:"))
        && generated.is_some()
    {
        if reference.len() > MAX_VIEW_RESOURCE_BYTES {
            return Err(format!("{url}: inline source map exceeds view resource limit; raw measurements retained"));
        }
        return crate::cdp_runtime::decode_source_map_data_url(reference)
            .map(|bytes| (bytes, url.to_owned()))
            .map_err(|error| format!("{url}: inline source map unavailable ({error}); raw measurements retained"));
    }
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
    needs_generated_source: bool,
    prepared: Option<&VerifiedLocalSources>,
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
    let (generated, map_url, map_bytes) = if let Some(sources) = prepared {
        (sources.generated.clone(), sources.map_url.clone(), sources.map_bytes.clone())
    } else {
        let local_generated = local_file(url)
            .map(|_| load_verified_generated_file(url, provenance.source_sha256.as_deref()))
            .transpose()?;
        let map_ref = provenance
            .source_map_url
            .as_deref()
            .or_else(|| local_generated.as_deref().and_then(source_mapping_reference));
        let (map_bytes, map_url) = recover_map_for_view(
            url,
            map_ref,
            provenance.source_sha256.as_deref(),
        )?;
        let generated = if needs_generated_source {
            local_generated.ok_or_else(|| format!("{url}: generated source is unavailable locally; raw measurements retained"))?
        } else {
            String::new()
        };
        (generated, map_url, map_bytes)
    };
    let map = match decode_slice(&map_bytes)
        .map_err(|error| format!("{url}: source map could not be decoded ({error}); raw measurements retained"))?
    {
        DecodedMap::Regular(map) => map,
        DecodedMap::Index(index) => index.flatten()
            .map_err(|error| format!("{url}: source map could not be flattened ({error}); raw measurements retained"))?,
        DecodedMap::Hermes(_) => {
            return Err(format!(
                "{url}: unsupported Hermes source map; raw measurements retained"
            ));
        }
    };
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

fn recover_map_for_view(
    url: &str,
    map_ref: Option<&str>,
    hash: Option<&str>,
) -> Result<(Vec<u8>, String), String> {
    if let Some(reference) = map_ref.filter(|reference| reference.starts_with("data:")) {
        if reference.len() > MAX_VIEW_RESOURCE_BYTES {
            return Err(format!("{url}: inline source map exceeds view resource limit; raw measurements retained"));
        }
        let bytes = crate::cdp_runtime::decode_source_map_data_url(reference)
            .map_err(|error| format!("{url}: inline source map unavailable ({error}); raw measurements retained"))?;
        return Ok((bytes, url.to_owned()));
    }
    recover_source_map_for_view(url, map_ref, hash, hash.unwrap_or_default())
}

fn source_mapping_reference(source: &str) -> Option<&str> {
    source.lines().rev().find_map(|line| {
        let line = line.trim();
        ["//# sourceMappingURL=", "//@ sourceMappingURL=", "/*# sourceMappingURL="]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .map(|reference| reference.trim_end_matches("*/").trim())
            .filter(|reference| !reference.is_empty())
    })
}

async fn fetch_view_resource(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let parsed = url::Url::parse(url).map_err(|error| error.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("{url}: only HTTP(S) view resources can be fetched"));
    }
    let mut response = client
        .get(parsed)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| format!("{url}: view resource unavailable ({error})"))?;
    if response.status().is_redirection() {
        return Err(format!("{url}: view resource redirected; raw measurements retained"));
    }
    if response.content_length().is_some_and(|length| length > MAX_VIEW_RESOURCE_BYTES as u64) {
        return Err(format!("{url}: view resource exceeds {MAX_VIEW_RESOURCE_BYTES} bytes"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_VIEW_RESOURCE_BYTES {
            return Err(format!("{url}: view resource exceeds {MAX_VIEW_RESOURCE_BYTES} bytes"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn fetch_verified_view_sources(
    client: &reqwest::Client,
    provenance: &CaptureScriptProvenance,
) -> Result<VerifiedLocalSources, String> {
    let url = &provenance.url;
    let generated = if local_file(url).is_some() {
        load_verified_generated_file(url, provenance.source_sha256.as_deref())?
    } else {
        let expected = provenance.source_sha256.as_deref()
            .ok_or_else(|| format!("{url}: generated source identity unavailable; raw measurements retained"))?;
        let bytes = fetch_view_resource(client, url).await?;
        if format!("{:x}", Sha256::digest(&bytes)) != expected {
            return Err(format!("{url}: generated source identity changed; raw measurements retained"));
        }
        String::from_utf8(bytes)
            .map_err(|_| format!("{url}: generated source is not UTF-8; raw measurements retained"))?
    };
    let map_ref = provenance.source_map_url.as_deref()
        .or_else(|| source_mapping_reference(&generated));
    let (map_bytes, map_url) = if map_ref.is_some_and(|reference| reference.starts_with("data:")) {
        recover_map_for_view(url, map_ref, provenance.source_sha256.as_deref())?
    } else if let Ok(cached) = recover_map_for_view(url, map_ref, provenance.source_sha256.as_deref()) {
        cached
    } else {
        let map_url = resolved_map_url(url, map_ref)?;
        let bytes = fetch_view_resource(client, &map_url).await?;
        if let Some(hash) = provenance.source_sha256.as_deref()
            && let Err(error) = crate::cdp_runtime::cache_source_map_for_view(hash, &map_url, bytes.clone()).await
        {
            eprintln!("{error}");
        }
        (bytes, map_url)
    };
    Ok(VerifiedLocalSources { generated, map_url, map_bytes })
}

pub(crate) async fn prepare_view_sources<'a>(
    scripts: impl Iterator<Item = (&'a str, &'a CaptureScriptProvenance)>,
    needs_generated_source: bool,
) -> (PreparedViewSources, Vec<String>) {
    let mut prepared = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => return (prepared, vec![format!("view HTTP client unavailable: {error}")]),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    for (id, provenance) in scripts {
        let key = (id.to_owned(), provenance.url.clone());
        if prepared.contains_key(&key) || load_map(Some(provenance), &provenance.url, needs_generated_source, None).is_ok() {
            continue;
        }
        match tokio::time::timeout_at(deadline, fetch_verified_view_sources(&client, provenance)).await {
            Ok(Ok(sources)) => { prepared.insert(key, sources); }
            Ok(Err(error)) => diagnostics.push(format!("{error}; raw measurements retained")),
            Err(_) => {
                diagnostics.push(format!("{}: view reconstruction timed out; raw measurements retained", provenance.url));
                break;
            }
        }
    }
    (prepared, diagnostics)
}

fn load_verified_generated_file(url: &str, source_sha256: Option<&str>) -> Result<String, String> {
    if source_sha256.is_none() {
        return Err(format!(
            "{url}: generated source identity unavailable; raw measurements retained"
        ));
    }
    let source = local_file(url).ok_or_else(|| {
        format!("{url}: generated source is unavailable locally; raw measurements retained")
    })?;
    if fs::metadata(&source).is_ok_and(|metadata| metadata.len() > MAX_VIEW_RESOURCE_BYTES as u64) {
        return Err(format!("{url}: generated source exceeds view resource limit; raw measurements retained"));
    }
    let bytes = fs::read(&source).map_err(|error| {
        format!("{url}: generated source unavailable ({error}); raw measurements retained")
    })?;
    if bytes.len() > MAX_VIEW_RESOURCE_BYTES {
        return Err(format!("{url}: generated source exceeds view resource limit; raw measurements retained"));
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if source_sha256 != Some(actual.as_str()) {
        return Err(format!(
            "{url}: generated source identity unavailable or changed; raw measurements retained"
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| format!("{url}: generated source is not UTF-8; raw measurements retained"))
}

pub(crate) fn load_verified_local_sources(
    url: &str,
    map_ref: Option<&str>,
    source_sha256: Option<&str>,
) -> Result<VerifiedLocalSources, String> {
    let generated = load_verified_generated_file(url, source_sha256)?;
    let source = local_file(url).ok_or_else(|| {
        format!("{url}: generated source is unavailable locally; raw measurements retained")
    })?;
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
    if fs::metadata(&map_path).is_ok_and(|metadata| metadata.len() > MAX_VIEW_RESOURCE_BYTES as u64) {
        return Err(format!("{url}: source map exceeds view resource limit; raw measurements retained"));
    }
    let map_bytes = fs::read(map_path).map_err(|error| {
        format!("{url}: source map {map_url} unavailable ({error}); raw measurements retained")
    })?;
    if map_bytes.len() > MAX_VIEW_RESOURCE_BYTES {
        return Err(format!("{url}: source map exceeds view resource limit; raw measurements retained"));
    }
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

#[cfg(test)]
pub(crate) fn project_stored_coverage(snapshot: &mut CoverageSnapshot) {
    project_stored_coverage_with_sources(snapshot, &BTreeMap::new());
}

pub(crate) fn project_stored_coverage_with_sources(
    snapshot: &mut CoverageSnapshot,
    prepared: &PreparedViewSources,
) {
    for source in &mut snapshot.sources {
        let Some(provenance) = source.provenance.as_ref() else {
            continue;
        };
        for function in &mut source.functions {
            if function.effective_ranges.is_empty() {
                function.effective_ranges =
                    crate::target_debugger::effective_coverage_ranges(&function.ranges);
            }
        }
        match load_map(Some(provenance), &source.generated_url, true, prepared.get(&(source.script_id.clone(), source.generated_url.clone()))) {
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

#[cfg(test)]
pub(crate) fn project_stored_cpu(
    snapshot: &mut CpuProfileSnapshot,
    source_path: Option<&str>,
) -> Result<(), crate::target_debugger::TargetDebuggerError> {
    project_stored_cpu_with_sources(snapshot, source_path, &BTreeMap::new())
}

pub(crate) fn project_stored_cpu_with_sources(
    snapshot: &mut CpuProfileSnapshot,
    source_path: Option<&str>,
    prepared: &PreparedViewSources,
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
        let result = load_map(snapshot.script_provenance.get(&frame.script_id), &frame.url, false, prepared.get(&(frame.script_id.clone(), frame.url.clone())));
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

    #[test]
    fn vscode_app_file_urls_and_native_paths_resolve_only_on_the_host_platform() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("generated script.js");
        fs::write(&path, b"work();").unwrap();
        let file_url = url::Url::from_file_path(&path).unwrap();
        let vscode_url = format!("vscode-file://vscode-app{}", file_url.path());
        assert_eq!(local_file(&vscode_url).as_deref(), Some(path.as_path()));
        assert_eq!(local_file(file_url.as_str()).as_deref(), Some(path.as_path()));
        assert_eq!(local_file(path.to_str().unwrap()).as_deref(), Some(path.as_path()));
        assert_eq!(
            resolved_map_url(path.to_str().unwrap(), Some("https://cdn.example/app.js.map")).unwrap(),
            "https://cdn.example/app.js.map"
        );
        assert_eq!(
            resolved_map_url(
                "vscode-file://vscode-app/d:/workspace/workbench.js",
                Some("workbench.js.map")
            ).unwrap(),
            "vscode-file://vscode-app/d:/workspace/workbench.js.map"
        );
        #[cfg(windows)]
        {
            assert_eq!(
                local_file("vscode-file://vscode-app/d:/workspace/workbench.js"),
                Some(PathBuf::from(r"d:\workspace\workbench.js"))
            );
            assert_eq!(
                local_file(r"d:\workspace\preload.js"),
                Some(PathBuf::from(r"d:\workspace\preload.js"))
            );
            assert_eq!(
                resolved_map_url(r"d:\workspace\preload.js", Some("preload.js.map")).unwrap(),
                r"d:\workspace\preload.js.map"
            );
            assert_eq!(
                resolved_map_url(r"d:\workspace\preload.js", Some("https://cdn.example/preload.js.map")).unwrap(),
                "https://cdn.example/preload.js.map"
            );
        }
        #[cfg(not(windows))]
        assert!(local_file("vscode-file://vscode-app/d:/workspace/workbench.js").is_none());
    }

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
    fn cached_map_projects_cpu_without_generated_file_and_coverage_with_verified_file() {
        let mut profile = cpu();
        let mut coverage = coverage();
        let provenance = coverage.sources[0].provenance.as_mut().unwrap();
        let map_url = format!("cached-view-{}.map", std::process::id());
        provenance.source_map_url = Some(map_url.clone());
        let cpu_provenance = profile.script_provenance.get_mut("1").unwrap();
        cpu_provenance.source_map_url = Some(map_url);
        cpu_provenance.url = "https://example.invalid/app.js".into();
        for node in &mut profile.nodes {
            if node.call_frame.script_id == "1" {
                node.call_frame.url = cpu_provenance.url.clone();
            }
        }
        let source_map = fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/capture_projection/bundle.js.map"),
        )
        .unwrap();
        let cached = format!("dbgjs-source-map-v1\n{:x}\n", Sha256::digest(&source_map));
        let cpu_map_url = format!("https://example.invalid/cached-view-{}.map", std::process::id());
        let cpu_path = crate::cdp_runtime::source_map_cache_path_for_test(
            cpu_provenance.source_sha256.as_deref().unwrap(),
            &cpu_map_url,
        );
        let coverage_map_url = resolved_map_url(&provenance.url, provenance.source_map_url.as_deref()).unwrap();
        let coverage_path = crate::cdp_runtime::source_map_cache_path_for_test(
            provenance.source_sha256.as_deref().unwrap(),
            &coverage_map_url,
        );
        for path in [&cpu_path, &coverage_path] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut bytes = cached.as_bytes().to_vec();
            bytes.extend_from_slice(&source_map);
            fs::write(path, bytes).unwrap();
        }
        project_stored_cpu(&mut profile, None).unwrap();
        project_stored_coverage(&mut coverage);
        fs::remove_file(cpu_path).unwrap();
        fs::remove_file(coverage_path).unwrap();
        assert!(profile.nodes[1].authored_location.is_some());
        assert!(coverage.sources[0].functions[0].authored_location.is_some());
    }

    #[test]
    fn omitted_inline_map_is_recovered_from_verified_generated_file_at_view_time() {
        let directory = tempfile::tempdir().unwrap();
        let map = fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/capture_projection/bundle.js.map"),
        )
        .unwrap();
        let source = format!(
            "a\nb\n//# sourceMappingURL=data:application/json;base64,{}\n",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, map.as_bytes())
        );
        let path = directory.path().join("bundle.js");
        fs::write(&path, &source).unwrap();
        let mut snapshot = coverage();
        let url = url::Url::from_file_path(path).unwrap().to_string();
        snapshot.sources[0].generated_url = url.clone();
        let provenance = snapshot.sources[0].provenance.as_mut().unwrap();
        provenance.url = url;
        provenance.source_map_url = None;
        provenance.source_sha256 = Some(format!("{:x}", Sha256::digest(source.as_bytes())));
        let (recovered, map_url) = recover_source_map_for_view(
            &provenance.url, None, provenance.source_sha256.as_deref(), "",
        ).unwrap();
        assert_eq!(recovered, map.as_bytes());
        assert_eq!(map_url, provenance.url);
        let raw = serde_json::to_vec(&snapshot).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("base64,"));
        project_stored_coverage(&mut snapshot);
        assert!(snapshot.sources[0].functions[0].authored_location.is_some());
    }

    #[tokio::test]
    async fn cold_http_view_fetches_verified_source_and_map_without_capture_requests() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let generated = b"a\nb\n".to_vec();
        let map = fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/capture_projection/bundle.js.map"),
        )
        .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).await.unwrap();
                server_hits.fetch_add(1, Ordering::SeqCst);
                let missing = request[..size].starts_with(b"GET /missing.js.map ");
                let oversized = request[..size].starts_with(b"GET /oversized.js.map ");
                let body: &[u8] = if request[..size].starts_with(b"GET /bundle.js.map ") {
                    &map
                } else if missing || oversized {
                    &[]
                } else {
                    &generated
                };
                stream.write_all(
                    format!("HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        if missing { "404 Not Found" } else { "200 OK" },
                        if oversized { MAX_VIEW_RESOURCE_BYTES + 1 } else { body.len() },
                    ).as_bytes(),
                ).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let mut profile = cpu();
        let url = format!("{base}/bundle.js");
        for node in &mut profile.nodes {
            if node.call_frame.script_id == "1" {
                node.call_frame.url = url.clone();
            }
        }
        let provenance = profile.script_provenance.get_mut("1").unwrap();
        provenance.url = url;
        provenance.source_map_url = Some("bundle.js.map".into());
        let payload = serde_json::to_vec(&profile).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let (prepared, diagnostics) = prepare_view_sources(
            profile.script_provenance.iter().map(|(id, provenance)| (id.as_str(), provenance)),
            false,
        ).await;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        project_stored_cpu_with_sources(&mut profile, None, &prepared).unwrap();
        assert!(profile.nodes[1].authored_location.is_some());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let mut second: CpuProfileSnapshot = serde_json::from_slice(&payload).unwrap();
        let (prepared, diagnostics) = prepare_view_sources(
            second.script_provenance.iter().map(|(id, provenance)| (id.as_str(), provenance)),
            false,
        ).await;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        project_stored_cpu_with_sources(&mut second, None, &prepared).unwrap();
        assert!(second.nodes[1].authored_location.is_some());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let mut coverage = coverage();
        coverage.sources[0].generated_url = format!("{base}/bundle.js");
        coverage.sources[0].provenance.as_mut().unwrap().url = coverage.sources[0].generated_url.clone();
        coverage.sources[0].provenance.as_mut().unwrap().source_map_url = Some("bundle.js.map".into());
        let (prepared, diagnostics) = prepare_view_sources(
            coverage.sources.iter().filter_map(|source| source.provenance.as_ref()
                .map(|provenance| (source.script_id.as_str(), provenance))),
            true,
        ).await;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        project_stored_coverage_with_sources(&mut coverage, &prepared);
        assert!(coverage.sources[0].functions[0].authored_location.is_some());
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        for (reference, expected) in [
            ("missing.js.map", "404"),
            ("oversized.js.map", "exceeds"),
        ] {
            let mut missing: CpuProfileSnapshot = serde_json::from_slice(&payload).unwrap();
            missing.script_provenance.get_mut("1").unwrap().source_map_url = Some(reference.into());
            let (prepared, errors) = prepare_view_sources(
                missing.script_provenance.iter().map(|(id, provenance)| (id.as_str(), provenance)),
                false,
            ).await;
            assert!(prepared.is_empty());
            assert!(errors.iter().any(|error| error.contains(expected)), "{errors:?}");
            missing.projection_diagnostics.extend(errors);
            project_stored_cpu_with_sources(&mut missing, None, &prepared).unwrap();
            assert!(missing.nodes[1].authored_location.is_none());
            assert_eq!(missing.samples, vec![2, 3]);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 7);
        let mut mismatched: CpuProfileSnapshot = serde_json::from_slice(&payload).unwrap();
        mismatched.script_provenance.get_mut("1").unwrap().source_sha256 = Some("0".repeat(64));
        let (prepared, errors) = prepare_view_sources(
            mismatched.script_provenance.iter().map(|(id, provenance)| (id.as_str(), provenance)),
            false,
        ).await;
        assert!(prepared.is_empty());
        assert!(errors.iter().any(|error| error.contains("identity changed")), "{errors:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 8);
        let cache_path = crate::cdp_runtime::source_map_cache_path_for_test(
            coverage.sources[0].provenance.as_ref().unwrap().source_sha256.as_deref().unwrap(),
            &format!("{base}/bundle.js.map"),
        );
        fs::remove_file(&cache_path).unwrap();
        let _ = fs::remove_file(cache_path.with_extension("access"));
        server.abort();
    }

    #[tokio::test]
    async fn file_script_recovers_remote_map_and_rejects_changed_source_before_request() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let map_url = format!("http://{}/cdn-map.js.map", listener.local_addr().unwrap());
        let map = fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/capture_projection/bundle.js.map"),
        )
        .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                stream.read(&mut request).await.unwrap();
                server_hits.fetch_add(1, Ordering::SeqCst);
                stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    map.len()
                ).as_bytes()).await.unwrap();
                stream.write_all(&map).await.unwrap();
            }
        });
        let mut snapshot = coverage();
        let generated_url = url::Url::parse(&snapshot.sources[0].generated_url).unwrap();
        let vscode_url = format!("vscode-file://vscode-app{}", generated_url.path());
        snapshot.sources[0].generated_url = vscode_url.clone();
        let provenance = snapshot.sources[0].provenance.as_mut().unwrap();
        provenance.url = vscode_url.clone();
        provenance.source_map_url = Some(map_url.clone());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let (prepared, errors) = prepare_view_sources(
            snapshot.sources.iter().filter_map(|source| source.provenance.as_ref()
                .map(|provenance| (source.script_id.as_str(), provenance))),
            true,
        ).await;
        assert!(errors.is_empty(), "{errors:?}");
        project_stored_coverage_with_sources(&mut snapshot, &prepared);
        assert!(snapshot.sources[0].functions[0].authored_location.is_some());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let mut changed = coverage();
        changed.sources[0].generated_url = vscode_url.clone();
        changed.sources[0].provenance.as_mut().unwrap().url = vscode_url;
        changed.sources[0].provenance.as_mut().unwrap().source_map_url = Some(map_url.clone());
        changed.sources[0].provenance.as_mut().unwrap().source_sha256 = Some("0".repeat(64));
        let (prepared, errors) = prepare_view_sources(
            changed.sources.iter().filter_map(|source| source.provenance.as_ref()
                .map(|provenance| (source.script_id.as_str(), provenance))),
            true,
        ).await;
        changed.projection_diagnostics.extend(errors);
        project_stored_coverage_with_sources(&mut changed, &prepared);
        assert!(changed.sources[0].functions[0].authored_location.is_none());
        assert!(changed.projection_diagnostics.iter().any(|error| error.contains("identity")));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let path = crate::cdp_runtime::source_map_cache_path_for_test(
            snapshot.sources[0].provenance.as_ref().unwrap().source_sha256.as_deref().unwrap(),
            &map_url,
        );
        fs::remove_file(&path).unwrap();
        let _ = fs::remove_file(path.with_extension("access"));
        server.abort();
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
        let mut map = load_map(Some(&provenance), &provenance.url, true, None).unwrap();
        map.generated = "😀a".into();
        map.checkpoints = vec![(0, 0, 0, 0), (2, 4, 0, 2)];
        assert_eq!(map.generated_position(1), None);
        assert_eq!(map.generated_position(2), Some((0, 2)));
        assert_eq!(map.generated_position(3), Some((0, 3)));
    }
}

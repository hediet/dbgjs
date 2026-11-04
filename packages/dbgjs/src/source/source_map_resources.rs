use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::source::source_view::SourceMapData;

pub(crate) const SOURCE_MAP_RESOURCE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const MAX_VIEW_SOURCE_MAP_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceMapDataUrlError {
    #[error("invalid source-map data URL: {0}")]
    Invalid(String),
    #[error("invalid base64 source map: {0}")]
    Base64(base64::DecodeError),
}

pub(crate) fn decode_source_map_data_url(url: &str) -> Result<Vec<u8>, SourceMapDataUrlError> {
    let data = url
        .strip_prefix("data:")
        .ok_or_else(|| SourceMapDataUrlError::Invalid(url.to_owned()))?;
    let (metadata, payload) = data
        .split_once(',')
        .ok_or_else(|| SourceMapDataUrlError::Invalid(url.to_owned()))?;
    if metadata
        .split(';')
        .any(|component| component.eq_ignore_ascii_case("base64"))
    {
        BASE64_STANDARD
            .decode(payload)
            .map_err(SourceMapDataUrlError::Base64)
    } else {
        Ok(percent_encoding::percent_decode_str(payload).collect())
    }
}

pub(crate) fn resolve_source_map_url(
    generated_url: &str,
    source_map_url: &str,
) -> Result<String, url::ParseError> {
    if let Ok(url) = url::Url::parse(source_map_url) {
        return Ok(url.into());
    }
    url::Url::parse(generated_url)?
        .join(source_map_url)
        .map(String::from)
}

pub(crate) fn local_script_file_path(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Some(path.to_owned());
    }
    let url = url::Url::parse(value).ok()?;
    match url.scheme() {
        "file" => url.to_file_path().ok(),
        "vscode-file" if url.host_str() == Some("vscode-app") => {
            let decoded = percent_encoding::percent_decode_str(url.path())
                .decode_utf8()
                .ok()?;
            #[cfg(windows)]
            let decoded = decoded
                .strip_prefix('/')
                .filter(|path| path.as_bytes().get(1) == Some(&b':'))
                .unwrap_or(&decoded);
            #[cfg(not(windows))]
            if decoded.as_bytes().get(2) == Some(&b':') {
                return None;
            }
            #[cfg(windows)]
            let path = PathBuf::from(decoded);
            #[cfg(not(windows))]
            let path = PathBuf::from(decoded.as_ref());
            path.is_absolute().then_some(path)
        }
        _ => None,
    }
}

pub(crate) fn source_map_cache_path(script_hash: &str, resolved_url: &str) -> Option<PathBuf> {
    let directory = if let Some(path) = env::var_os("DBGJS_SOURCE_MAP_CACHE") {
        PathBuf::from(path)
    } else if let Some(state_file) = env::var_os("DBGJS_SERVICE_STATE") {
        PathBuf::from(state_file)
            .parent()
            .map(|parent| parent.join("source-map-cache"))?
    } else if let Some(path) = env::var_os("LOCALAPPDATA") {
        PathBuf::from(path).join("dbgjs").join("source-map-cache")
    } else if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(path).join("dbgjs").join("source-map-cache")
    } else if let Some(path) = env::var_os("HOME") {
        PathBuf::from(path)
            .join(".cache")
            .join("dbgjs")
            .join("source-map-cache")
    } else {
        return None;
    };
    let mut hasher = Sha256::new();
    hasher.update(script_hash.as_bytes());
    hasher.update([0]);
    hasher.update(resolved_url.as_bytes());
    Some(directory.join(format!("{:x}.map", hasher.finalize())))
}

#[cfg(test)]
pub(crate) fn source_map_cache_path_for_test(script_hash: &str, resolved_url: &str) -> PathBuf {
    source_map_cache_path(script_hash, resolved_url).expect("test cache directory")
}

pub(crate) fn read_source_map_cache_for_view(
    script_hash: &str,
    resolved_url: &str,
) -> Option<Vec<u8>> {
    let path = source_map_cache_path(script_hash, resolved_url)?;
    read_source_map_cache_file(&path)
}

pub(crate) async fn cache_source_map_for_view(
    script_hash: &str,
    resolved_url: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let map = SourceMapData::new(bytes);
    if !map.is_supported() {
        return Err(format!("{resolved_url}: unsupported source map"));
    }
    let path = source_map_cache_path(script_hash, resolved_url)
        .ok_or_else(|| "source map cache directory unavailable".to_owned())?;
    write_source_map_cache(&path, &map)
        .await
        .map_err(|error| format!("failed to cache {resolved_url}: {error}"))
}

fn read_source_map_cache_file(path: &Path) -> Option<Vec<u8>> {
    if std::fs::metadata(path).ok()?.len() > (MAX_VIEW_SOURCE_MAP_BYTES + 128) as u64 {
        return None;
    }
    let cached = std::fs::read(path).ok()?;
    if cached.len() > MAX_VIEW_SOURCE_MAP_BYTES + 128 {
        return None;
    }
    decode_source_map_cache(cached).map(|map| map.to_vec())
}

pub(crate) async fn write_source_map_cache(
    path: &Path,
    source_map: &SourceMapData,
) -> Result<(), std::io::Error> {
    const MAGIC: &[u8] = b"dbgjs-source-map-v1\n";
    static TEMPORARY_ID: AtomicU64 = AtomicU64::new(1);
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "source-map cache path has no parent",
        ));
    };
    tokio::fs::create_dir_all(parent).await?;
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let digest = format!("{}\n", source_map.content_hash());
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(MAGIC).await?;
    file.write_all(digest.as_bytes()).await?;
    file.write_all(source_map).await?;
    file.sync_data().await?;
    drop(file);
    match tokio::fs::rename(&temporary, path).await {
        Ok(()) => {
            if let Err(error) = tokio::fs::write(path.with_extension("access"), []).await {
                eprintln!(
                    "failed to create source-map cache access marker {}: {error}",
                    path.display()
                );
            }
            cleanup_source_map_cache(parent).await;
            Ok(())
        }
        Err(_error) if path.exists() => {
            tokio::fs::remove_file(&temporary).await?;
            Ok(())
        }

        Err(error) => {
            if let Err(remove_error) = tokio::fs::remove_file(&temporary).await
                && remove_error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "failed to clean temporary source-map cache entry {}: {remove_error}",
                    temporary.display()
                );
            }
            Err(error)
        }
    }
}

async fn cleanup_source_map_cache(directory: &Path) {
    const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    const MAX_ENTRIES: usize = 64;
    const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

    let Ok(mut directory_entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    let now = SystemTime::now();
    let mut entries = Vec::new();
    while let Ok(Some(entry)) = directory_entries.next_entry().await {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "access")
        {
            if !path.with_extension("map").exists()
                && let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "failed to remove orphaned source-map access marker {}: {error}",
                    path.display()
                );
            }
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if path.extension().is_some_and(|extension| extension == "tmp") {
            let stale = now
                .duration_since(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH))
                .is_ok_and(|age| age > Duration::from_secs(60 * 60));
            if stale
                && let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "failed to remove stale source-map cache entry {}: {error}",
                    path.display()
                );
            }
            continue;
        }
        let access_path = path.with_extension("access");
        let modified = tokio::fs::metadata(&access_path)
            .await
            .and_then(|metadata| metadata.modified())
            .or_else(|_| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let expired = now.duration_since(modified).is_ok_and(|age| age > MAX_AGE);
        if expired {
            if let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "failed to remove stale source-map cache entry {}: {error}",
                    path.display()
                );
            }
            let _ = tokio::fs::remove_file(access_path).await;
            continue;
        }
        entries.push((modified, metadata.len(), path, access_path));
    }
    entries.sort_by_key(|(modified, _, _, _)| *modified);
    let mut total_bytes = entries.iter().map(|(_, size, _, _)| *size).sum::<u64>();
    let remove_count = entries.len().saturating_sub(MAX_ENTRIES);
    for (index, (_, size, path, access_path)) in entries.into_iter().enumerate() {
        if index >= remove_count && total_bytes <= MAX_BYTES {
            break;
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                total_bytes = total_bytes.saturating_sub(size);
                let _ = tokio::fs::remove_file(access_path).await;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                total_bytes = total_bytes.saturating_sub(size);
                let _ = tokio::fs::remove_file(access_path).await;
            }
            Err(error) => eprintln!(
                "failed to prune source-map cache entry {}: {error}",
                path.display()
            ),
        }
    }
}

pub(crate) fn decode_source_map_cache(mut bytes: Vec<u8>) -> Option<SourceMapData> {
    const MAGIC: &[u8] = b"dbgjs-source-map-v1\n";
    let remainder = bytes.strip_prefix(MAGIC)?;
    let newline = remainder.iter().position(|byte| *byte == b'\n')?;
    let expected: [u8; 64] = remainder[..newline].try_into().ok()?;
    let payload_start = MAGIC.len() + newline + 1;
    drop(bytes.drain(..payload_start));
    let source_map = SourceMapData::new(bytes);
    if source_map.content_hash().to_string().as_bytes() != expected.as_slice() {
        return None;
    }
    source_map.is_supported().then_some(source_map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_source_maps_against_the_generated_script() {
        assert_eq!(
            resolve_source_map_url(
                "https://cdn.example.com/assets/app.js",
                "../maps/app.js.map"
            )
            .unwrap(),
            "https://cdn.example.com/maps/app.js.map"
        );
    }

    #[test]
    fn preserves_absolute_source_map_urls() {
        assert_eq!(
            resolve_source_map_url(
                "https://cdn.example.com/assets/app.js",
                "https://maps.example.com/app.js.map"
            )
            .unwrap(),
            "https://maps.example.com/app.js.map"
        );
    }

    #[test]
    fn normalizes_backslashes_in_absolute_source_map_urls() {
        assert_eq!(
            resolve_source_map_url(
                "vscode-file://vscode-app/c:/resources/app/out/vs/workbench/workbench.js",
                "https://main.vscode-cdn.net/sourcemaps/commit/core/vs\\workbench\\workbench.js.map"
            )
            .unwrap(),
            "https://main.vscode-cdn.net/sourcemaps/commit/core/vs/workbench/workbench.js.map"
        );
    }

    #[test]
    fn decodes_inline_source_maps_and_preserves_errors() {
        assert_eq!(
            decode_source_map_data_url("data:application/json;BASE64,e30=").unwrap(),
            b"{}"
        );
        assert_eq!(
            decode_source_map_data_url("data:application/json,%7B%7D").unwrap(),
            b"{}"
        );
        assert!(matches!(
            decode_source_map_data_url("data:application/json"),
            Err(SourceMapDataUrlError::Invalid(_))
        ));
        assert!(matches!(
            decode_source_map_data_url("data:application/json;base64,!"),
            Err(SourceMapDataUrlError::Base64(_))
        ));
    }

    #[test]
    fn local_paths_decode_file_and_vscode_urls_without_accepting_remote_urls() {
        let path = std::env::current_dir()
            .unwrap()
            .join("source with spaces.js");
        assert_eq!(
            local_script_file_path(&path.to_string_lossy()),
            Some(path.clone())
        );
        assert_eq!(
            local_script_file_path(url::Url::from_file_path(&path).unwrap().as_str()),
            Some(path)
        );
        assert!(local_script_file_path("relative.js").is_none());
        assert!(local_script_file_path("https://example.com/source.js").is_none());
        assert!(local_script_file_path("vscode-file://other/source.js").is_none());
        #[cfg(not(windows))]
        {
            assert_eq!(
                local_script_file_path("vscode-file://vscode-app/workspace/source%20file.js"),
                Some(PathBuf::from("/workspace/source file.js"))
            );
            assert!(local_script_file_path("vscode-file://vscode-app/c:/source.js").is_none());
        }
        #[cfg(windows)]
        assert_eq!(
            local_script_file_path("vscode-file://vscode-app/c:/workspace/source%20file.js"),
            Some(PathBuf::from(r"c:\workspace\source file.js"))
        );
    }

    #[test]
    fn source_map_cache_identity_includes_script_hash_and_url() {
        let first = source_map_cache_path("script-a", "https://example.com/app.js.map").unwrap();
        let changed_script =
            source_map_cache_path("script-b", "https://example.com/app.js.map").unwrap();
        let changed_url =
            source_map_cache_path("script-a", "https://example.com/other.js.map").unwrap();
        assert_ne!(first, changed_script);
        assert_ne!(first, changed_url);
    }

    #[test]
    fn source_map_cache_payload_is_content_verified() {
        let source_map = br#"{"version":3,"sources":[],"names":[],"mappings":""}"#;
        let mut cached = b"dbgjs-source-map-v1\n".to_vec();
        cached.extend(format!("{:x}\n", Sha256::digest(source_map)).as_bytes());
        cached.extend(source_map);
        assert_eq!(
            decode_source_map_cache(cached.clone()).as_deref(),
            Some(source_map.as_slice())
        );
        *cached.last_mut().unwrap() ^= 1;
        assert!(decode_source_map_cache(cached).is_none());

        let invalid = b"<html>temporary CDN error</html>";
        let mut cached = b"dbgjs-source-map-v1\n".to_vec();
        cached.extend(format!("{:x}\n", Sha256::digest(invalid)).as_bytes());
        cached.extend(invalid);
        assert!(decode_source_map_cache(cached).is_none());
    }

    #[test]
    fn stored_view_reads_only_integrity_checked_existing_map_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("script.map");
        let source_map = br#"{"version":3,"sources":["source.ts"],"names":[],"mappings":"AAAA"}"#;
        let mut cached =
            format!("dbgjs-source-map-v1\n{:x}\n", Sha256::digest(source_map)).into_bytes();
        cached.extend_from_slice(source_map);
        std::fs::write(&path, &cached).unwrap();
        assert_eq!(read_source_map_cache_file(&path), Some(source_map.to_vec()));
        *cached.last_mut().unwrap() ^= 1;
        std::fs::write(&path, cached).unwrap();
        assert!(read_source_map_cache_file(&path).is_none());
    }

    #[test]
    fn stored_view_rejects_cache_entries_above_the_resource_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized.map");
        std::fs::File::create(&path)
            .unwrap()
            .set_len((MAX_VIEW_SOURCE_MAP_BYTES + 129) as u64)
            .unwrap();
        assert!(read_source_map_cache_file(&path).is_none());
    }

    #[tokio::test]
    async fn cache_cleanup_preserves_in_progress_temporary_map_writes() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("source.123.tmp");
        tokio::fs::write(&temporary, b"in-progress").await.unwrap();
        cleanup_source_map_cache(directory.path()).await;
        assert_eq!(tokio::fs::read(&temporary).await.unwrap(), b"in-progress");
    }

    #[tokio::test]
    async fn source_map_cache_reuses_the_digest_without_changing_the_disk_format() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.map");
        let bytes = br#"{"version":3,"sources":[],"names":[],"mappings":""}"#;
        let source_map = SourceMapData::new(bytes.as_slice());
        assert!(source_map.is_supported());
        write_source_map_cache(&path, &source_map).await.unwrap();
        let cached = tokio::fs::read(&path).await.unwrap();
        let mut expected =
            format!("dbgjs-source-map-v1\n{:x}\n", Sha256::digest(bytes)).into_bytes();
        expected.extend(bytes);
        assert_eq!(cached, expected);
        let restored = decode_source_map_cache(cached).unwrap();
        assert_eq!(restored.content_hash(), source_map.content_hash());
        assert_eq!(restored, source_map);
    }

    #[tokio::test]
    async fn source_map_cache_cleanup_bounds_retained_hashes() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..65 {
            tokio::fs::write(directory.path().join(format!("{index}.map")), [index as u8])
                .await
                .unwrap();
            tokio::fs::write(directory.path().join(format!("{index}.access")), [])
                .await
                .unwrap();
        }
        cleanup_source_map_cache(directory.path()).await;
        let mut entries = tokio::fs::read_dir(directory.path()).await.unwrap();
        let mut maps = 0;
        while let Some(entry) = entries.next_entry().await.unwrap() {
            maps += usize::from(
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "map"),
            );
        }
        assert_eq!(maps, 64);
    }
}

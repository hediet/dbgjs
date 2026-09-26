use globset::{GlobBuilder, GlobMatcher};

use crate::api::service_api::{CoverageRangeSnapshot, CoverageSnapshot};

pub fn filter_coverage(
    snapshot: &mut CoverageSnapshot,
    path_prefix: Option<&str>,
    path_glob: Option<&str>,
) -> Result<(), String> {
    CoveragePathFilter::new(path_prefix, path_glob)?.apply(snapshot)
}

pub struct CoveragePathFilter {
    _prefix: Option<String>,
    _glob: Option<GlobMatcher>,
}

impl CoveragePathFilter {
    pub fn new(prefix: Option<&str>, glob: Option<&str>) -> Result<Self, String> {
        if prefix.is_some() && glob.is_some() {
            return Err("coverage path prefix and glob are mutually exclusive".to_owned());
        }
        let glob = glob
            .map(|pattern| {
                GlobBuilder::new(&normalize_path(pattern))
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| format!("invalid coverage path glob '{pattern}': {error}"))
            })
            .transpose()?;
        Ok(Self {
            _prefix: prefix.map(normalize_path),
            _glob: glob,
        })
    }

    pub fn apply(&self, snapshot: &mut CoverageSnapshot) -> Result<(), String> {
        if self._prefix.is_none() && self._glob.is_none() {
            return Ok(());
        }
        snapshot.sources.retain_mut(|source| {
            if self.matches(&source.generated_url) {
                return true;
            }
            let associated_matches = source
                .associated_authored_source
                .as_deref()
                .is_some_and(|path| self.matches(path));
            let had_functions = !source.functions.is_empty();
            source.functions.retain_mut(|function| {
                let function_matches = function
                    .authored_location
                    .as_ref()
                    .map(|location| self.matches(&location.source_url));
                let fallback = function_matches.unwrap_or(associated_matches);
                function.ranges.retain(|range| self.matches_range(range, fallback));
                function
                    .effective_ranges
                    .retain(|range| self.matches_range(range, fallback));
                function_matches.unwrap_or(false)
                    || !function.ranges.is_empty()
                    || !function.effective_ranges.is_empty()
            });
            if !associated_matches {
                source.associated_authored_source = None;
            }
            !source.functions.is_empty() || (!had_functions && associated_matches)
        });
        if snapshot.sources.is_empty() {
            return Err(if let Some(prefix) = &self._prefix {
                format!("No executed functions matched source URL prefix {prefix:?}. Prefixes match from the beginning of the URL; use --path-glob '**/issue/**' to match a directory anywhere.")
            } else if let Some(glob) = &self._glob {
                format!("No executed functions matched source URL glob {:?}.", glob.glob().glob())
            } else {
                unreachable!("unfiltered coverage returns before applying a path filter")
            });
        }
        Ok(())
    }

    fn matches(&self, path: &str) -> bool {
        let path = normalize_path(path);
        self._prefix.as_ref().is_none_or(|prefix| {
            path == *prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| prefix.ends_with('/') || suffix.starts_with('/'))
        }) && self._glob.as_ref().is_none_or(|glob| glob.is_match(&path))
    }

    fn matches_range(&self, range: &CoverageRangeSnapshot, fallback: bool) -> bool {
        match (&range.authored_start, &range.authored_end) {
            (None, None) => fallback,
            (start, end) => start
                .iter()
                .chain(end.iter())
                .any(|location| self.matches(&location.source_url)),
        }
    }
}

pub(crate) fn normalize_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while let Some(remainder) = path.strip_prefix("../").or_else(|| path.strip_prefix("./")) {
        path = remainder.to_owned();
    }
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_match_diagnostics_identify_the_filter_and_explain_prefixes() {
        let prefix_error = CoveragePathFilter::new(Some("issue"), None).unwrap()
            .apply(&mut mixed_bundle()).unwrap_err();
        assert!(prefix_error.contains("prefix \"issue\""), "{prefix_error}");
        assert!(prefix_error.contains("--path-glob"), "{prefix_error}");
        let glob_error = CoveragePathFilter::new(None, Some("**/missing/**")).unwrap()
            .apply(&mut mixed_bundle()).unwrap_err();
        assert!(glob_error.contains("glob \"**/missing/**\""), "{glob_error}");
    }

    fn mixed_bundle() -> CoverageSnapshot {
        serde_json::from_value(json!({
            "timestampMicros": 1,
            "sources": [{
                "scriptId": "1", "generatedUrl": "file:///dist/bundle.js",
                "associatedAuthoredSource": "src/selected/a.ts",
                "functions": (["src/selected/a.ts", "src/other/b.ts"].map(|path| json!({
                    "name": path, "blockCoverage": true,
                    "rootStartOffset": 0, "rootEndOffset": 10,
                    "authoredLocation": {"sourceUrl": path, "line": 1, "column": 1},
                    "ranges": [{"startOffset": 0, "endOffset": 10, "count": 1,
                        "authoredStart": {"sourceUrl": path, "line": 1, "column": 1},
                        "authoredEnd": {"sourceUrl": path, "line": 1, "column": 10}}],
                    "effectiveRanges": []
                })))
            }]
        })).unwrap()
    }

    #[test]
    fn filters_functions_inside_mixed_authored_bundles() {
        let mut snapshot = mixed_bundle();
        filter_coverage(&mut snapshot, Some(r"src\selected"), None).unwrap();
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(snapshot.sources[0].functions.len(), 1);
        assert_eq!(snapshot.sources[0].functions[0].name, "src/selected/a.ts");
    }

    #[test]
    fn filters_ranges_before_dropping_their_containing_function() {
        let mut snapshot = mixed_bundle();
        let selected = snapshot.sources[0].functions[0].ranges[0].clone();
        snapshot.sources[0].functions.remove(0);
        snapshot.sources[0].functions[0].effective_ranges.push(selected);
        filter_coverage(&mut snapshot, Some("src/selected"), None).unwrap();
        assert!(snapshot.sources[0].functions[0].ranges.is_empty());
        assert_eq!(snapshot.sources[0].functions[0].effective_ranges.len(), 1);
    }

    #[test]
    fn explicit_errors_for_invalid_globs_and_missing_paths() {
        assert!(filter_coverage(&mut mixed_bundle(), None, Some("[")).unwrap_err().contains("invalid"));
        let missing = filter_coverage(&mut mixed_bundle(), Some("src/select"), None).unwrap_err();
        assert!(missing.contains("No executed functions matched"), "{missing}");
        assert!(missing.contains("prefix \"src/select\""), "{missing}");
        assert!(filter_coverage(&mut mixed_bundle(), Some("src"), Some("**/*.ts")).unwrap_err().contains("mutually exclusive"));
    }

    #[test]
    fn glob_filters_authored_paths_inside_mixed_bundles() {
        let mut snapshot = mixed_bundle();
        filter_coverage(&mut snapshot, None, Some("**/selected/*.ts")).unwrap();
        assert_eq!(snapshot.sources[0].functions.len(), 1);
        assert_eq!(snapshot.sources[0].functions[0].name, "src/selected/a.ts");
    }

    #[test]
    fn generated_bundle_selection_preserves_all_functions() {
        let mut snapshot = mixed_bundle();
        let expected = snapshot.clone();
        filter_coverage(&mut snapshot, Some(r"file:\\\dist"), None).unwrap();
        assert_eq!(snapshot, expected);
    }
}

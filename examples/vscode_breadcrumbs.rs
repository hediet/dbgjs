use std::{env, error::Error, fs, hint::black_box, path::Path, time::Instant};

use dbgjs::language_intelligence::SymbolIndex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let workload_path = args.next().ok_or(
        "usage: vscode_breadcrumbs <workload.json> [--iterations <count>] [--baseline <result.json>]",
    )?;
    let mut iterations = 2_usize;
    let mut baseline_path = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--iterations" => {
                iterations = args.next().ok_or("missing iteration count")?.parse()?;
                if iterations == 0 {
                    return Err("iteration count must be positive".into());
                }
            }
            "--baseline" => baseline_path = Some(args.next().ok_or("missing baseline path")?),
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }
    let workload_path = Path::new(&workload_path);
    let manifest = fs::read(workload_path)?;
    let workload: Workload = serde_json::from_slice(&manifest)?;
    let source = fs::read_to_string(
        workload_path
            .parent()
            .unwrap()
            .join("workbench.desktop.main.js"),
    )?;
    workload.validate(&source)?;
    let workload_sha256 = sha256(&manifest);
    let baseline = baseline_path
        .map(|path| -> Result<ReplayResult, Box<dyn Error>> {
            Ok(serde_json::from_slice(&fs::read(path)?)?)
        })
        .transpose()?;
    if let Some(baseline) = &baseline
        && (baseline.version != 1 || baseline.workload_sha256 != workload_sha256)
    {
        return Err("baseline belongs to a different workload".into());
    }

    let started = Instant::now();
    let index = SymbolIndex::new(&workload.source_file, &source)
        .ok_or("could not parse workload source")?;
    let index_build_seconds = started.elapsed().as_secs_f64();
    let mut results = Vec::new();
    let mut lookup_seconds = Vec::new();
    for iteration in 0..iterations {
        let started = Instant::now();
        let current = workload
            .lookups
            .iter()
            .map(|location| {
                index.breadcrumb(
                    black_box(&source),
                    black_box(location.line),
                    black_box(location.column),
                )
            })
            .collect::<Vec<_>>();
        lookup_seconds.push(started.elapsed().as_secs_f64());
        if iteration == 0 {
            if let Some(baseline) = &baseline {
                verify_results(&baseline.results, &current)?;
            }
            results = current;
        } else {
            verify_results(&results, &current)?;
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&ReplayResult {
            version: 1,
            workload_sha256,
            debug_assertions: cfg!(debug_assertions),
            architecture: env::consts::ARCH.to_owned(),
            index_build_seconds,
            lookup_seconds,
            results,
        })?
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Workload {
    version: u32,
    source_file: String,
    source_sha256: String,
    vscode_commit: String,
    lookups: Vec<Lookup>,
}

impl Workload {
    fn validate(&self, source: &str) -> Result<(), Box<dyn Error>> {
        if self.version != 1 {
            return Err(format!("unsupported workload version: {}", self.version).into());
        }
        if self.source_file != "workbench.desktop.main.js" {
            return Err("sourceFile must be workbench.desktop.main.js".into());
        }
        if self.vscode_commit.is_empty() || self.lookups.is_empty() {
            return Err("workload needs a VS Code commit and at least one lookup".into());
        }
        if sha256(source.as_bytes()) != self.source_sha256 {
            return Err("bundle SHA-256 differs from the recorded workload".into());
        }
        let line_lengths = source
            .split('\n')
            .map(|line| line.trim_end_matches('\r').encode_utf16().count())
            .collect::<Vec<_>>();
        for lookup in &self.lookups {
            let valid = lookup
                .line
                .checked_sub(1)
                .and_then(|line| line_lengths.get(line as usize))
                .is_some_and(|length| lookup.column > 0 && lookup.column as usize <= length + 1);
            if !valid {
                return Err(
                    format!("invalid UTF-16 position {}:{}", lookup.line, lookup.column).into(),
                );
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lookup {
    line: u32,
    column: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplayResult {
    version: u32,
    workload_sha256: String,
    debug_assertions: bool,
    architecture: String,
    index_build_seconds: f64,
    lookup_seconds: Vec<f64>,
    results: Vec<Option<String>>,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_results(
    expected: &[Option<String>],
    actual: &[Option<String>],
) -> Result<(), Box<dyn Error>> {
    if expected != actual {
        let mismatch = expected.iter().zip(actual).position(|(a, b)| a != b);
        return Err(format!(
            "breadcrumb results changed: expected {} entries, got {}; first mismatch: {mismatch:?}",
            expected.len(),
            actual.len()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload(source: &str) -> Workload {
        Workload {
            version: 1,
            source_file: "workbench.desktop.main.js".into(),
            source_sha256: sha256(source.as_bytes()),
            vscode_commit: "fixture".into(),
            lookups: vec![Lookup { line: 1, column: 1 }],
        }
    }

    #[test]
    fn validates_frozen_source_and_utf16_positions() {
        let source = "const x = '\u{1f600}';\r\nfunction example() {}";
        let mut input = workload(source);
        assert!(input.validate(source).is_ok());
        assert!(input.validate("different source").is_err());
        input.lookups = vec![Lookup { line: 0, column: 1 }];
        assert!(input.validate(source).is_err());
        input.lookups = vec![Lookup {
            line: 1,
            column: 999,
        }];
        assert!(input.validate(source).is_err());
        input.lookups = vec![Lookup { line: 2, column: 1 }];
        assert!(input.validate(source).is_ok());
        input.source_file = "../outside.js".into();
        assert!(input.validate(source).is_err());
    }

    #[test]
    fn detects_changed_missing_and_reordered_results() {
        let expected = vec![
            Some("Example.first".into()),
            None,
            Some("Example.second".into()),
        ];
        assert!(verify_results(&expected, &expected).is_ok());
        assert!(verify_results(&expected, &expected[..2]).is_err());
        let mut reordered = expected.clone();
        reordered.swap(0, 2);
        assert!(verify_results(&expected, &reordered).is_err());
        let mut changed = expected.clone();
        changed[1] = Some("new".into());
        assert!(verify_results(&expected, &changed).is_err());
    }

    #[test]
    fn rejects_empty_or_unknown_workloads() {
        let source = "function example() {}";
        let mut input = workload(source);
        input.version = 2;
        assert!(input.validate(source).is_err());
        input.version = 1;
        input.lookups.clear();
        assert!(input.validate(source).is_err());
    }

    #[test]
    fn reuses_the_production_symbol_index() {
        let source = "class Example { method() { return 1; } }";
        let index = SymbolIndex::new("workbench.desktop.main.js", source).unwrap();
        assert_eq!(
            index.breadcrumb(source, 1, 30),
            Some("Example.method".into())
        );
    }
}

//! Bounded fixtures exercise the production store and endpoints, not an alternative serializer.

use super::*;
use crate::api::service_api::{
    CaptureScriptProvenance, HeapMappingSnapshot, HeapMappingStatus, HeapScriptSnapshot,
};
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FUNCTIONS: usize = 128;
const SAMPLES: usize = 16_384;
const HEAP_OBJECTS: usize = 1_024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Traffic {
    source_requests: u64,
    source_bytes: u64,
    map_requests: u64,
    map_bytes: u64,
}

impl Traffic {
    fn since(self, previous: Self) -> Self {
        Self {
            source_requests: self.source_requests - previous.source_requests,
            source_bytes: self.source_bytes - previous.source_bytes,
            map_requests: self.map_requests - previous.map_requests,
            map_bytes: self.map_bytes - previous.map_bytes,
        }
    }
}

struct FixtureServer {
    base: String,
    source: String,
    mode: Arc<AtomicU64>,
    counts: Arc<[AtomicU64; 4]>,
    task: tokio::task::JoinHandle<()>,
}

impl FixtureServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let source = format!("class a {{}}\n{}", "// fixture padding\n".repeat(1_024));
        let mode = Arc::new(AtomicU64::new(0));
        let counts = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
        let server_source = source.clone();
        let server_mode = mode.clone();
        let server_counts = counts.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1_024];
                while !request.windows(4).any(|chunk| chunk == b"\r\n\r\n") {
                    let length = stream.read(&mut buffer).await.unwrap();
                    if length == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..length]);
                    assert!(request.len() <= 16_384, "unexpected fixture request");
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let is_map = path.ends_with(".map");
                let mode = server_mode.load(Ordering::SeqCst);
                let body = if is_map {
                    serde_json::json!({
                        "version": 3, "file": path.trim_start_matches('/').trim_end_matches(".map"),
                        "sources": ["original.ts"],
                        "sourcesContent": ["class Original {}\n"],
                        "names": [], "mappings": "AAAA"
                    })
                    .to_string()
                } else if mode == 1 {
                    "class Changed {}".into()
                } else if mode == 2 {
                    String::new()
                } else {
                    server_source.clone()
                };
                let index = if is_map { 2 } else { 0 };
                server_counts[index].fetch_add(1, Ordering::SeqCst);
                server_counts[index + 1].fetch_add(body.len() as u64, Ordering::SeqCst);
                let status = if !is_map && mode == 2 {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).await.unwrap();
                stream.write_all(body.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        Self {
            base,
            source,
            mode,
            counts,
            task,
        }
    }

    fn provenance(&self, name: &str) -> CaptureScriptProvenance {
        CaptureScriptProvenance {
            url: format!("{}/{name}.js", self.base),
            source_map_url: Some(format!("{}/{name}.js.map", self.base)),
            source_sha256: Some(format!("{:x}", Sha256::digest(self.source.as_bytes()))),
        }
    }

    fn traffic(&self) -> Traffic {
        let values: [u64; 4] = std::array::from_fn(|i| self.counts[i].load(Ordering::SeqCst));
        Traffic {
            source_requests: values[0],
            source_bytes: values[1],
            map_requests: values[2],
            map_bytes: values[3],
        }
    }

    fn clear_map(&self, name: &str) {
        let provenance = self.provenance(name);
        let path = crate::source::source_map_resources::source_map_cache_path_for_test(
            provenance.source_sha256.as_deref().unwrap(),
            provenance.source_map_url.as_deref().unwrap(),
        );
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
        for name in ["coverage", "cpu", "heap"] {
            self.clear_map(name);
        }
    }
}

fn coverage_fixture(provenance: CaptureScriptProvenance) -> CoverageSnapshot {
    let functions: Vec<_> = (0..FUNCTIONS)
        .map(|index| {
            serde_json::json!({
                "name": format!("work{index}"), "blockCoverage": true,
                "rootStartOffset": 0, "rootEndOffset": 10,
                "ranges": [
                    { "startOffset": 0, "endOffset": 10, "count": index + 1 },
                    { "startOffset": 2, "endOffset": 8, "count": index % 3 }
                ]
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "timestampMicros": 42,
        "sources": [{
            "scriptId": "7", "generatedUrl": provenance.url,
            "provenance": provenance, "functions": functions
        }]
    }))
    .unwrap()
}

fn cpu_fixture(provenance: CaptureScriptProvenance) -> CpuProfileSnapshot {
    let nodes: Vec<_> = (1..=FUNCTIONS)
        .map(|id| {
            serde_json::json!({
                "id": id, "callFrame": {
                    "functionName": format!("work{id}"), "scriptId": "7",
                    "url": provenance.url, "lineNumber": 0, "columnNumber": 0
                },
                "hitCount": SAMPLES / FUNCTIONS,
                "children": if id == 1 { (2..=FUNCTIONS).collect::<Vec<_>>() } else { Vec::new() },
                "positionTicks": [], "selfTimeMicros": 0, "totalTimeMicros": 0, "sampleCount": 0
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "captureId": "cpu", "samplingIntervalMicros": 100,
        "startTimeMicros": 0.0, "endTimeMicros": (SAMPLES * 100) as f64,
        "nodes": nodes, "samples": (0..SAMPLES).map(|i| i % FUNCTIONS + 1).collect::<Vec<_>>(),
        "timeDeltasMicros": vec![100; SAMPLES],
        "scriptProvenance": { "7": provenance }
    }))
    .unwrap()
}

fn heap_fixture() -> Vec<u8> {
    let mut nodes = vec![0, 0, 1, 0, HEAP_OBJECTS];
    let mut edges = Vec::new();
    let mut locations = Vec::new();
    for index in 0..HEAP_OBJECTS {
        nodes.extend([1, 1, index * 2 + 3, 16 + index % 64, 0]);
        edges.extend([0, 2, (index + 1) * 5]);
        locations.extend([(index + 1) * 5, 7, 0, 0]);
    }
    serde_json::to_vec(&serde_json::json!({
        "snapshot": { "meta": {
            "node_fields": ["type", "name", "id", "self_size", "edge_count"],
            "node_types": [["hidden", "object"], "string", "number", "number", "number"],
            "edge_fields": ["type", "name_or_index", "to_node"],
            "edge_types": [["property"], "string_or_number", "node"],
            "location_fields": ["object_index", "script_id", "line", "column"]
        }, "node_count": HEAP_OBJECTS + 1, "edge_count": HEAP_OBJECTS },
        "nodes": nodes, "edges": edges, "locations": locations,
        "strings": ["root", "a", "child"]
    }))
    .unwrap()
}

fn rss_kib() -> Option<u64> {
    // A point-in-time process-wide Linux RSS, not allocated bytes or a phase peak.
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

enum Fixture {
    Coverage(CoverageSnapshot),
    Cpu(CpuProfileSnapshot),
    Heap(Vec<u8>),
}

async fn publish_fixture(
    service: &DebuggerService,
    server: &FixtureServer,
    name: &str,
    kind: CaptureKind,
    fixture: Fixture,
) {
    let reservation = service
        .reserve_capture("test", "runtime", "target-a", 1, name.into(), kind)
        .await
        .unwrap();
    match fixture {
        Fixture::Coverage(snapshot) => {
            service
                .store_capture(&reservation, CapturePayload::Coverage(snapshot))
                .await
                .unwrap();
        }
        Fixture::Cpu(snapshot) => {
            service
                .store_capture(&reservation, CapturePayload::CpuProfile(snapshot))
                .await
                .unwrap();
        }
        Fixture::Heap(bytes) => {
            let (staging, path) = service.heap_capture_paths(&reservation);
            fs::create_dir_all(staging.parent().unwrap()).unwrap();
            fs::write(&staging, &bytes).unwrap();
            service.capture_storage.sync_file(&staging).unwrap();
            fs::rename(staging, &path).unwrap();
            service.capture_storage.sync_parent(&path).unwrap();
            let provenance = server.provenance(name);
            let result: HeapCaptureResult = serde_json::from_value(serde_json::json!({
                "captureId": name, "bytesWritten": bytes.len(),
                "timing": { "takingDurationMicros": 0, "retrievingDurationMicros": 0 }
            }))
            .unwrap();
            let mut result = result;
            result.mapping = Some(HeapMappingSnapshot {
                connection_generation: 1,
                hydration_duration_micros: 0,
                scripts: vec![HeapScriptSnapshot {
                    script_id: "7".into(),
                    url: provenance.url,
                    hash: provenance.source_sha256.unwrap(),
                    provenance: Default::default(),
                    source_map_url: provenance.source_map_url,
                    generated_source: None,
                    source_map: None,
                    mapping_status: HeapMappingStatus::NotAttempted,
                    diagnostic: None,
                }],
            });
            service
                .store_heap_capture(
                    &reservation,
                    CapturePayload::HeapSnapshot {
                        path: path.to_string_lossy().into_owned(),
                    },
                    result,
                )
                .await
                .unwrap();
        }
    }
}

async fn view(service: &DebuggerService, name: &str, kind: CaptureKind, available: bool) {
    let call = CallCtx::default();
    match kind {
        CaptureKind::Coverage => {
            let snapshot = service
                .get_stored_coverage(
                    &call,
                    "test".into(),
                    name.into(),
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(snapshot.sources[0].functions.len(), FUNCTIONS);
            assert_eq!(snapshot.sources[0].functions[0].ranges[0].count, 1);
            assert_eq!(
                snapshot.sources[0].functions[0].authored_location.is_some(),
                available
            );
            assert_eq!(snapshot.projection_diagnostics.is_empty(), available);
            if !available {
                assert!(
                    snapshot
                        .projection_diagnostics
                        .iter()
                        .any(|diagnostic| { diagnostic.contains("raw measurements retained") })
                );
            }
        }
        CaptureKind::CpuProfile => {
            let snapshot = service
                .get_stored_cpu_profile(&call, "test".into(), name.into(), None, None, None)
                .await
                .unwrap();
            assert_eq!(snapshot.samples.len(), SAMPLES);
            assert_eq!(snapshot.functions.len(), FUNCTIONS);
            assert_eq!(snapshot.nodes[0].authored_location.is_some(), available);
            assert_eq!(snapshot.projection_diagnostics.is_empty(), available);
            if !available {
                assert!(
                    snapshot
                        .projection_diagnostics
                        .iter()
                        .any(|diagnostic| { diagnostic.contains("raw measurements retained") })
                );
            }
        }
        CaptureKind::HeapSnapshot => {
            let snapshot = service
                .get_stored_heap_classes(&call, "test".into(), name.into(), None, None, None)
                .await
                .unwrap();
            assert!(!snapshot.classes.is_empty());
            assert_eq!(
                snapshot.analysis.mapping_status,
                if available {
                    HeapMappingStatus::Mapped
                } else {
                    HeapMappingStatus::MapLoadingFailed
                }
            );
            assert_eq!(
                snapshot.classes[0].name,
                if available { "Original" } else { "a" }
            );
            if !available {
                assert!(
                    snapshot.analysis.script_mappings[0]
                        .diagnostic
                        .as_deref()
                        .unwrap()
                        .contains("raw measurements retained")
                );
            }
        }
    }
}

async fn run_workflow(measure: bool) {
    let server = FixtureServer::start().await;
    let (root, writer) = capture_catalog_service();
    fs::create_dir_all(&root).unwrap();
    let path = root.join("service.json");
    let cases = [
        ("coverage", CaptureKind::Coverage),
        ("cpu", CaptureKind::CpuProfile),
        ("heap", CaptureKind::HeapSnapshot),
    ];
    let mut total_written = 0;
    let mut total_serialized = 0;
    for (name, kind) in cases {
        server.clear_map(name);
        let fixture = match kind {
            CaptureKind::Coverage => Fixture::Coverage(coverage_fixture(server.provenance(name))),
            CaptureKind::CpuProfile => Fixture::Cpu(cpu_fixture(server.provenance(name))),
            CaptureKind::HeapSnapshot => Fixture::Heap(heap_fixture()),
        };
        let before = server.traffic();
        let started = Instant::now();
        publish_fixture(&writer, &server, name, kind, fixture).await;
        let elapsed = started.elapsed();
        assert_eq!(
            server.traffic(),
            before,
            "publication must not fetch sources/maps"
        );
        let state = writer.state.lock().await;
        let capture = &state.captures[&("test".into(), name.into())];
        let payload_bytes = fs::metadata(capture.payload_path()).unwrap().len();
        assert_eq!(payload_bytes, capture.payload.byte_len);
        let catalog_bytes = fs::metadata(&path).unwrap().len();
        let payload_serialized = if kind == CaptureKind::HeapSnapshot {
            0
        } else {
            payload_bytes
        };
        total_written += payload_bytes + catalog_bytes;
        total_serialized += payload_serialized + catalog_bytes;
        if measure {
            println!(
                "publish {name}: elapsed_us={} payload_serialized_bytes={payload_serialized} payload_written_bytes={payload_bytes} catalog_serialized_written_bytes={catalog_bytes} rss_kib={:?} traffic={:?}",
                elapsed.as_micros(),
                rss_kib(),
                server.traffic().since(before)
            );
        }
    }
    if measure {
        println!("cumulative: serialized_bytes={total_serialized} written_bytes={total_written}");
    }
    drop(writer);
    let before = server.traffic();
    let started = Instant::now();
    let (shutdown, _) = watch::channel(false);
    let restored = DebuggerService::load(shutdown, path.clone()).unwrap();
    let restart = started.elapsed();
    assert_eq!(server.traffic(), before);
    assert!(restored.state.lock().await.target_debuggers.is_empty());
    if measure {
        println!(
            "restart: elapsed_us={} rss_kib={:?} traffic={:?}",
            restart.as_micros(),
            rss_kib(),
            server.traffic().since(before)
        );
    }
    let catalog = fs::read(&path).unwrap();
    for (name, kind) in cases {
        let reference = restored.state.lock().await.captures[&("test".into(), name.into())]
            .payload
            .clone();
        let original = fs::read(&reference.path).unwrap();
        assert!(!String::from_utf8_lossy(&original).contains(&server.source));
        assert!(!String::from_utf8_lossy(&catalog).contains(&server.source));
        let before = server.traffic();
        let started = Instant::now();
        let raw = load_capture_payload(&reference, kind).unwrap();
        let raw_read = started.elapsed();
        match raw {
            CapturePayload::Coverage(snapshot) => {
                assert!(snapshot.sources[0].functions[0].authored_location.is_none());
                assert_eq!(serde_json::to_vec(&snapshot).unwrap(), original);
            }
            CapturePayload::CpuProfile(snapshot) => {
                assert!(snapshot.functions.is_empty());
                assert_eq!(serde_json::to_vec(&snapshot).unwrap(), original);
            }
            CapturePayload::HeapSnapshot { path } => assert_eq!(fs::read(path).unwrap(), original),
        }
        assert_eq!(server.traffic(), before);
        if measure {
            println!(
                "raw {name}: elapsed_us={} rss_kib={:?} traffic={:?}",
                raw_read.as_micros(),
                rss_kib(),
                server.traffic().since(before)
            );
        }
        for phase in ["first", "repeated"] {
            let before = server.traffic();
            let started = Instant::now();
            view(&restored, name, kind, true).await;
            let traffic = server.traffic().since(before);
            if phase == "first" {
                assert_eq!(traffic.source_requests, 1);
                assert_eq!(traffic.source_bytes, server.source.len() as u64);
                assert_eq!(traffic.map_requests, 1);
                assert!(traffic.map_bytes > 0);
            } else {
                assert_eq!(
                    traffic.map_requests, 0,
                    "repeated views reuse the verified map cache"
                );
                assert_eq!(
                    traffic.source_requests,
                    u64::from(kind == CaptureKind::Coverage)
                );
            }
            if measure {
                println!(
                    "view {name} {phase}: elapsed_us={} rss_kib={:?} traffic={:?}",
                    started.elapsed().as_micros(),
                    rss_kib(),
                    traffic
                );
            }
        }
        for mode in [1, 2] {
            server.clear_map(name);
            server.mode.store(mode, Ordering::SeqCst);
            let before = server.traffic();
            view(&restored, name, kind, false).await;
            let traffic = server.traffic().since(before);
            assert_eq!(traffic.source_requests, 1);
            assert_eq!(
                traffic.map_requests, 0,
                "unverified generated source must not fetch a map"
            );
            assert_eq!(fs::read(&reference.path).unwrap(), original);
        }
        server.mode.store(0, Ordering::SeqCst);
        assert_eq!(fs::read(&reference.path).unwrap(), original);
        assert_eq!(
            fs::read(&path).unwrap(),
            catalog,
            "views must not rewrite durable capture state"
        );
    }
    drop(restored);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn capture_measurement_roundtrip_preserves_raw_and_unavailable_projection() {
    run_workflow(false).await;
}

#[tokio::test]
#[ignore = "observational benchmark; run explicitly with --ignored --nocapture --test-threads=1"]
async fn capture_measurement_baseline() {
    println!(
        "fixture: functions={FUNCTIONS} cpu_samples={SAMPLES} heap_objects={HEAP_OBJECTS}; times are wall-clock, RSS is process-wide current Linux RSS"
    );
    run_workflow(true).await;
}

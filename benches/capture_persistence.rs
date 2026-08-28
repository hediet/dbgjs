use std::hint::black_box;
use std::time::Instant;

use serde::Serialize;

const CAPTURES: usize = 64;
const PAYLOAD_BYTES: usize = 64 * 1024;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EmbeddedCapture {
    name: String,
    payload: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalCapture {
    name: String,
    path: String,
    sha256: String,
    byte_len: u64,
}

fn main() {
    let payload = "x".repeat(PAYLOAD_BYTES);
    let mut embedded = Vec::new();
    let mut external = Vec::new();
    let mut embedded_serialized_bytes = 0_u64;
    let mut external_catalog_serialized_bytes = 0_u64;
    let mut external_payload_written_bytes = 0_u64;

    let embedded_started = Instant::now();
    for index in 0..CAPTURES {
        embedded.push(EmbeddedCapture {
            name: format!("capture-{index}"),
            payload: payload.clone(),
        });
        let snapshot = black_box(embedded.clone());
        let bytes = serde_json::to_vec(black_box(&snapshot)).unwrap();
        embedded_serialized_bytes += bytes.len() as u64;
        black_box(bytes);
    }
    let embedded_elapsed = embedded_started.elapsed();

    let external_started = Instant::now();
    for index in 0..CAPTURES {
        external_payload_written_bytes += payload.len() as u64;
        external.push(ExternalCapture {
            name: format!("capture-{index}"),
            path: format!("captures/storage-{index}.coverage.json"),
            sha256: "0".repeat(64),
            byte_len: payload.len() as u64,
        });
        let snapshot = black_box(external.clone());
        let bytes = serde_json::to_vec(black_box(&snapshot)).unwrap();
        external_catalog_serialized_bytes += bytes.len() as u64;
        black_box(bytes);
    }
    let external_elapsed = external_started.elapsed();
    let external_total = external_payload_written_bytes + external_catalog_serialized_bytes;

    assert_eq!(
        external_payload_written_bytes,
        (CAPTURES * PAYLOAD_BYTES) as u64
    );
    assert!(
        external_total * 10 < embedded_serialized_bytes,
        "externalized persistence should avoid repeatedly serializing large payloads"
    );

    println!(
        "capture-persistence: {CAPTURES} captures x {PAYLOAD_BYTES} bytes\n\
         embedded cumulative serialized: {embedded_serialized_bytes} bytes in {embedded_elapsed:?}\n\
         external cumulative: {external_payload_written_bytes} payload bytes + \
         {external_catalog_serialized_bytes} bounded catalog bytes = {external_total} bytes \
         in {external_elapsed:?}\n\
         {:.1}x fewer bytes processed",
        embedded_serialized_bytes as f64 / external_total as f64
    );
}

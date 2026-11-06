//! Opt-in phase timing for diagnosing command latency: set `DBGJS_TIMING=1` and each recorded
//! phase prints `dbgjs-timing: <phase> <ms>` to stderr, relative to process start.

use std::sync::OnceLock;
use std::time::Instant;

fn start() -> Option<Instant> {
    static START: OnceLock<Option<Instant>> = OnceLock::new();
    *START.get_or_init(|| std::env::var_os("DBGJS_TIMING").map(|_| Instant::now()))
}

/// Starts the clock; call first in `main` so later marks are relative to process start.
pub fn init() {
    start();
}

pub fn mark(phase: &str) {
    if let Some(start) = start() {
        eprintln!("dbgjs-timing: {phase} {:.1}ms", start.elapsed().as_secs_f64() * 1000.0);
    }
}

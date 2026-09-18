//! Transport seam: the byte/message pump and an in-memory pair for tests and the loopback
//! wiring used throughout the higher layers.

pub mod memory;
pub mod message;
pub mod multiplexed;

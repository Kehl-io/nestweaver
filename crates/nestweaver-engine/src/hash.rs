//! Hashing utilities for the engine crate.
//!
//! The engine uses BLAKE3 for file-level content hashing (change detection,
//! filemeta sidecar). The parser crate uses SHA-256 separately for symbol-level
//! content hashing — these are independent and not compared across levels.

/// Shared BLAKE3 hashing utilities for content change detection.
///
/// BLAKE3 is used throughout the engine for content hashing (not security).
/// It is 2-4x faster than SHA-256 with SIMD and produces 256-bit hashes.

pub fn blake3_hex(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}

pub fn blake3_hex_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn blake3_hex_short(s: &str) -> String {
    blake3_hex(s)[..12].to_string()
}

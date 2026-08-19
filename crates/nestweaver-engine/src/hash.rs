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

/// Stream a reader through BLAKE3 with bounded memory, returning the byte
/// count and lowercase digest. Publication artifacts can be multi-gigabyte;
/// callers must never size a temporary allocation to the artifact itself.
pub fn blake3_stream(mut reader: impl std::io::Read) -> std::io::Result<(u64, String)> {
    let mut hasher = blake3::Hasher::new();
    let byte_size = update_blake3_stream(&mut hasher, &mut reader)?;
    Ok((byte_size, hasher.finalize().to_hex().to_string()))
}

/// Stream one reader into an existing BLAKE3 state. This preserves the legacy
/// snapshot checksum that hashes several files as one concatenated byte
/// sequence without loading any of them wholesale.
pub fn update_blake3_stream(
    hasher: &mut blake3::Hasher,
    mut reader: impl std::io::Read,
) -> std::io::Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut byte_size = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        byte_size = byte_size
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("artifact byte count overflow"))?;
    }
    Ok(byte_size)
}

pub fn blake3_file(path: impl AsRef<std::path::Path>) -> std::io::Result<(u64, String)> {
    blake3_stream(std::fs::File::open(path)?)
}

pub fn blake3_hex_short(s: &str) -> String {
    blake3_hex(s)[..12].to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn streaming_digest_matches_in_memory_digest_across_chunks() {
        let bytes = vec![0x5a; 64 * 1024 + 17];
        let (size, digest) = super::blake3_stream(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(digest, super::blake3_hex_bytes(&bytes));
    }
}

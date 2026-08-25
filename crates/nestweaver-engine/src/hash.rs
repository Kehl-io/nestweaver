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
    /// A KNOWN-ANSWER test against the official BLAKE3 vectors.
    ///
    /// Every other test here compares blake3 to itself, so all of them would
    /// pass unchanged if the digest function changed entirely — a dependency
    /// bump, or an accidental swap to a different hash. These digests are
    /// stored: `content_blake3` gates publication artifact identity and
    /// incremental-index reuse, so a silent change would invalidate every
    /// stored hash while the suite stayed green.
    ///
    /// Vectors from the BLAKE3 reference test set. They are constants of the
    /// specification, not of our code — if this fails, the hash changed, and
    /// nothing downstream that recorded a digest is still valid.
    #[test]
    fn digests_match_the_published_blake3_vectors() {
        assert_eq!(
            super::blake3_hex_bytes(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "empty-input digest changed — stored content_blake3 values are invalid"
        );
        assert_eq!(
            super::blake3_hex_bytes(b"abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
        // The streaming path must agree with the same vector, since that is the
        // path `blake3_file` takes for real content.
        let (size, streamed) = super::blake3_stream(std::io::Cursor::new(b"abc")).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            streamed,
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn streaming_digest_matches_in_memory_digest_across_chunks() {
        let bytes = vec![0x5a; 64 * 1024 + 17];
        let (size, digest) = super::blake3_stream(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(digest, super::blake3_hex_bytes(&bytes));
    }
}

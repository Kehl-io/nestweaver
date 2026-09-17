//! Bounded input policy for source-code and markdown-note indexing.
//!
//! Source files and notes have independent ceilings (`IndexLimits` vs
//! [`NoteLimits`]) that share the same reject-don't-clamp contract.

/// Default maximum source file size: 2 MiB.
pub const DEFAULT_MAX_SOURCE_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Smallest accepted configured source limit: 1 KiB.
pub const MIN_MAX_SOURCE_FILE_BYTES: u64 = 1024;

/// Non-configurable safety ceiling: 64 MiB.
pub const HARD_MAX_SOURCE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Validated source-indexing limits shared by every reader/indexing path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLimits {
    max_source_file_bytes: u64,
}

impl IndexLimits {
    pub fn new(max_source_file_bytes: u64) -> Result<Self, anyhow::Error> {
        if max_source_file_bytes < MIN_MAX_SOURCE_FILE_BYTES {
            anyhow::bail!(
                "[indexing].max_source_file_bytes must be at least {MIN_MAX_SOURCE_FILE_BYTES} bytes (got {max_source_file_bytes})"
            );
        }
        if max_source_file_bytes > HARD_MAX_SOURCE_FILE_BYTES {
            anyhow::bail!(
                "[indexing].max_source_file_bytes must not exceed the hard ceiling of {HARD_MAX_SOURCE_FILE_BYTES} bytes (got {max_source_file_bytes})"
            );
        }
        Ok(Self {
            max_source_file_bytes,
        })
    }

    pub const fn max_source_file_bytes(self) -> u64 {
        self.max_source_file_bytes
    }
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_source_file_bytes: DEFAULT_MAX_SOURCE_FILE_BYTES,
        }
    }
}

/// Default maximum note size: 1 MiB.
pub const DEFAULT_MAX_NOTE_BYTES: u64 = 1024 * 1024;

/// Smallest accepted configured note limit: 1 KiB.
pub const MIN_MAX_NOTE_BYTES: u64 = 1024;

/// Non-configurable safety ceiling: 64 MiB.
pub const HARD_MAX_NOTE_BYTES: u64 = 64 * 1024 * 1024;

/// Validated markdown-note size limit. Mirrors [`IndexLimits`] so notes and
/// source files share the same reject-don't-clamp contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteLimits {
    max_note_bytes: u64,
}

impl NoteLimits {
    pub fn new(max_note_bytes: u64) -> Result<Self, anyhow::Error> {
        if max_note_bytes < MIN_MAX_NOTE_BYTES {
            anyhow::bail!(
                "[indexing].max_note_bytes must be at least {MIN_MAX_NOTE_BYTES} bytes (got {max_note_bytes})"
            );
        }
        if max_note_bytes > HARD_MAX_NOTE_BYTES {
            anyhow::bail!(
                "[indexing].max_note_bytes must not exceed the hard ceiling of {HARD_MAX_NOTE_BYTES} bytes (got {max_note_bytes})"
            );
        }
        Ok(Self { max_note_bytes })
    }

    pub const fn max_note_bytes(self) -> u64 {
        self.max_note_bytes
    }

    /// Filesystem readers reuse [`IndexLimits`] as their byte ceiling.
    pub fn as_index_limits(self) -> IndexLimits {
        IndexLimits::new(self.max_note_bytes).expect("note limits share source-reader bounds")
    }
}

impl Default for NoteLimits {
    fn default() -> Self {
        Self {
            max_note_bytes: DEFAULT_MAX_NOTE_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_limit_bounds_are_rejected_not_clamped() {
        assert!(IndexLimits::new(0).is_err());
        assert!(IndexLimits::new(MIN_MAX_SOURCE_FILE_BYTES - 1).is_err());
        assert!(IndexLimits::new(MIN_MAX_SOURCE_FILE_BYTES).is_ok());
        assert!(IndexLimits::new(HARD_MAX_SOURCE_FILE_BYTES).is_ok());
        assert!(IndexLimits::new(HARD_MAX_SOURCE_FILE_BYTES + 1).is_err());
    }

    #[test]
    fn max_note_bytes_config_is_validated_like_source_bytes() {
        assert!(NoteLimits::new(0).is_err());
        assert!(NoteLimits::new(MIN_MAX_NOTE_BYTES - 1).is_err());
        assert!(NoteLimits::new(MIN_MAX_NOTE_BYTES).is_ok());
        assert!(NoteLimits::new(DEFAULT_MAX_NOTE_BYTES).is_ok());
        assert!(NoteLimits::new(HARD_MAX_NOTE_BYTES).is_ok());
        assert!(NoteLimits::new(HARD_MAX_NOTE_BYTES + 1).is_err());
        let err = NoteLimits::new(512).unwrap_err().to_string();
        assert!(err.contains("max_note_bytes"), "{err}");
    }
}

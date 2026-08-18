//! Bounded input policy for source-code indexing.
//!
//! Markdown notes intentionally keep their independent 1 MiB policy in
//! `index_md`; source inputs use this configurable parser-safety ceiling.

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
}

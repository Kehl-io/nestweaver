use std::collections::HashMap;
use std::path::Path;

use nestweaver_parser::{AstTypeBinding, RawReference, RawSymbol};
use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedParseResult {
    pub symbols: Vec<RawSymbol>,
    pub references: Vec<RawReference>,
    pub type_bindings: Vec<AstTypeBinding>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ParsedCacheFile {
    version: u32,
    entries: HashMap<String, CachedParseResult>,
}

pub struct ParsedCache {
    entries: HashMap<String, CachedParseResult>,
}

impl ParsedCache {
    /// Load a parsed cache from disk. Returns an empty cache on missing/corrupt/version-mismatch files.
    pub fn load(path: &Path) -> Self {
        let entries = match std::fs::read(path) {
            Ok(data) => match rmp_serde::from_slice::<ParsedCacheFile>(&data) {
                Ok(file) if file.version == CACHE_VERSION => file.entries,
                Ok(_) => {
                    tracing::debug!("parsed cache version mismatch, starting fresh");
                    HashMap::new()
                }
                Err(_) => {
                    tracing::debug!("parsed cache corrupt or unreadable, starting fresh");
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };
        Self { entries }
    }

    /// Look up a cached parse result by content hash.
    pub fn get(&self, content_hash: &str) -> Option<&CachedParseResult> {
        self.entries.get(content_hash)
    }

    /// Insert or update a cached parse result keyed by content hash.
    pub fn insert(&mut self, content_hash: String, result: CachedParseResult) {
        self.entries.insert(content_hash, result);
    }

    /// Persist the cache to disk in MessagePack format.
    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        let file = ParsedCacheFile {
            version: CACHE_VERSION,
            entries: self.entries.clone(),
        };
        let data = rmp_serde::to_vec(&file).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
        std::fs::write(path, data).map_err(|e| anyhow::anyhow!("write: {e}"))?;
        Ok(())
    }

    /// Number of entries in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict entries whose content hash is not in `live_hashes`
    /// (i.e. files that were deleted or renamed since the last run).
    pub fn retain_hashes(&mut self, live_hashes: &std::collections::HashSet<String>) {
        self.entries.retain(|hash, _| live_hashes.contains(hash));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_parser::{AstTypeBinding, RawReference, RawSymbol};
    use nestweaver_schema::{SymbolKind, Visibility};

    fn sample_result() -> CachedParseResult {
        CachedParseResult {
            symbols: vec![RawSymbol {
                name: "hello".into(),
                kind: SymbolKind::Function,
                start_line: 1,
                end_line: 3,
                signature: "fn hello()".into(),
                content_hash: "abc123".into(),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
                scope_chain: None,
            }],
            references: vec![RawReference {
                name: "world".into(),
                kind: nestweaver_parser::ReferenceKind::Call,
                start_line: 2,
                context: String::new(),
                receiver: None,
            }],
            type_bindings: vec![AstTypeBinding {
                var_name: "x".into(),
                type_name: "i32".into(),
                line: 1,
                kind: nestweaver_parser::AstBindingKind::Annotation,
            }],
        }
    }

    #[test]
    fn round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");

        let mut cache = ParsedCache::load(&path);
        assert!(cache.is_empty());

        cache.insert("hash1".into(), sample_result());
        cache.insert("hash2".into(), sample_result());
        assert_eq!(cache.len(), 2);

        cache.save(&path).unwrap();

        let loaded = ParsedCache::load(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get("hash1").is_some());
        assert!(loaded.get("hash2").is_some());
        assert!(loaded.get("nonexistent").is_none());

        let result = loaded.get("hash1").unwrap();
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "hello");
        assert_eq!(result.references.len(), 1);
        assert_eq!(result.references[0].name, "world");
        assert_eq!(result.type_bindings.len(), 1);
        assert_eq!(result.type_bindings[0].var_name, "x");
    }

    #[test]
    fn version_mismatch_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");

        // Write a cache with a different version
        let file = ParsedCacheFile {
            version: 999,
            entries: HashMap::new(),
        };
        let data = rmp_serde::to_vec(&file).unwrap();
        std::fs::write(&path, data).unwrap();

        let cache = ParsedCache::load(&path);
        assert!(cache.is_empty());
    }

    #[test]
    fn corrupt_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");

        std::fs::write(&path, b"not valid msgpack").unwrap();

        let cache = ParsedCache::load(&path);
        assert!(cache.is_empty());
    }

    #[test]
    fn missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.bin");

        let cache = ParsedCache::load(&path);
        assert!(cache.is_empty());
    }
}

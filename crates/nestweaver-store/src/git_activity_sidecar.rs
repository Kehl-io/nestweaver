//! On-disk shape of `<db>.gitactivity.json`.
//!
//! Lives in the STORE rather than the engine because both crates need it: the
//! engine mines and writes it, the store reads it into the ranking cache. Two
//! copies of a serialization shape is how the two halves drift apart.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// v1 was an implicit, unversioned, flat `path -> score` map with no repo
/// dimension at all.
pub const GITACTIVITY_VERSION: u32 = 2;

/// Recency scores keyed by repo uid, then repo-relative path.
///
/// The repo dimension is the whole point. v1 was flat, and the read side looked
/// up a REPO-RELATIVE path, so two repos collided on every shared name —
/// `src/main.rs`, `README.md`, `mod.rs`. The write side compounded it by
/// replacing the entire file with just the repo being indexed, so in a
/// multi-repo database indexing ANY repo erased every other repo's scores.
///
/// Same shape, and for the same reason, as `FileMetaSidecar` (nw-022) and
/// `ResolutionCacheFile` (nw-045). Nesting rather than `"<uid>\0<path>"`
/// composite keys, because nesting is what makes per-repo replace and per-repo
/// deletion trivial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitActivitySidecar {
    pub version: u32,
    pub repos: HashMap<String, HashMap<String, f64>>,
}

impl Default for GitActivitySidecar {
    fn default() -> Self {
        Self {
            version: GITACTIVITY_VERSION,
            repos: HashMap::new(),
        }
    }
}

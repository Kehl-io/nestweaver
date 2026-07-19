//! P0.2: persistence for the `graph_generation` counter.
//!
//! `graph_generation` is an in-memory [`std::sync::atomic::AtomicU64`] that
//! previously reset to 0 on every process open and was bumped only by the
//! long-lived watchers — never by the one-shot `index` command. That made it
//! useless as a cache-invalidation key for short-lived processes (e.g. an MCP
//! server that starts, answers one query, and exits).
//!
//! We persist it to a plain-integer sidecar `<db>.generation`, mirroring how
//! the PageRank sidecar is loaded on open. The counter is loaded on store open
//! (so it survives restarts) and bumped + persisted at the end of every
//! graph-mutating operation. The F16 response cache's correctness rests
//! entirely on this persisted value: a reindex bumps + persists the
//! generation, so a freshly-opened process sees the new value and treats every
//! older cache entry as a MISS — no background daemon required.

use std::path::Path;
use std::sync::atomic::Ordering;

use crate::db::GraphStore;
use crate::error::StoreError;

impl GraphStore {
    fn load_fail_closed_index_publication_generation(&self, canonical: Option<u64>) {
        let canonical = canonical.unwrap_or(u64::MAX);
        let reserved = canonical.saturating_add(1);
        *self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(canonical);
        self.graph_generation.store(reserved, Ordering::Release);
    }

    /// Load the persisted `graph_generation` value from the `<db>.generation`
    /// sidecar at `path` into the in-memory counter. When publication is clean,
    /// an absent or unparseable sidecar leaves the current value unchanged. A
    /// dirty publication instead reserves the canonical successor; if the
    /// canonical value is unavailable or exhausted, recovery remains at the
    /// non-advancing fail-closed value and publication cannot complete.
    ///
    /// Called automatically on [`GraphStore::open`] / [`GraphStore::create`] /
    /// [`GraphStore::open_read_only`] via the stored `db_path`.
    pub fn load_graph_generation(&self, path: &Path) {
        let canonical = std::fs::read_to_string(path)
            .ok()
            .and_then(|contents| contents.trim().parse::<u64>().ok());
        if self.is_index_publication_dirty() {
            self.load_fail_closed_index_publication_generation(canonical);
            return;
        }
        self.complete_index_publication_generation();
        if let Some(value) = canonical {
            self.graph_generation.store(value, Ordering::Release);
        }
    }

    /// Persist the current in-memory `graph_generation` value to the
    /// `<db>.generation` sidecar at `path` (a plain decimal integer).
    pub fn save_graph_generation(&self, path: &Path) -> Result<(), StoreError> {
        let value = self.graph_generation.load(Ordering::Acquire);
        std::fs::write(path, value.to_string())
            .map_err(|e| StoreError::Query(format!("write generation sidecar: {e}")))
    }

    /// Bump the `graph_generation` counter and immediately persist it to the
    /// `<db>.generation` sidecar at `path`. This is the canonical call for the
    /// end of any graph-mutating operation (`index`, incremental index, watcher
    /// batch). Persisting on every bump is what lets a later short-lived
    /// process observe the bump without any running daemon.
    ///
    /// The operation is best-effort: advancement exhaustion and write failures
    /// are logged but do not abort the surrounding operation.
    pub fn bump_and_persist_graph_generation(&self, path: &Path) {
        if let Err(e) = self.try_bump_graph_generation() {
            tracing::warn!("failed to advance graph generation: {e}");
            return;
        }
        if let Err(e) = self.save_graph_generation(path) {
            tracing::warn!("failed to persist graph generation: {e}");
        }
    }
}

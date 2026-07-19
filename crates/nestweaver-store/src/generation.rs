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

use std::io::Write;
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
    /// dirty publication instead reserves the canonical N+1 dirty successor;
    /// completion separately publishes N+2. If either successor is unavailable,
    /// recovery remains fail-closed and publication cannot complete.
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
        self.clear_index_publication_generation_on_clean_load();
        if let Some(value) = canonical {
            self.graph_generation.store(value, Ordering::Release);
        }
    }

    /// Persist the current in-memory `graph_generation` value to the
    /// `<db>.generation` sidecar at `path` (a plain decimal integer).
    ///
    /// If this returns an error from the final parent-directory sync, the
    /// complete new generation may already be canonical even though crash
    /// durability of the rename could not be confirmed.
    pub fn save_graph_generation(&self, path: &Path) -> Result<(), StoreError> {
        let value = self.graph_generation.load(Ordering::Acquire);
        self.save_graph_generation_value(path, value)
    }

    /// Persist an explicitly prepared generation without making it visible to
    /// live cache consumers. Index publication uses this for clean N+2 while
    /// the dirty marker and live N+1 reservation remain authoritative.
    /// A final directory-sync error can occur after the canonical file was
    /// replaced; callers must retain their fail-closed publication marker.
    pub fn save_graph_generation_value(&self, path: &Path, value: u64) -> Result<(), StoreError> {
        let contents = value.to_string();
        crate::durable_sidecar::atomic_replace_file(path, |file| {
            file.write_all(contents.as_bytes())
        })
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

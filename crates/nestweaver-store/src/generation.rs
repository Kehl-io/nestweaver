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
    /// Capture the durable generation of a clean graph publication.
    ///
    /// Unlike [`Self::graph_generation`], this is a cross-process snapshot for
    /// file-backed stores: it reads the canonical `<db>.generation` sidecar
    /// instead of the process-local atomic loaded when the store was opened.
    /// Callers can compare snapshots around a multi-read operation to detect a
    /// publication that started *and completed* between dirty-marker probes.
    ///
    /// The snapshot fails closed while an in-process publisher owns the lease,
    /// while the durable marker is present or unreadable, or when the durable
    /// generation itself is missing/unreadable after generation zero. A fresh
    /// database is the only valid file-backed state without a sidecar.
    pub fn clean_published_generation_snapshot(&self) -> Result<u64, StoreError> {
        if !self.index_publication_lease_is_unowned() {
            return Err(StoreError::RankingUnavailable);
        }
        if !matches!(
            self.index_publication_marker_state(),
            crate::index_publication::MarkerState::Absent
        ) {
            return Err(StoreError::RankingUnavailable);
        }

        let generation = match self.db_path() {
            None => self.graph_generation(),
            Some(_) => {
                let generation_path = self.generation_sidecar_path();
                match std::fs::read(&generation_path) {
                    Ok(contents) => parse_published_generation(&contents)?,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound
                            && self.graph_generation() == 0 =>
                    {
                        0
                    }
                    Err(_) => return Err(StoreError::RankingUnavailable),
                }
            }
        };

        // Re-probe both process-local ownership and the cross-process marker
        // after the sidecar read. Atomic sidecar replacement means the read
        // observed either the old complete file or the new complete file; the
        // probes reject an in-progress publication on either side of it.
        if !self.index_publication_lease_is_unowned()
            || !matches!(
                self.index_publication_marker_state(),
                crate::index_publication::MarkerState::Absent
            )
        {
            return Err(StoreError::RankingUnavailable);
        }

        Ok(generation)
    }

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

fn parse_published_generation(contents: &[u8]) -> Result<u64, StoreError> {
    if contents.is_empty() || !contents.iter().all(u8::is_ascii_digit) {
        return Err(StoreError::RankingUnavailable);
    }
    let value = std::str::from_utf8(contents).map_err(|_| StoreError::RankingUnavailable)?;
    value
        .parse::<u64>()
        .map_err(|_| StoreError::RankingUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_snapshot_uses_the_in_memory_generation() {
        let store = GraphStore::in_memory().unwrap();
        store.bump_graph_generation();

        assert_eq!(store.clean_published_generation_snapshot().unwrap(), 1);
    }

    #[test]
    fn clean_snapshot_strictly_parses_the_durable_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let generation_path = store.generation_sidecar_path();

        assert_eq!(
            store.clean_published_generation_snapshot().unwrap(),
            0,
            "generation zero is the only clean state allowed without a sidecar"
        );

        std::fs::write(&generation_path, b"42").unwrap();
        assert_eq!(store.clean_published_generation_snapshot().unwrap(), 42);

        std::fs::write(&generation_path, b"42\n").unwrap();
        assert!(matches!(
            store.clean_published_generation_snapshot(),
            Err(StoreError::RankingUnavailable)
        ));
    }

    #[test]
    fn clean_snapshot_rejects_missing_nonzero_generation_and_a_local_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();

        store.bump_graph_generation();
        assert!(matches!(
            store.clean_published_generation_snapshot(),
            Err(StoreError::RankingUnavailable)
        ));

        store
            .save_graph_generation(&store.generation_sidecar_path())
            .unwrap();
        let _publication = store.acquire_index_publication_lease().unwrap();
        assert!(matches!(
            store.clean_published_generation_snapshot(),
            Err(StoreError::RankingUnavailable)
        ));
    }
}

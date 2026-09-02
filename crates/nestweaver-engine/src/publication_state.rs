//! Typed preservation of user-owned state across a fresh publication rebuild.
//!
//! Source-derived graph rows and accelerators are rebuilt. Interaction history
//! is different: it represents user behaviour and must survive when its stable
//! graph UID still exists. This module captures it before staging, imports only
//! live UIDs after graph materialization, and writes an identity-bound receipt
//! so cutover validation can prove what was retained or deliberately pruned.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PreservedStateSnapshot {
    interactions: Option<crate::interactions::InteractionStore>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreservedStateReceipt {
    pub version: u32,
    pub interaction_nodes_captured: usize,
    pub interaction_nodes_imported: usize,
    pub interaction_nodes_pruned: usize,
    pub captured_interactions_blake3: Option<String>,
    pub imported_interactions_blake3: Option<String>,
}

impl PreservedStateSnapshot {
    pub fn capture(db_path: &Path) -> anyhow::Result<Self> {
        let path = crate::interactions::interaction_sidecar_path(db_path);
        let interactions = if path.exists() {
            Some(
                crate::interactions::load_interaction_store_public(db_path).ok_or_else(|| {
                    anyhow::anyhow!(
                        "interaction history at {} is unreadable; refusing to silently drop user state",
                        path.display()
                    )
                })?,
            )
        } else {
            None
        };
        Ok(Self { interactions })
    }

    /// Stable fingerprint of every captured non-derived input. Publication
    /// resume and final source revalidation include this value so interaction
    /// history written against the incumbent cannot be omitted by resuming a
    /// graph phase that imported an older snapshot.
    pub fn fingerprint(&self) -> anyhow::Result<String> {
        let interaction = self
            .interactions
            .as_ref()
            .map(interaction_digest)
            .transpose()?;
        Ok(crate::hash::blake3_hex_bytes(&serde_json::to_vec(&(
            crate::publication::PRESERVED_STATE_SCHEMA_VERSION,
            interaction,
        ))?))
    }

    pub fn import_into(self, db_path: &Path) -> anyhow::Result<PreservedStateReceipt> {
        let store = nestweaver_store::GraphStore::open_read_only_without_migration(db_path)?;
        let live = store.live_graph_node_uids()?;
        drop(store);

        let captured_count = self
            .interactions
            .as_ref()
            .map_or(0, |store| store.node_scores.len());
        let captured_digest = self
            .interactions
            .as_ref()
            .map(interaction_digest)
            .transpose()?;
        let mut interactions = self.interactions;
        if let Some(store) = interactions.as_mut() {
            store.node_scores.retain(|uid, _| live.contains(uid));
            crate::interactions::save_interaction_store(db_path, store)?;
        }
        let imported_count = interactions
            .as_ref()
            .map_or(0, |store| store.node_scores.len());
        let imported_digest = interactions.as_ref().map(interaction_digest).transpose()?;
        Ok(PreservedStateReceipt {
            version: crate::publication::PRESERVED_STATE_SCHEMA_VERSION,
            interaction_nodes_captured: captured_count,
            interaction_nodes_imported: imported_count,
            interaction_nodes_pruned: captured_count.saturating_sub(imported_count),
            captured_interactions_blake3: captured_digest,
            imported_interactions_blake3: imported_digest,
        })
    }
}

impl PreservedStateReceipt {
    pub fn write_bound(&self, db_path: &Path) -> anyhow::Result<PathBuf> {
        let store = nestweaver_store::GraphStore::open_read_only_without_migration(db_path)?;
        let identity = store
            .publication_identity()?
            .ok_or_else(|| anyhow::anyhow!("publication graph has no identity"))?;
        let envelope = nestweaver_store::artifact_envelope::ArtifactEnvelope::new(
            nestweaver_store::artifact_envelope::ArtifactExpectation {
                artifact_kind: crate::publication::PRESERVED_STATE_ARTIFACT_KIND,
                artifact_schema_version: crate::publication::PRESERVED_STATE_SCHEMA_VERSION,
                identity: &identity,
                producer_version: env!("CARGO_PKG_VERSION"),
                source_graph_generation: store.graph_generation(),
                algorithm_fingerprint: crate::publication::PRESERVED_STATE_ALGORITHM_FINGERPRINT,
            },
            self,
        )?;
        drop(store);
        let path = crate::sidecar_path(db_path, crate::publication::PRESERVED_STATE_SUFFIX);
        let bytes = serde_json::to_vec_pretty(&envelope)?;
        nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| {
            use std::io::Write as _;
            file.write_all(&bytes)?;
            file.write_all(b"\n")
        })?;
        Ok(path)
    }
}

fn interaction_digest(store: &crate::interactions::InteractionStore) -> anyhow::Result<String> {
    let mut entries: Vec<_> = store.node_scores.iter().collect();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    Ok(crate::hash::blake3_hex_bytes(&serde_json::to_vec(&(
        store.version,
        store.last_compacted.to_bits(),
        entries,
    ))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{Symbol, SymbolKind, Visibility};

    #[test]
    fn import_preserves_live_interactions_and_prunes_stale_uids() {
        let dir = tempfile::tempdir().unwrap();
        let incumbent = dir.path().join("incumbent.lbug");
        let target = dir.path().join("target.lbug");
        let incumbent_store = nestweaver_store::GraphStore::create(&incumbent).unwrap();
        drop(incumbent_store);
        let mut interactions = crate::interactions::InteractionStore::default();
        interactions.node_scores.insert(
            "sym:live".to_string(),
            crate::interactions::NodeScore::default(),
        );
        interactions.node_scores.insert(
            "sym:stale".to_string(),
            crate::interactions::NodeScore::default(),
        );
        crate::interactions::save_interaction_store(&incumbent, &interactions).unwrap();
        let snapshot = PreservedStateSnapshot::capture(&incumbent).unwrap();

        let target_store = nestweaver_store::GraphStore::create(&target).unwrap();
        target_store
            .insert_symbol(&Symbol {
                uid: "sym:live".to_string(),
                name: "live".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:test".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn live()".to_string(),
                summary: None,
                content_hash: "hash".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();
        drop(target_store);

        let receipt = snapshot.import_into(&target).unwrap();
        assert_eq!(receipt.interaction_nodes_captured, 2);
        assert_eq!(receipt.interaction_nodes_imported, 1);
        assert_eq!(receipt.interaction_nodes_pruned, 1);
        receipt.write_bound(&target).unwrap();
        let imported = crate::interactions::load_interaction_store_public(&target).unwrap();
        assert!(imported.node_scores.contains_key("sym:live"));
        assert!(!imported.node_scores.contains_key("sym:stale"));
    }

    #[test]
    fn fingerprint_changes_when_preserved_interactions_change() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        drop(nestweaver_store::GraphStore::create(&db).unwrap());
        let empty = PreservedStateSnapshot::capture(&db)
            .unwrap()
            .fingerprint()
            .unwrap();

        let mut interactions = crate::interactions::InteractionStore::default();
        interactions.node_scores.insert(
            "sym:one".to_string(),
            crate::interactions::NodeScore::default(),
        );
        crate::interactions::save_interaction_store(&db, &interactions).unwrap();
        let populated = PreservedStateSnapshot::capture(&db)
            .unwrap()
            .fingerprint()
            .unwrap();
        assert_ne!(empty, populated);
    }
}

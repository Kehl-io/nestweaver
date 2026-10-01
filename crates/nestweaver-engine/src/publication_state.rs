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
        } else {
            // A resumed rebuild imports again into a slot that may hold an
            // earlier import; no captured history means none in the slot.
            nestweaver_store::durable_sidecar::remove_file_durable_if_exists(
                &crate::interactions::interaction_sidecar_path(db_path),
            )?;
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

/// Digest of the interaction history in a canonical order.
///
/// Both unordered collections are sorted first: the node map by uid, and each
/// node's `session_ids` (a hash set, which serialises in iteration order).
/// Without that, two loads of one sidecar produce two digests, and the
/// resume and revalidation checks that compare them never agree.
fn interaction_digest(store: &crate::interactions::InteractionStore) -> anyhow::Result<String> {
    let mut entries = store
        .node_scores
        .iter()
        .map(|(uid, score)| {
            let mut sessions: Vec<&String> = score.session_ids.iter().collect();
            sessions.sort();
            let mut fields = serde_json::to_value(score)?;
            if let Some(object) = fields.as_object_mut() {
                object.remove("session_ids");
            }
            Ok((uid, fields, sessions))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
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

    /// The fingerprint gates resume and the final revalidation, so the same
    /// sidecar must fingerprint the same on every load. `session_ids` is a
    /// hash set: serialised in iteration order, two loads of one file gave
    /// two digests, and a rebuild of any brain whose history held a node
    /// seen in several sessions could never validate.
    #[test]
    fn the_fingerprint_of_one_sidecar_is_the_same_on_every_load() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let mut interactions = crate::interactions::InteractionStore::default();
        for node in 0..8 {
            let score = crate::interactions::NodeScore {
                session_ids: (0..40)
                    .map(|session| format!("session-{node}-{session}"))
                    .collect(),
                ..Default::default()
            };
            interactions
                .node_scores
                .insert(format!("sym:{node}"), score);
        }
        crate::interactions::save_interaction_store(&db, &interactions).unwrap();

        let first = PreservedStateSnapshot::capture(&db)
            .unwrap()
            .fingerprint()
            .unwrap();
        for _ in 0..20 {
            let again = PreservedStateSnapshot::capture(&db)
                .unwrap()
                .fingerprint()
                .unwrap();
            assert_eq!(again, first, "one sidecar, two fingerprints");
        }

        // Counterweight: a real change to the history still moves it.
        interactions
            .node_scores
            .get_mut("sym:0")
            .unwrap()
            .session_ids
            .insert("another-session".to_string());
        crate::interactions::save_interaction_store(&db, &interactions).unwrap();
        assert_ne!(
            PreservedStateSnapshot::capture(&db)
                .unwrap()
                .fingerprint()
                .unwrap(),
            first
        );
    }

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

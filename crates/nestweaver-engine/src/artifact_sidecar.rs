//! Identity- and generation-bound JSON sidecars derived from the live graph.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::Path;

pub(crate) fn save_json<T: Serialize>(
    store: &nestweaver_store::GraphStore,
    path: &Path,
    artifact_kind: &str,
    artifact_schema_version: u32,
    algorithm_fingerprint: &str,
    payload: &T,
) -> anyhow::Result<()> {
    let identity = store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph has no publication identity"))?;
    let envelope = nestweaver_store::artifact_envelope::ArtifactEnvelope::new(
        nestweaver_store::artifact_envelope::ArtifactExpectation {
            artifact_kind,
            artifact_schema_version,
            identity: &identity,
            producer_version: env!("CARGO_PKG_VERSION"),
            source_graph_generation: store.graph_generation(),
            algorithm_fingerprint,
        },
        payload,
    )?;
    let bytes = serde_json::to_vec_pretty(&envelope)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(path, |file| file.write_all(&bytes))?;
    Ok(())
}

pub(crate) fn load_json<T: DeserializeOwned>(
    store: &nestweaver_store::GraphStore,
    path: &Path,
    artifact_kind: &str,
    artifact_schema_version: u32,
    algorithm_fingerprint: &str,
) -> anyhow::Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let envelope: nestweaver_store::artifact_envelope::ArtifactEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "incompatible {artifact_kind} sidecar {}: expected a self-describing artifact envelope; run a full reindex ({error})",
                path.display()
            )
        })?;
    let identity = store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph has no publication identity"))?;
    envelope
        .validate_and_decode(nestweaver_store::artifact_envelope::ArtifactExpectation {
            artifact_kind,
            artifact_schema_version,
            identity: &identity,
            producer_version: env!("CARGO_PKG_VERSION"),
            source_graph_generation: store.graph_generation(),
            algorithm_fingerprint,
        })
        .map(Some)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_json_sidecar_round_trips_and_rejects_stale_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.lbug");
        let path = dir.path().join("graph.lbug.example.json");
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        let payload = std::collections::BTreeMap::from([("key", "value")]);
        save_json(&store, &path, "example", 1, "example-v1", &payload).unwrap();
        let loaded: std::collections::BTreeMap<String, String> =
            load_json(&store, &path, "example", 1, "example-v1")
                .unwrap()
                .unwrap();
        assert_eq!(loaded.get("key").map(String::as_str), Some("value"));

        store.bump_graph_generation();
        let error = load_json::<std::collections::BTreeMap<String, String>>(
            &store,
            &path,
            "example",
            1,
            "example-v1",
        )
        .unwrap_err();
        assert!(error.to_string().contains("stale artifact generation"));
    }
}

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    pub instance_id: String,
    pub engine_version: String,
    pub min_compatible_engine: String,
    pub schema_hash_core: String,
    pub schema_hash_extensions: String,
    pub schema_hash_effective: String,
    pub embedding_model_id: String,
    pub embedding_dimension: u32,
    pub built_at: String,
    pub repos: Vec<RepoStamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoStamp {
    pub url: String,
    pub indexed_sha: String,
    pub commits_behind_head: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub repos: Vec<ManifestRepo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRepo {
    pub url: String,
    pub indexed_sha: String,
    pub files_skipped: Vec<SkippedFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFileEntry {
    pub path: String,
    pub reason: String,
}

/// The filenames stored inside a snapshot directory.
const GRAPH_FILE: &str = "graph.lbug";
const MANIFEST_FILE: &str = "manifest.json";
const STAMP_FILE: &str = "stamp.json";
const CHECKSUM_FILE: &str = "checksum.sha256";
/// Sidecar filenames (relative to the db_path prefix, not snapshot_dir).
const SIDECAR_PAGERANK: &str = "pagerank.json";
const SIDECAR_MANIFESTS: &str = "manifests.json";
const SIDECAR_TANTIVY_DIR: &str = "tantivy";

/// Core files that MUST be present and checksummed in every snapshot.
const CORE_FILES: &[&str] = &[GRAPH_FILE, MANIFEST_FILE, STAMP_FILE];

/// Sidecar files that are checksummed when present.
const SIDECAR_FILES: &[&str] = &[SIDECAR_PAGERANK, SIDECAR_MANIFESTS];

/// Compute per-file SHA-256 checksums for all core files and any present
/// sidecars.  Returns a sha256sum-style string: one `<hash>  <filename>\n`
/// line per file, sorted by filename for determinism.
fn compute_checksums(snapshot_dir: &Path) -> Result<String, anyhow::Error> {
    let mut lines: Vec<String> = Vec::new();
    for &name in CORE_FILES {
        let bytes = std::fs::read(snapshot_dir.join(name))
            .map_err(|e| anyhow::anyhow!("failed to read {name} for checksum: {e}"))?;
        let hash = hex::encode(Sha256::digest(&bytes));
        lines.push(format!("{hash}  {name}"));
    }
    for &name in SIDECAR_FILES {
        let path = snapshot_dir.join(name);
        if path.exists() {
            let bytes = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("failed to read sidecar {name} for checksum: {e}"))?;
            let hash = hex::encode(Sha256::digest(&bytes));
            lines.push(format!("{hash}  {name}"));
        }
    }
    // Hash tantivy directory contents if present (hash each file, sorted)
    let tantivy_dir = snapshot_dir.join(SIDECAR_TANTIVY_DIR);
    if tantivy_dir.is_dir() {
        let mut tantivy_files = collect_files_recursive(&tantivy_dir, SIDECAR_TANTIVY_DIR)?;
        tantivy_files.sort();
        for (rel_path, abs_path) in tantivy_files {
            let bytes = std::fs::read(&abs_path)
                .map_err(|e| anyhow::anyhow!("failed to read {rel_path} for checksum: {e}"))?;
            let hash = hex::encode(Sha256::digest(&bytes));
            lines.push(format!("{hash}  {rel_path}"));
        }
    }
    lines.sort();
    Ok(lines.join("\n") + "\n")
}

/// Recursively collect (relative_path, absolute_path) for all files under `dir`.
fn collect_files_recursive(
    dir: &Path,
    prefix: &str,
) -> Result<Vec<(String, PathBuf)>, anyhow::Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        let rel = format!("{prefix}/{}", name.to_string_lossy());
        if ty.is_dir() {
            out.extend(collect_files_recursive(&entry.path(), &rel)?);
        } else if ty.is_file() {
            out.push((rel, entry.path()));
        }
    }
    Ok(out)
}

/// Verify checksums from a checksum file.  Supports both the new per-file
/// format (multiple `<hash>  <filename>` lines) and the legacy single-hash
/// format for backwards compatibility.
fn verify_checksums(snapshot_dir: &Path) -> Result<(), anyhow::Error> {
    let stored = std::fs::read_to_string(snapshot_dir.join(CHECKSUM_FILE))
        .map_err(|e| anyhow::anyhow!("failed to read checksum.sha256: {e}"))?;
    let stored = stored.trim();

    if stored.contains("  ") {
        // New per-file format
        for line in stored.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (expected_hash, filename) = line.split_once("  ").ok_or_else(|| {
                anyhow::anyhow!("malformed checksum line: {line}")
            })?;
            let file_path = snapshot_dir.join(filename);
            if !file_path.exists() {
                anyhow::bail!("checksum references missing file: {filename}");
            }
            let bytes = std::fs::read(&file_path)
                .map_err(|e| anyhow::anyhow!("failed to read {filename}: {e}"))?;
            let actual = hex::encode(Sha256::digest(&bytes));
            if actual != expected_hash {
                anyhow::bail!(
                    "integrity check failed for {filename}: \
                     expected {expected_hash}, got {actual}"
                );
            }
        }
    } else {
        // Legacy single-hash format: concatenated hash of core files
        let mut hasher = Sha256::new();
        for name in CORE_FILES {
            let bytes = std::fs::read(snapshot_dir.join(name))
                .map_err(|e| anyhow::anyhow!("failed to read {name} for checksum: {e}"))?;
            hasher.update(&bytes);
        }
        let computed = hex::encode(hasher.finalize());
        if computed != stored {
            anyhow::bail!(
                "snapshot integrity check failed: stored checksum does not match computed checksum"
            );
        }
    }
    Ok(())
}

use nestweaver_storage::copy_dir_all;

/// Build a snapshot in `output_dir`.
///
/// 1. Copy core files (graph, manifest, stamp).
/// 2. Copy sidecars (pagerank, manifests, tantivy) if present.
/// 3. Compute per-file SHA-256 checksums over everything that was copied.
pub fn build_snapshot(
    output_dir: &Path,
    stamp: &Stamp,
    manifest: &Manifest,
    db_path: &Path,
) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(output_dir).map_err(|e| {
        anyhow::anyhow!("failed to create output_dir {}: {e}", output_dir.display())
    })?;

    // ── Core files ──────────────────────────────────────────────────────────
    std::fs::copy(db_path, output_dir.join(GRAPH_FILE))
        .map_err(|e| anyhow::anyhow!("failed to copy graph file: {e}"))?;

    let manifest_json = serde_json::to_string_pretty(manifest)?;
    std::fs::write(output_dir.join(MANIFEST_FILE), &manifest_json)?;

    let stamp_json = serde_json::to_string_pretty(stamp)?;
    std::fs::write(output_dir.join(STAMP_FILE), &stamp_json)?;

    // ── Sidecars (best-effort) ──────────────────────────────────────────────
    let pagerank_src = crate::sidecar_path(db_path, &format!(".{SIDECAR_PAGERANK}"));
    if pagerank_src.exists() {
        if let Err(e) = std::fs::copy(&pagerank_src, output_dir.join(SIDECAR_PAGERANK)) {
            tracing::warn!(
                src = %pagerank_src.display(),
                "build_snapshot: failed to copy pagerank sidecar: {e}"
            );
        }
    } else {
        tracing::debug!(
            src = %pagerank_src.display(),
            "build_snapshot: pagerank sidecar not found, skipping"
        );
    }

    let manifests_src = crate::sidecar_path(db_path, &format!(".{SIDECAR_MANIFESTS}"));
    if manifests_src.exists() {
        if let Err(e) = std::fs::copy(&manifests_src, output_dir.join(SIDECAR_MANIFESTS)) {
            tracing::warn!(
                src = %manifests_src.display(),
                "build_snapshot: failed to copy manifests sidecar: {e}"
            );
        }
    } else {
        tracing::debug!(
            src = %manifests_src.display(),
            "build_snapshot: manifests sidecar not found, skipping"
        );
    }

    let tantivy_src = crate::sidecar_path(db_path, ".tantivy");
    if tantivy_src.exists() && tantivy_src.is_dir() {
        if let Err(e) = copy_dir_all(&tantivy_src, &output_dir.join(SIDECAR_TANTIVY_DIR)) {
            tracing::warn!(
                src = %tantivy_src.display(),
                "build_snapshot: failed to copy tantivy directory: {e}"
            );
        }
    } else {
        tracing::debug!(
            src = %tantivy_src.display(),
            "build_snapshot: tantivy index directory not found, skipping"
        );
    }

    // ── Checksums (after all files are in place) ────────────────────────────
    let checksums = compute_checksums(output_dir)?;
    std::fs::write(output_dir.join(CHECKSUM_FILE), &checksums)?;

    Ok(())
}

/// Verify a snapshot directory's integrity.
///
/// Supports both the new per-file checksum format and the legacy single-hash
/// format for backwards compatibility.
pub fn verify_snapshot(snapshot_dir: &Path) -> Result<Stamp, anyhow::Error> {
    verify_checksums(snapshot_dir)?;

    let stamp_json = std::fs::read_to_string(snapshot_dir.join(STAMP_FILE))
        .map_err(|e| anyhow::anyhow!("failed to read stamp.json: {e}"))?;
    let stamp: Stamp = serde_json::from_str(&stamp_json)?;

    Ok(stamp)
}

/// Compare two semver strings (e.g. "1.2.3"). Returns true if `a >= b`.
/// Only handles numeric major.minor.patch without pre-release suffixes.
fn semver_ge(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let mut parts = s.trim().splitn(3, '.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(a) >= parse(b)
}

/// Load a snapshot, validating compatibility against the running engine.
///
/// Steps:
/// 1. `verify_snapshot()` — integrity check.
/// 2. Check `stamp.min_compatible_engine <= engine_version` (stamp requires at least that version).
/// 3. Check `stamp.schema_hash_effective == expected_schema_hash`.
/// 4. Check `stamp.embedding_model_id == expected_embedding_model`.
/// 5. Return `(stamp, path to graph.lbug)`.
pub fn load_snapshot(
    snapshot_dir: &Path,
    engine_version: &str,
    expected_schema_hash: &str,
    expected_embedding_model: &str,
) -> Result<(Stamp, PathBuf), anyhow::Error> {
    let stamp = verify_snapshot(snapshot_dir)?;

    // The snapshot requires at least min_compatible_engine to load it.
    // If the running engine_version < min_compatible_engine, reject.
    if !semver_ge(engine_version, &stamp.min_compatible_engine) {
        anyhow::bail!(
            "snapshot requires engine >= {} but current engine is {}; \
             rebuild the snapshot with a newer engine or downgrade the min_compatible_engine requirement",
            stamp.min_compatible_engine,
            engine_version
        );
    }

    if stamp.schema_hash_effective != expected_schema_hash {
        anyhow::bail!(
            "snapshot schema hash mismatch: snapshot has '{}' but engine expects '{}'; \
             rebuild the snapshot to pick up the new schema",
            stamp.schema_hash_effective,
            expected_schema_hash
        );
    }

    if stamp.embedding_model_id != expected_embedding_model {
        anyhow::bail!(
            "snapshot embedding model mismatch: snapshot used '{}' but engine expects '{}'; \
             rebuild the snapshot with the correct embedding model",
            stamp.embedding_model_id,
            expected_embedding_model
        );
    }

    Ok((stamp, snapshot_dir.join(GRAPH_FILE)))
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stamp(
        engine_version: &str,
        min_compatible: &str,
        schema_hash: &str,
        model: &str,
    ) -> Stamp {
        Stamp {
            instance_id: "test-instance".to_string(),
            engine_version: engine_version.to_string(),
            min_compatible_engine: min_compatible.to_string(),
            schema_hash_core: "core-hash".to_string(),
            schema_hash_extensions: "ext-hash".to_string(),
            schema_hash_effective: schema_hash.to_string(),
            embedding_model_id: model.to_string(),
            embedding_dimension: 1536,
            built_at: "2026-05-22T00:00:00Z".to_string(),
            repos: vec![RepoStamp {
                url: "https://github.com/example/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                commits_behind_head: 0,
            }],
        }
    }

    fn make_manifest() -> Manifest {
        Manifest {
            repos: vec![ManifestRepo {
                url: "https://github.com/example/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                files_skipped: vec![],
            }],
        }
    }

    fn make_fake_db(dir: &Path) -> PathBuf {
        let db = dir.join("test.lbug");
        std::fs::write(&db, b"fake-graph-data").unwrap();
        db
    }

    #[test]
    fn build_creates_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_fake_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();

        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        assert!(snap_dir.join(GRAPH_FILE).exists(), "graph.lbug missing");
        assert!(
            snap_dir.join(MANIFEST_FILE).exists(),
            "manifest.json missing"
        );
        assert!(snap_dir.join(STAMP_FILE).exists(), "stamp.json missing");
        assert!(
            snap_dir.join(CHECKSUM_FILE).exists(),
            "checksum.sha256 missing"
        );
    }

    #[test]
    fn verify_passes_valid_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_fake_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        let loaded = verify_snapshot(&snap_dir).unwrap();
        assert_eq!(loaded.instance_id, "test-instance");
        assert_eq!(loaded.schema_hash_effective, "schema-hash-abc");
    }

    #[test]
    fn verify_fails_tampered_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_fake_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        // Tamper with stamp.json
        std::fs::write(
            snap_dir.join(STAMP_FILE),
            r#"{"instance_id":"tampered","engine_version":"0.1.0","min_compatible_engine":"0.1.0","schema_hash_core":"x","schema_hash_extensions":"x","schema_hash_effective":"evil-hash","embedding_model_id":"bad","embedding_dimension":0,"built_at":"","repos":[]}"#,
        )
        .unwrap();

        let result = verify_snapshot(&snap_dir);
        assert!(
            result.is_err(),
            "tampered snapshot should fail verification"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("integrity check failed") || msg.contains("checksum"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn load_rejects_incompatible_engine() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_fake_db(dir.path());

        // Snapshot requires engine >= 2.0.0
        let stamp = make_stamp(
            "2.0.0",
            "2.0.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        // Try to load with engine 1.0.0
        let result = load_snapshot(
            &snap_dir,
            "1.0.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("engine") && (msg.contains("rebuild") || msg.contains("requires")),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn load_rejects_mismatched_schema() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_fake_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "old-schema-hash",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        let result = load_snapshot(
            &snap_dir,
            "0.1.0",
            "new-schema-hash",
            "text-embedding-3-small",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("schema") && msg.contains("rebuild"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn load_rejects_mismatched_embedding_model() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_fake_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        let result = load_snapshot(
            &snap_dir,
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-ada-002",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("embedding model") && msg.contains("rebuild"),
            "unexpected error: {msg}"
        );
    }
}

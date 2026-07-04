use std::collections::HashMap;
use std::path::Path;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::{CancelReason, StoreError};
use crate::ranking::SeedResolutionConfig;

// ---------------------------------------------------------------------------
// EmbeddingIndex
// ---------------------------------------------------------------------------

/// In-memory embedding index backed by a binary sidecar file.
///
/// Binary format (v1):
/// ```text
/// [header: 16 bytes]
///   magic: b"NWEM" (4 bytes)
///   version: u32 LE (4 bytes) = 1
///   dimension: u32 LE (4 bytes)
///   count: u32 LE (4 bytes)
/// [uid table: count entries]
///   uid_len: u16 LE (2 bytes)
///   uid: [u8; uid_len]
/// [vectors: count * dimension * 4 bytes]
///   Contiguous f32 LE array, one vector per row
/// ```
pub struct EmbeddingIndex {
    embeddings: HashMap<String, Vec<f32>>, // uid -> embedding vector
}

impl Default for EmbeddingIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddingIndex {
    pub fn new() -> Self {
        Self {
            embeddings: HashMap::new(),
        }
    }

    pub fn add(&mut self, uid: &str, embedding: Vec<f32>) {
        // Keep the index homogeneous. A mixed-dimension index breaks the binary
        // sidecar (`save_binary` writes one header dim but per-vector bytes;
        // `load_binary` reads a fixed stride) → misaligned garbage or a load
        // failure that silently kills semantic search. A dimension mismatch here
        // means a vector from a different model — e.g. the daemon's local-fallback
        // (384) leaking into a remote-embedded (768/1536) index on a transient
        // remote outage, or a model switch without `--force`. Reject it rather
        // than corrupt the index; the caller should re-embed with `--force`.
        if let Some(existing) = self.embeddings.values().next()
            && embedding.len() != existing.len()
        {
            tracing::warn!(
                uid,
                got = embedding.len(),
                expected = existing.len(),
                "skipping embedding with mismatched dimension (re-embed with --force to switch models)"
            );
            return;
        }
        self.embeddings.insert(uid.to_string(), embedding);
    }

    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        let json = serde_json::to_string(&self.embeddings)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, anyhow::Error> {
        let json = std::fs::read_to_string(path)?;
        let embeddings: HashMap<String, Vec<f32>> = serde_json::from_str(&json)?;
        Ok(Self { embeddings })
    }

    // -- Binary persistence -------------------------------------------------

    /// Write the index in the compact binary sidecar format.
    pub fn save_binary(&self, path: &Path) -> Result<(), anyhow::Error> {
        use std::io::Write;
        let dim = self.dimension().unwrap_or(0);
        let count = self.embeddings.len() as u32;

        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);

        // Header
        file.write_all(b"NWEM")?;
        file.write_all(&1u32.to_le_bytes())?; // version
        file.write_all(&(dim as u32).to_le_bytes())?;
        file.write_all(&count.to_le_bytes())?;

        // Collect keys in deterministic order
        let mut entries: Vec<(&String, &Vec<f32>)> = self.embeddings.iter().collect();
        entries.sort_by_key(|(k, _)| k.as_str());

        // UID table
        for (uid, _) in &entries {
            let bytes = uid.as_bytes();
            file.write_all(&(bytes.len() as u16).to_le_bytes())?;
            file.write_all(bytes)?;
        }

        // Vectors (contiguous f32 LE)
        for (_, vec) in &entries {
            for &val in vec.iter() {
                file.write_all(&val.to_le_bytes())?;
            }
        }

        file.flush()?;
        Ok(())
    }

    /// Read the index from the compact binary sidecar format.
    pub fn load_binary(path: &Path) -> Result<Self, anyhow::Error> {
        let data = std::fs::read(path)?;
        if data.len() < 16 {
            anyhow::bail!("embedding file too small");
        }
        if &data[0..4] != b"NWEM" {
            anyhow::bail!("invalid embedding file magic");
        }
        let version = u32::from_le_bytes(data[4..8].try_into()?);
        if version != 1 {
            anyhow::bail!("unsupported embedding file version {version}");
        }
        let dim = u32::from_le_bytes(data[8..12].try_into()?) as usize;
        let count = u32::from_le_bytes(data[12..16].try_into()?) as usize;

        let mut offset = 16;
        let mut uids = Vec::with_capacity(count);

        // Read UID table
        for _ in 0..count {
            if offset + 2 > data.len() {
                anyhow::bail!("truncated uid table");
            }
            let uid_len = u16::from_le_bytes(data[offset..offset + 2].try_into()?) as usize;
            offset += 2;
            if offset + uid_len > data.len() {
                anyhow::bail!("truncated uid");
            }
            let uid = std::str::from_utf8(&data[offset..offset + uid_len])?.to_string();
            offset += uid_len;
            uids.push(uid);
        }

        // Read vectors
        let vec_bytes = count * dim * 4;
        if offset + vec_bytes > data.len() {
            anyhow::bail!("truncated vectors");
        }

        let mut embeddings = HashMap::with_capacity(count);
        for (i, uid) in uids.into_iter().enumerate() {
            let start = offset + i * dim * 4;
            let vec: Vec<f32> = (0..dim)
                .map(|j| {
                    f32::from_le_bytes(data[start + j * 4..start + j * 4 + 4].try_into().unwrap())
                })
                .collect();
            embeddings.insert(uid, vec);
        }

        Ok(Self { embeddings })
    }

    /// Return the top-`limit` (uid, similarity) pairs sorted descending.
    ///
    /// Uses rayon for parallel iteration and assumes stored embeddings are
    /// L2-normalized, so cosine similarity reduces to dot-product / query_norm.
    pub fn vector_search(&self, query_vec: &[f32], limit: usize) -> Vec<(String, f64)> {
        self.vector_search_cancellable(query_vec, limit, None)
            .expect("vector_search with cancel=None cannot be cancelled")
    }

    /// Like [`vector_search`], but cooperatively bails when `cancel` trips (a
    /// query timeout or client disconnect). Once tripped, per-embedding scoring
    /// is skipped so the parallel scan drains cheaply, then the whole call
    /// returns `Err(StoreError::Cancelled(_))` — a cancelled computation is
    /// *incomplete*, distinct from a legitimately empty result, so no caller
    /// mistakes the truncated scan for a real answer (or caches it).
    /// `cancel = None` never trips and is byte-for-byte the original behavior.
    pub fn vector_search_cancellable(
        &self,
        query_vec: &[f32],
        limit: usize,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        let query_norm: f64 = query_vec
            .iter()
            .map(|x| (*x as f64) * (*x as f64))
            .sum::<f64>()
            .sqrt();
        if query_norm == 0.0 {
            return Ok(vec![]);
        }

        let mut scores: Vec<(String, f64)> = self
            .embeddings
            .par_iter()
            .map(|(uid, emb)| {
                if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
                    return (uid.clone(), f64::NEG_INFINITY);
                }
                // Exclude any stored vector whose dimension differs from the
                // query's. `.zip()` would otherwise truncate to the shorter and
                // return a plausible-but-wrong similarity (dot over a prefix,
                // divided by the full query norm) — silently corrupting rankings
                // when the index was built with a different embedding model.
                if emb.len() != query_vec.len() {
                    return (uid.clone(), f64::NEG_INFINITY);
                }
                // Stored embeddings are L2-normalized, so cosine = dot / query_norm.
                let dot: f64 = emb
                    .iter()
                    .zip(query_vec.iter())
                    .map(|(a, b)| (*a as f64) * (*b as f64))
                    .sum();
                let sim = dot / query_norm;
                (uid.clone(), sim)
            })
            .collect();

        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
            // The shared cancel flag is a bare bool and can't carry a reason, so
            // the leaf always reports `Timeout` — the only reason the gRPC
            // boundary ever observes. (A client disconnect drops the request
            // future before any error is returned, so it never surfaces here.)
            return Err(StoreError::Cancelled(CancelReason::Timeout));
        }

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(limit);
        Ok(scores)
    }

    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }

    /// Return the dimensionality of the stored embeddings (length of the first
    /// vector found), or `None` if the index is empty.
    pub fn dimension(&self) -> Option<usize> {
        self.embeddings.values().next().map(|v| v.len())
    }

    /// Like `vector_search`, but pre-filters embeddings whose UID contains `uid_prefix`.
    /// When `uid_prefix` is `None`, behaves identically to `vector_search`.
    pub fn vector_search_filtered(
        &self,
        query_vec: &[f32],
        limit: usize,
        uid_prefix: Option<&str>,
    ) -> Vec<(String, f64)> {
        let query_norm: f64 = query_vec
            .iter()
            .map(|x| (*x as f64) * (*x as f64))
            .sum::<f64>()
            .sqrt();
        if query_norm == 0.0 {
            return vec![];
        }

        let mut scores: Vec<(String, f64)> = self
            .embeddings
            .par_iter()
            .filter(|(uid, _)| match uid_prefix {
                Some(prefix) => uid.contains(prefix),
                None => true,
            })
            .map(|(uid, emb)| {
                // See vector_search_cancellable: a dimension mismatch must be
                // excluded, not silently truncated by `.zip()`.
                if emb.len() != query_vec.len() {
                    return (uid.clone(), f64::NEG_INFINITY);
                }
                let dot: f64 = emb
                    .iter()
                    .zip(query_vec.iter())
                    .map(|(a, b)| (*a as f64) * (*b as f64))
                    .sum();
                let sim = dot / query_norm;
                (uid.clone(), sim)
            })
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(limit);
        scores
    }

    /// Look up the embedding for a given UID.
    pub fn get(&self, uid: &str) -> Option<&Vec<f32>> {
        self.embeddings.get(uid)
    }
}

#[cfg(test)]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum();
    let norm_a: f64 = a
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    let norm_b: f64 = b
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ---------------------------------------------------------------------------
// SearchResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub start_line: u32,
    pub signature: String,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Hybrid search on GraphStore
// ---------------------------------------------------------------------------

impl GraphStore {
    /// Hybrid search combining text (substring) and optional vector similarity via
    /// Reciprocal Rank Fusion (RRF).
    ///
    /// * `text_query`       – substring passed to `search_symbols_by_name`
    /// * `query_embedding`  – embedding of the query (optional)
    /// * `embedding_index`  – pre-loaded `EmbeddingIndex` (optional)
    /// * `limit`            – maximum results to return
    /// * `seed_resolution`  – path-deboost + kind-priority for seed scoring
    pub fn hybrid_search(
        &self,
        text_query: &str,
        query_embedding: Option<&[f32]>,
        embedding_index: Option<&EmbeddingIndex>,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
    ) -> Result<Vec<SearchResult>, StoreError> {
        self.hybrid_search_cancellable(
            text_query,
            query_embedding,
            embedding_index,
            limit,
            seed_resolution,
            None,
        )
    }

    /// Like [`hybrid_search`], but threads a cooperative cancellation flag into
    /// the parallel vector scan. `cancel = None` is the original behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search_cancellable(
        &self,
        text_query: &str,
        query_embedding: Option<&[f32]>,
        embedding_index: Option<&EmbeddingIndex>,
        limit: usize,
        seed_resolution: &SeedResolutionConfig,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Vec<SearchResult>, StoreError> {
        // 1. Text search
        let text_results = self.search_symbols_by_name(text_query, limit * 2, seed_resolution)?;

        // 2. Vector search (only when both embedding and index are present)
        let vec_results: Vec<(String, f64)> = match (query_embedding, embedding_index) {
            (Some(qe), Some(idx)) => idx.vector_search_cancellable(qe, limit * 2, cancel)?,
            _ => vec![],
        };

        // 3. Reciprocal Rank Fusion
        let k = 60.0_f64;
        let mut rrf_scores: HashMap<String, f64> = HashMap::new();

        for (rank, sym) in text_results.iter().enumerate() {
            *rrf_scores.entry(sym.uid.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, (uid, _)) in vec_results.iter().enumerate() {
            *rrf_scores.entry(uid.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        // 4. Sort by RRF score descending
        let mut merged: Vec<(String, f64)> = rrf_scores.into_iter().collect();
        merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        merged.truncate(limit);

        // 5. Map UIDs back to SearchResult, preferring the already-fetched text_results
        let results = merged
            .into_iter()
            .filter_map(|(uid, score)| {
                // Try text_results first (cheap path)
                let found = text_results.iter().find(|s| s.uid == uid);
                if let Some(s) = found {
                    return Some(SearchResult {
                        uid: s.uid.clone(),
                        name: s.name.clone(),
                        kind: s.kind.to_string(),
                        file_path: s.file_path.clone(),
                        start_line: s.start_line,
                        signature: s.signature.clone(),
                        score,
                    });
                }
                // Fall back to a point lookup for vector-only hits
                self.lookup_symbol(&uid).ok().map(|s| SearchResult {
                    uid: s.uid,
                    name: s.name,
                    kind: s.kind.to_string(),
                    file_path: s.file_path,
                    start_line: s.start_line,
                    signature: s.signature,
                    score,
                })
            })
            .collect();

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{Symbol, SymbolKind, Visibility};

    #[test]
    fn cosine_similarity_identical_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let s = cosine_similarity(&a, &a);
        assert!((s - 1.0).abs() < 1e-6, "expected 1.0, got {s}");
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let s = cosine_similarity(&a, &b);
        assert!(s.abs() < 1e-6, "expected 0.0, got {s}");
    }

    #[test]
    fn cosine_similarity_empty_returns_zero() {
        let s = cosine_similarity(&[], &[]);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn add_rejects_dimension_mismatched_vector() {
        // The index must stay homogeneous so the binary sidecar can't misalign.
        let mut idx = EmbeddingIndex::new();
        idx.add("sym:a", vec![1.0_f32, 0.0, 0.0]); // establishes dim 3
        idx.add("sym:b", vec![1.0_f32, 0.0]); // dim 2 — must be rejected
        assert_eq!(idx.len(), 1, "mismatched-dim vector must not be added");
        assert_eq!(idx.dimension(), Some(3));
    }

    #[test]
    fn vector_search_excludes_dimension_mismatched_vectors() {
        // Defense-in-depth for the query path: simulate a legacy/loaded index that
        // somehow holds a mismatched vector (insert directly, bypassing the add
        // guard). Before the query guard, `.zip()` truncated and returned a
        // plausible-but-wrong score.
        let mut idx = EmbeddingIndex::new();
        idx.add("sym:right", vec![1.0_f32, 0.0, 0.0]);
        idx.embeddings
            .insert("sym:wrongdim".to_string(), vec![1.0_f32, 0.0]);
        let query = vec![1.0_f32, 0.0, 0.0];
        let results = idx.vector_search(&query, 10);
        // The matching-dim vector scores ~1.0; the mismatched one is excluded
        // (NEG_INFINITY), so it never ranks above a real result.
        let right = results.iter().find(|(u, _)| u == "sym:right").unwrap();
        assert!((right.1 - 1.0).abs() < 1e-6, "got {}", right.1);
        let wrong = results.iter().find(|(u, _)| u == "sym:wrongdim").unwrap();
        assert!(
            wrong.1 == f64::NEG_INFINITY,
            "mismatched-dim vector must be excluded, got {}",
            wrong.1
        );
    }

    #[test]
    fn cosine_similarity_mismatched_len_returns_zero() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        let s = cosine_similarity(&a, &b);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn vector_search_cancellable_uncancelled_returns_results() {
        let mut idx = EmbeddingIndex::new();
        idx.add("a", vec![1.0, 0.0, 0.0]);
        idx.add("b", vec![0.9, 0.1, 0.0]);
        idx.add("c", vec![0.0, 0.0, 1.0]);

        // Not cancelled → normal results.
        let live = idx
            .vector_search_cancellable(&[1.0, 0.0, 0.0], 3, None)
            .expect("an uncancelled search cannot be cancelled");
        assert!(
            !live.is_empty(),
            "an uncancelled vector search returns results"
        );
    }

    #[test]
    fn vector_search_cancellable_returns_err_not_empty_on_cancel() {
        let mut idx = EmbeddingIndex::new();
        idx.add("a", vec![1.0, 0.0, 0.0]);
        idx.add("b", vec![0.9, 0.1, 0.0]);
        idx.add("c", vec![0.0, 0.0, 1.0]);

        // Pre-cancelled over a NON-empty candidate set: a cancelled computation
        // is incomplete, not empty — it must surface as a distinct error so no
        // caller mistakes the truncated scan for a legitimate empty result.
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let res = idx.vector_search_cancellable(&[1.0, 0.0, 0.0], 3, Some(&cancel));
        assert!(
            matches!(res, Err(StoreError::Cancelled(_))),
            "a cancelled vector search must return Err(Cancelled), got {res:?}"
        );
    }

    #[test]
    fn vector_search_returns_most_similar() {
        let mut idx = EmbeddingIndex::new();
        idx.add("a", vec![1.0, 0.0, 0.0]);
        idx.add("b", vec![0.9, 0.1, 0.0]);
        idx.add("c", vec![0.0, 0.0, 1.0]);

        let results = idx.vector_search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "a");
        assert_eq!(results[1].0, "b");
    }

    #[test]
    fn vector_search_limit_respected() {
        let mut idx = EmbeddingIndex::new();
        for i in 0..10 {
            idx.add(&format!("sym:{i}"), vec![i as f32, 0.0]);
        }
        let results = idx.vector_search(&[1.0, 0.0], 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn embedding_index_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings.json");
        let mut idx = EmbeddingIndex::new();
        // Use an L2-normalized vector (vector_search assumes pre-normalized embeddings)
        let norm = (0.1_f32 * 0.1 + 0.2 * 0.2 + 0.3 * 0.3).sqrt();
        let v = vec![0.1 / norm, 0.2 / norm, 0.3 / norm];
        idx.add("sym:test", v.clone());
        idx.save(&path).unwrap();

        let loaded = EmbeddingIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        let results = loaded.vector_search(&v, 1);
        assert_eq!(results[0].0, "sym:test");
        assert!((results[0].1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn embedding_index_is_empty() {
        let idx = EmbeddingIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    fn make_symbol(uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "r".to_string(),
            file_path: "a.js".to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("function {name}()"),
            summary: None,
            content_hash: "x".to_string(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn empty_seed_resolution() -> SeedResolutionConfig {
        SeedResolutionConfig {
            path_deboost: Vec::new(),
            kind_priority: Vec::new(),
        }
    }

    #[test]
    fn hybrid_search_text_only() {
        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&make_symbol("sym:1", "greet")).unwrap();

        let results = store
            .hybrid_search("greet", None, None, 10, &empty_seed_resolution())
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "greet");
    }

    #[test]
    fn hybrid_search_no_match_returns_empty() {
        let store = GraphStore::in_memory().unwrap();
        store.insert_symbol(&make_symbol("sym:1", "greet")).unwrap();

        let results = store
            .hybrid_search("zzznomatch", None, None, 10, &empty_seed_resolution())
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn binary_save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings.bin");

        let mut idx = EmbeddingIndex::new();
        idx.add("sym:alpha", vec![0.1, 0.2, 0.3]);
        idx.add("sym:beta", vec![0.4, 0.5, 0.6]);
        idx.add("sym:gamma", vec![0.7, 0.8, 0.9]);
        idx.save_binary(&path).unwrap();

        let loaded = EmbeddingIndex::load_binary(&path).unwrap();
        assert_eq!(loaded.len(), 3);

        // Verify each vector survived the round-trip
        for uid in &["sym:alpha", "sym:beta", "sym:gamma"] {
            let orig = idx.get(uid).unwrap();
            let rt = loaded.get(uid).unwrap();
            assert_eq!(orig, rt, "round-trip mismatch for {uid}");
        }
    }

    #[test]
    fn binary_empty_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");

        let idx = EmbeddingIndex::new();
        idx.save_binary(&path).unwrap();

        let loaded = EmbeddingIndex::load_binary(&path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn binary_load_rejects_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.bin");
        std::fs::write(&path, b"BADMxxxxxxxxxxxx").unwrap();

        match EmbeddingIndex::load_binary(&path) {
            Ok(_) => panic!("should reject bad magic"),
            Err(e) => assert!(
                format!("{e}").contains("invalid embedding file magic"),
                "unexpected error: {e}",
            ),
        }
    }

    #[test]
    fn binary_load_rejects_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.bin");
        std::fs::write(&path, b"NWEM").unwrap(); // only 4 bytes, need 16

        assert!(EmbeddingIndex::load_binary(&path).is_err());
    }

    #[test]
    fn hybrid_search_with_vector_boosts_vector_hits() {
        let store = GraphStore::in_memory().unwrap();
        // Insert "greet" (matches text) and "farewell" (does not match text)
        store
            .insert_symbol(&make_symbol("sym:greet", "greet"))
            .unwrap();
        store
            .insert_symbol(&make_symbol("sym:farewell", "farewell"))
            .unwrap();

        let mut idx = EmbeddingIndex::new();
        // farewell gets a perfect embedding match; greet gets a distant one
        idx.add("sym:farewell", vec![1.0, 0.0, 0.0]);
        idx.add("sym:greet", vec![0.0, 1.0, 0.0]);

        let query_vec = [1.0_f32, 0.0, 0.0];
        let results = store
            .hybrid_search(
                "greet",
                Some(&query_vec),
                Some(&idx),
                10,
                &empty_seed_resolution(),
            )
            .unwrap();

        // Both should appear (greet from text, farewell from vector)
        let uids: Vec<&str> = results.iter().map(|r| r.uid.as_str()).collect();
        assert!(uids.contains(&"sym:greet"), "greet should be in results");
        assert!(
            uids.contains(&"sym:farewell"),
            "farewell should be in results via vector"
        );
    }
}

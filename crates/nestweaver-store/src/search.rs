use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::db::GraphStore;
use crate::error::StoreError;
use crate::ranking::SeedResolutionConfig;

// ---------------------------------------------------------------------------
// EmbeddingIndex
// ---------------------------------------------------------------------------

/// In-memory embedding index backed by a JSON sidecar file.
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

    /// Return the top-`limit` (uid, cosine_similarity) pairs sorted descending.
    pub fn vector_search(&self, query_vec: &[f32], limit: usize) -> Vec<(String, f64)> {
        let mut scores: Vec<(String, f64)> = self
            .embeddings
            .iter()
            .map(|(uid, emb)| (uid.clone(), cosine_similarity(query_vec, emb)))
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(limit);
        scores
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

    /// Look up the embedding for a given UID.
    pub fn get(&self, uid: &str) -> Option<&Vec<f32>> {
        self.embeddings.get(uid)
    }
}

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
        // 1. Text search
        let text_results = self.search_symbols_by_name(text_query, limit * 2, seed_resolution)?;

        // 2. Vector search (only when both embedding and index are present)
        let vec_results: Vec<(String, f64)> = match (query_embedding, embedding_index) {
            (Some(qe), Some(idx)) => idx.vector_search(qe, limit * 2),
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
    fn cosine_similarity_mismatched_len_returns_zero() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        let s = cosine_similarity(&a, &b);
        assert_eq!(s, 0.0);
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
        idx.add("sym:test", vec![0.1, 0.2, 0.3]);
        idx.save(&path).unwrap();

        let loaded = EmbeddingIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        let results = loaded.vector_search(&[0.1, 0.2, 0.3], 1);
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

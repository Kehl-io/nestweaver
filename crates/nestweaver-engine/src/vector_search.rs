use nestweaver_store::GraphStore;

/// Search across symbols, notes, AND headings by cosine similarity.
/// Returns (uid, similarity) pairs sorted descending, limited to `limit`.
/// Delegates to the sidecar `EmbeddingIndex` which stores all node kinds
/// in a single index (keyed by UID prefix: `sym:`, `note:`, `heading:`).
pub fn vector_knn_all(
    store: &GraphStore,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(String, f64)>, anyhow::Error> {
    Ok(store.vector_search(query_embedding, limit))
}

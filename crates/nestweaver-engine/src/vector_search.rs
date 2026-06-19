use nestweaver_store::GraphStore;

/// Cosine similarity between two embedding vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| *x as f64 * *y as f64)
        .sum();
    let norm_a: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Search for nodes whose stored embedding is closest to `query_embedding`.
/// Returns (uid, similarity) pairs sorted descending, limited to `limit`.
/// Delegates to the sidecar `EmbeddingIndex` on the store which persists
/// across DB sessions.
pub fn vector_knn(
    store: &GraphStore,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(String, f64)>, anyhow::Error> {
    Ok(store.vector_search(query_embedding, limit))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_empty_vectors() {
        let sim = cosine_similarity(&[], &[]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }
}

use nestweaver_algorithms::graph::{EdgeWeightConfig, InMemoryGraph};
use nestweaver_algorithms::ppr::{self, PprConfig};
use wasm_bindgen::prelude::*;

/// A graph loaded from MessagePack bytes, ready for in-browser algorithm execution.
#[wasm_bindgen]
pub struct WasmGraph {
    graph: InMemoryGraph,
}

#[wasm_bindgen]
impl WasmGraph {
    /// Deserialize a graph from MessagePack bytes.
    #[wasm_bindgen(constructor)]
    pub fn new(data: &[u8]) -> Result<WasmGraph, JsError> {
        let graph: InMemoryGraph = rmp_serde::from_slice(data)
            .map_err(|e| JsError::new(&format!("Failed to deserialize graph: {e}")))?;
        Ok(WasmGraph { graph })
    }

    /// Get the graph generation number.
    pub fn generation(&self) -> u64 {
        self.graph.generation
    }

    /// Get node count.
    pub fn node_count(&self) -> usize {
        self.graph.uids.len()
    }

    /// Get edge count.
    pub fn edge_count(&self) -> usize {
        self.graph.edges.len()
    }

    /// Run PPR with given seed UIDs (JSON array of strings) and return results as JSON.
    ///
    /// Returns a JSON array of `[uid, score]` pairs sorted descending by score.
    pub fn ppr(&self, seeds_json: &str, damping: f64) -> Result<String, JsError> {
        let seeds: Vec<String> = serde_json::from_str(seeds_json)
            .map_err(|e| JsError::new(&format!("Invalid seeds JSON: {e}")))?;

        let adjacency = self.graph.build_adjacency(&EdgeWeightConfig::default_config());
        let config = PprConfig {
            damping,
            max_iterations: 20,
            min_score: 1e-4,
            interaction_scores: None,
            interaction_bias_weight: 0.05,
        };
        let results = ppr::personalized_pagerank(&self.graph.uids, &adjacency, &seeds, &config);

        serde_json::to_string(&results)
            .map_err(|e| JsError::new(&format!("Failed to serialize results: {e}")))
    }
}

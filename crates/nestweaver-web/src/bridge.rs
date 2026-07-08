//! Scene-level bridge annotation for web payloads.
//!
//! Reuses the engine's betweenness-centrality implementation
//! ([`nestweaver_engine::find_bridge_nodes`], Brandes' algorithm with source
//! sampling — the same call backing the MCP `bridge_nodes` tool) rather than
//! reimplementing centrality. The raw scores are advisory emphasis for the UI
//! (bridge glyphs), not truth-critical data.
//!
//! Cost model: on small graphs the engine computes exact betweenness; on
//! large graphs it samples up to 500 BFS sources, which can take seconds at
//! ~58k symbols. To keep the overview/context latency budget, the global
//! bridge pool is computed **once per process** and cached on [`AppState`]
//! (see [`global_bridge_scores`]). Every request after the first is a hash
//! lookup. The cache can go stale as the graph is re-indexed within a
//! process lifetime — acceptable for advisory glyph data.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::state::AppState;

/// Maximum number of bridges annotated with a `bridge_score` per scene
/// (one overview response or one context response).
pub const SCENE_BRIDGE_LIMIT: usize = 12;

/// Size of the cached global bridge pool. Scenes intersect against this
/// pool, so it must comfortably exceed any scene size (overview is capped
/// at 100 landmarks). Nodes outside the global top pool are, by
/// definition, not notable bridges and simply get no `bridge_score`.
const BRIDGE_POOL: usize = 512;

/// Return the process-wide bridge score pool (uid -> raw betweenness),
/// computing it on first use via the engine's sampled Brandes
/// implementation. Errors degrade to an empty pool: bridge glyphs are
/// advisory, so a failed centrality pass must never fail the request.
pub fn global_bridge_scores(state: &AppState) -> Arc<HashMap<String, f64>> {
    state
        .bridge_scores
        .get_or_init(|| {
            // The sampled Brandes pass can take seconds on large stores; keep
            // the cold-cache computation off the async worker threads so the
            // first overview request doesn't stall the runtime. block_in_place
            // panics on current-thread runtimes (tokio::test), so fall back to
            // inline computation there.
            let compute = || {
                let bridges = nestweaver_engine::find_bridge_nodes(&state.store, BRIDGE_POOL)
                    .unwrap_or_default();
                Arc::new(
                    bridges
                        .into_iter()
                        .filter(|bridge| bridge.betweenness_score > 0.0)
                        .map(|bridge| (bridge.uid, bridge.betweenness_score))
                        .collect(),
                )
            };
            match tokio::runtime::Handle::try_current() {
                Ok(handle)
                    if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
                {
                    tokio::task::block_in_place(compute)
                }
                _ => compute(),
            }
        })
        .clone()
}

/// Intersect a scene's node uids with the global bridge pool and return
/// normalized scores for at most [`SCENE_BRIDGE_LIMIT`] nodes.
///
/// Scores are normalized 0..=1 by the scene maximum, so the strongest
/// bridge *within the scene* always reads 1.0. Nodes outside the returned
/// map should serialize without a `bridge_score` field.
pub fn scene_bridge_scores(
    global: &HashMap<String, f64>,
    scene_uids: impl IntoIterator<Item = String>,
) -> HashMap<String, f32> {
    let unique: HashSet<String> = scene_uids.into_iter().collect();
    let mut hits: Vec<(String, f64)> = unique
        .into_iter()
        .filter_map(|uid| global.get(&uid).map(|&score| (uid, score)))
        .collect();
    hits.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    hits.truncate(SCENE_BRIDGE_LIMIT);

    let max = hits.first().map(|(_, score)| *score).unwrap_or(0.0);
    if max <= 0.0 {
        return HashMap::new();
    }
    hits.into_iter()
        .map(|(uid, score)| (uid, ((score / max) as f32).clamp(0.0, 1.0)))
        .collect()
}

/// Annotate an already-serialized context payload (an object with `seeds`
/// and/or `connected` arrays of nodes carrying a `uid`) with per-scene
/// `bridge_score` fields. Both arrays together form one scene, so the
/// top-[`SCENE_BRIDGE_LIMIT`] cap and normalization span the whole response.
///
/// Additive only: nodes that are not scene bridges are left untouched, so
/// the payload stays backward compatible.
pub fn annotate_context_payload(state: &AppState, json: &mut serde_json::Value) {
    let serde_json::Value::Object(object) = json else {
        return;
    };

    let scene_uids: Vec<String> = ["seeds", "connected"]
        .iter()
        .filter_map(|key| object.get(*key))
        .filter_map(|value| value.as_array())
        .flatten()
        .filter_map(|node| node.get("uid").and_then(|uid| uid.as_str()))
        .map(str::to_string)
        .collect();
    if scene_uids.is_empty() {
        return;
    }

    let scores = scene_bridge_scores(&global_bridge_scores(state), scene_uids);
    if scores.is_empty() {
        return;
    }

    for key in ["seeds", "connected"] {
        let Some(nodes) = object.get_mut(key).and_then(|value| value.as_array_mut()) else {
            continue;
        };
        for node in nodes {
            let Some(node_object) = node.as_object_mut() else {
                continue;
            };
            let score = node_object
                .get("uid")
                .and_then(|uid| uid.as_str())
                .and_then(|uid| scores.get(uid))
                .copied();
            if let Some(score) = score {
                node_object.insert("bridge_score".to_string(), serde_json::json!(score));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_scores_normalize_by_scene_max_and_cap_at_limit() {
        let global: HashMap<String, f64> = (0..20)
            .map(|i| (format!("sym:{i}"), (i + 1) as f64))
            .collect();
        let scene: Vec<String> = (0..20).map(|i| format!("sym:{i}")).collect();

        let scores = scene_bridge_scores(&global, scene);
        assert_eq!(scores.len(), SCENE_BRIDGE_LIMIT);
        // The strongest scene bridge is normalized to exactly 1.0.
        assert_eq!(scores.get("sym:19").copied(), Some(1.0));
        // Everything is within 0..=1.
        assert!(scores.values().all(|s| (0.0..=1.0).contains(s)));
        // Weakest members of the scene fall outside the top-12 cut.
        assert!(!scores.contains_key("sym:0"));
    }

    #[test]
    fn scene_scores_empty_when_no_scene_node_is_a_bridge() {
        let global: HashMap<String, f64> = HashMap::from([("sym:hub".to_string(), 5.0)]);
        let scores = scene_bridge_scores(&global, vec!["sym:other".to_string()]);
        assert!(scores.is_empty());
    }
}

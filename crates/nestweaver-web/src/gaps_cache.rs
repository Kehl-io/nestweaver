use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use nestweaver_store::GraphStore;
use serde_json::json;

use crate::error::ApiError;

struct CachedGaps {
    generation: u64,
    response: serde_json::Value,
}

pub struct GapsCache {
    cached: RwLock<Option<CachedGaps>>,
}

impl GapsCache {
    pub fn new() -> Self {
        Self {
            cached: RwLock::new(None),
        }
    }

    pub fn get_or_compute(&self, store: &GraphStore) -> Result<serde_json::Value, ApiError> {
        let current_gen = store.graph_generation();

        // Fast path: read lock, check generation
        {
            let guard = self.cached.read().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = guard.as_ref() {
                if cached.generation == current_gen {
                    return Ok(cached.response.clone());
                }
            }
        }

        // Slow path: write lock, double-check, compute
        let mut guard = self.cached.write().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.generation == current_gen {
                return Ok(cached.response.clone());
            }
        }

        let response = Self::compute(store)?;
        *guard = Some(CachedGaps {
            generation: current_gen,
            response: response.clone(),
        });
        Ok(response)
    }

    fn compute(store: &GraphStore) -> Result<serde_json::Value, ApiError> {
        // --- undocumented ---
        let refs_count = store.count_references_code_edges()?;
        let undocumented = if refs_count == 0 {
            let repos = nestweaver_engine::list_repos(store, None)?;
            let mut module_counts: HashMap<String, usize> = HashMap::new();

            for repo in &repos {
                if let Ok(symbols) = store.lookup_symbols_by_repo(&repo.uid) {
                    for sym in &symbols {
                        let module = sym
                            .file_path
                            .split('/')
                            .next()
                            .unwrap_or(&sym.file_path)
                            .to_string();
                        *module_counts.entry(module).or_insert(0) += 1;
                    }
                }
            }

            let mut modules: Vec<serde_json::Value> = module_counts
                .into_iter()
                .map(|(module, count)| {
                    json!({
                        "module": module,
                        "symbol_count": count,
                    })
                })
                .collect();
            modules.sort_by(|a, b| {
                a["module"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["module"].as_str().unwrap_or(""))
            });
            modules
        } else {
            Vec::new()
        };

        // --- untested ---
        let services = nestweaver_engine::list_services(store, None)?;
        let all_uids: HashSet<String> = services.iter().map(|s| s.uid.clone()).collect();
        let tested = store.tested_service_uids()?;
        let mut untested: Vec<String> = all_uids.difference(&tested).cloned().collect();
        untested.sort();

        // --- disconnected_pairs ---
        let disconnected_pairs: Vec<serde_json::Value> = Vec::new();

        Ok(json!({
            "undocumented": undocumented,
            "untested": untested,
            "disconnected_pairs": disconnected_pairs,
        }))
    }
}

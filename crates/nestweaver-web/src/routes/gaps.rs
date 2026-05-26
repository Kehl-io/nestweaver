use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub async fn gaps(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    // --- undocumented ---
    // Group symbols by top-level directory. If no notes reference any code
    // (count_references_code_edges == 0), list all modules with symbol counts.
    let refs_count = state.store.count_references_code_edges()?;

    let undocumented = if refs_count == 0 {
        // Get all symbols to group by top-level directory
        let repos = nestweaver_engine::list_repos(&state.store, None)?;
        let mut module_counts: HashMap<String, usize> = HashMap::new();

        for repo in &repos {
            if let Ok(symbols) = state.store.lookup_symbols_by_repo(&repo.uid) {
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
    // For each service, check if any caller has a test/spec file path.
    let services = nestweaver_engine::list_services(&state.store, None)?;
    let mut untested: Vec<String> = Vec::new();

    for svc in &services {
        let callers: Vec<nestweaver_schema::Symbol> =
            state.store.callers_of(&svc.uid).unwrap_or_default();
        let has_test_caller = callers.iter().any(|c| {
            let path = c.file_path.to_lowercase();
            path.contains("test") || path.contains("spec")
        });
        if !has_test_caller {
            untested.push(svc.uid.clone());
        }
    }

    // --- disconnected_pairs ---
    // Empty for v1 (requires community detection which runs client-side).
    let disconnected_pairs: Vec<serde_json::Value> = Vec::new();

    Ok(Json(json!({
        "undocumented": undocumented,
        "untested": untested,
        "disconnected_pairs": disconnected_pairs,
    }))
    .into_response())
}

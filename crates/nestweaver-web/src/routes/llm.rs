use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use nestweaver_engine::{HybridSearchConfig, build_brain_context_hybrid};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LlmQueryRequest {
    pub query: String,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
}

fn default_token_budget() -> usize {
    4000
}

pub async fn query(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LlmQueryRequest>,
) -> Result<Response, ApiError> {
    let seeds: Vec<String> = body
        .query
        .split_whitespace()
        .filter(|w| w.len() > 3)
        .map(|w| w.to_string())
        .collect();

    if seeds.is_empty() {
        return Err(ApiError::bad_request(
            "No meaningful keywords found in query (all words must be longer than 3 characters)",
        ));
    }

    let seed_count = seeds.len();
    let config = HybridSearchConfig::default();
    let result =
        build_brain_context_hybrid(&state.store, &seeds, state.tantivy.as_ref(), &config, None);

    let explanation = format!(
        "Keyword extraction fallback: extracted {} seeds from query",
        seed_count
    );

    match result {
        Ok(ctx) => {
            let context = serde_json::to_value(&ctx)?;
            Ok(Json(json!({
                "seeds": seeds,
                "explanation": explanation,
                "context": context,
            }))
            .into_response())
        }
        Err(_) => {
            // No seeds resolved — return empty context rather than 500
            Ok(Json(json!({
                "seeds": seeds,
                "explanation": format!("{} (no matching symbols found)", explanation),
                "context": {
                    "seeds": [],
                    "connected": [],
                    "unresolved_seeds": seeds,
                },
            }))
            .into_response())
        }
    }
}

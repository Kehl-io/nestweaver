use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use nestweaver_engine::{
    BrainContextResult, BrainNode, HybridSearchConfig, build_brain_context_hybrid,
};

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

/// A query token that was not used as a seed, with the reason it was left out.
#[derive(Debug, Serialize)]
struct DroppedToken {
    token: String,
    reason: &'static str,
}

pub async fn query(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LlmQueryRequest>,
) -> Result<Response, ApiError> {
    let (seeds, dropped) = extract_query_seeds(&body.query);
    if seeds.is_empty() {
        return Err(ApiError::bad_request(no_seed_message(&dropped)));
    }

    let config = HybridSearchConfig::default();
    let (mut context, unresolved) = match build_brain_context_hybrid(
        &state.store,
        &seeds,
        state.tantivy.as_deref(),
        &config,
        None,
        None,
    ) {
        Ok(context) => (context, false),
        // A name that resolved nothing is an empty context, as a vault
        // workspace is on `/api/v1/brain/context`. Embedding-identity,
        // PageRank publication, and cancellation failures stay errors.
        Err(err) if unresolved_seed_error(&err) => (
            BrainContextResult {
                unresolved_seeds: seeds.clone(),
                ..Default::default()
            },
            true,
        ),
        Err(err) => return Err(map_context_engine_error(err)),
    };
    // This route supplies no embedder. Say so for a completed result and for
    // an unresolved seed. Do not rewrite an engine error into
    // `web_surface_unsupported`, and do not tell the caller to rebuild
    // embeddings. Inference, pipeline, and artifact failures on an Ok result
    // keep their own reasons.
    disclose_web_semantic_surface(&mut context);
    let (tokens_used, truncated) = apply_token_budget(&mut context, body.token_budget);

    let mut explanation = format!(
        "Keyword extraction fallback: extracted {} seeds from query",
        seeds.len()
    );
    append_dropped(&mut explanation, &dropped);
    if unresolved {
        explanation.push_str(" (no matching symbols found)");
    }

    let context = serde_json::to_value(&context)?;
    Ok(Json(json!({
        "seeds": seeds,
        "dropped_tokens": dropped,
        "explanation": explanation,
        "token_budget": body.token_budget,
        "tokens_used": tokens_used,
        "truncated": truncated,
        "truncated_by": if truncated { Some("token_budget") } else { None::<&str> },
        "context": context,
    }))
    .into_response())
}

/// Function words that are not identifier seeds. Short names such as `id`
/// and `api` stay. A question like "how does the auth work" must not turn
/// "how" or "the" into an ambiguous-seed 400.
const QUERY_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "does", "for", "from", "how", "in", "is",
    "it", "of", "on", "or", "the", "to", "what", "when", "where", "which", "who", "why", "with",
];

/// Keep every token that can be an identifier, including short ones (`id`,
/// `api`). Single-character tokens and function words are not seeds.
fn extract_query_seeds(query: &str) -> (Vec<String>, Vec<DroppedToken>) {
    let mut seeds = Vec::new();
    let mut dropped = Vec::new();
    for word in query.split_whitespace() {
        // Strip surrounding punctuation so natural-language questions seed
        // resolvable names ("what is main?" -> "main", not "main?", which
        // matches nothing). Interior characters are kept — identifiers
        // legitimately contain `_`, `::`, etc.
        let token = word
            .trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'))
            .to_string();
        if token.is_empty() {
            continue;
        }
        if token.chars().count() < 2 {
            dropped.push(DroppedToken {
                token,
                reason: "single_character",
            });
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if QUERY_STOPWORDS.contains(&lower.as_str()) {
            dropped.push(DroppedToken {
                token,
                reason: "stopword",
            });
            continue;
        }
        seeds.push(token);
    }
    (seeds, dropped)
}

fn no_seed_message(dropped: &[DroppedToken]) -> String {
    if dropped.is_empty() {
        return "No identifier seeds found in query".to_string();
    }
    format!(
        "No identifier seeds found in query; dropped single-character tokens because they are not used as seeds: {}",
        dropped
            .iter()
            .map(|token| token.token.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn append_dropped(explanation: &mut String, dropped: &[DroppedToken]) {
    if dropped.is_empty() {
        return;
    }
    explanation.push_str("; dropped single-character tokens because they are not used as seeds: ");
    explanation.push_str(
        &dropped
            .iter()
            .map(|token| token.token.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    );
}

fn unresolved_seed_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("No seeds resolved"))
}

/// Same classification as `/api/v1/brain/context`: empty containers, missing
/// and ambiguous seeds, then [`ApiError::from_context_read`] for
/// embedding-identity, PageRank publication, and cancellation.
fn map_context_engine_error(err: anyhow::Error) -> ApiError {
    let message = err.to_string();
    if let Some(engine) = err
        .chain()
        .map(|cause| cause.to_string())
        .find(|text| text.contains(nestweaver_engine::query::EMPTY_SEED_CONTAINER))
    {
        let mut remedies = Vec::new();
        if engine.contains(" has no notes") {
            remedies
                .push("a seed names a vault with no notes: add notes to the vault and refresh it");
        }
        if engine.contains(" has no indexed symbols") {
            remedies.push("a seed names a repository with no indexed symbols: re-index it");
        }
        let message = remedies.join("; ");
        return ApiError::conflict_with_body(
            message.clone(),
            serde_json::json!({
                "error": "seed container is empty",
                "status": "empty",
                "message": message,
            }),
        );
    }
    if message.contains("No matching symbols")
        || message.contains("No symbols found")
        || message.contains("No seeds resolved")
    {
        return ApiError::not_found("no matching context seeds");
    }
    if message.contains("Ambiguous") {
        return ApiError::bad_request("ambiguous context seed");
    }
    ApiError::from_context_read(err)
}

/// Replace the engine's no-embedder `model_unavailable` placeholder.
///
/// A result that already names some other semantic failure — inference,
/// pipeline, or artifact — is left alone. Those are real retrieval failures.
/// `model_unavailable` from this route is not: the handler never passes an
/// embedder, so the engine never attempted to load one.
fn disclose_web_semantic_surface(result: &mut BrainContextResult) {
    if semantic_failure_is_real(result) {
        return;
    }
    result.semantic_applied = false;
    result.semantic_unavailable = Some(serde_json::json!({
        "component": "semantic",
        "reason": "web_surface_unsupported",
        "stage": "route_capability",
        "remediation": "Use a configured CLI or MCP surface for semantic context."
    }));
    if !result
        .degraded_components
        .iter()
        .any(|component| component == "semantic")
    {
        result.degraded_components.push("semantic".into());
    }
}

fn semantic_failure_is_real(result: &BrainContextResult) -> bool {
    if result.semantic_applied {
        return true;
    }
    result
        .semantic_unavailable
        .as_ref()
        .and_then(|detail| detail.get("reason"))
        .and_then(|reason| reason.as_str())
        .is_some_and(|reason| reason != "model_unavailable")
}

/// Detailed-node token estimate, matching `nestweaver_mcp::tools::render_cost`
/// with `concise = false`: this route serializes uid, kind, title, location,
/// and relevance. Seeds are charged first because the response always includes
/// them. The engine then pins those same nodes at the front of `connected`.
/// A pinned copy that was already charged, and that does not fit in what
/// remains, is skipped so a later neighbor can still fill the budget.
fn apply_token_budget(context: &mut BrainContextResult, budget: usize) -> (usize, bool) {
    let mut used = 0usize;
    let seeds_truncated = take_within_budget(&mut context.seeds, budget, &mut used);
    let charged_seed_uids: std::collections::HashSet<String> =
        context.seeds.iter().map(|node| node.uid.clone()).collect();
    let connected_truncated = take_connected_within_budget(
        &mut context.connected,
        budget,
        &mut used,
        &charged_seed_uids,
    );
    (used, seeds_truncated || connected_truncated)
}

fn node_token_cost(node: &BrainNode) -> usize {
    (node.uid.len() + node.title.len() + node.kind.len() + node.location.len() + 10 + 80)
        .div_ceil(4)
}

fn take_within_budget(nodes: &mut Vec<BrainNode>, budget: usize, used: &mut usize) -> bool {
    let mut taken = 0usize;
    for node in nodes.iter() {
        let cost = node_token_cost(node);
        if used.saturating_add(cost) > budget {
            break;
        }
        *used += cost;
        taken += 1;
    }
    let truncated = taken < nodes.len();
    nodes.truncate(taken);
    truncated
}

fn take_connected_within_budget(
    nodes: &mut Vec<BrainNode>,
    budget: usize,
    used: &mut usize,
    charged_seed_uids: &std::collections::HashSet<String>,
) -> bool {
    let mut kept = Vec::with_capacity(nodes.len());
    let mut truncated = false;
    for node in std::mem::take(nodes) {
        let cost = node_token_cost(&node);
        if used.saturating_add(cost) > budget {
            if charged_seed_uids.contains(&node.uid) {
                continue;
            }
            truncated = true;
            break;
        }
        *used += cost;
        kept.push(node);
    }
    *nodes = kept;
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use futures::FutureExt;
    use nestweaver_schema::{Repo, Symbol, SymbolKind, Visibility};
    use nestweaver_store::{GraphScope, GraphStore};

    async fn response_json(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn fixture_state() -> Arc<AppState> {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&Repo {
                uid: "repo:test".into(),
                url: "https://example.com/test.git".into(),
                indexed_sha: "abc123".into(),
                staleness_commits_behind: 0,
                instance_id: String::new(),
                name: None,
                root_path: None,
            })
            .unwrap();
        for (uid, name) in [
            ("sym:test:greet", "greet"),
            ("sym:test:api", "api"),
            ("sym:test:id", "id"),
        ] {
            store
                .insert_symbol(&Symbol {
                    uid: uid.into(),
                    name: name.into(),
                    kind: SymbolKind::Function,
                    repo_uid: "repo:test".into(),
                    file_path: "src/main.js".into(),
                    start_line: 1,
                    end_line: 1,
                    signature: format!("function {name}()"),
                    summary: None,
                    content_hash: uid.into(),
                    embedding: None,
                    pagerank_score: Some(0.85),
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: Visibility::Inferred,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
        }
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        AppState::new(store, None, std::path::PathBuf::from("/tmp/llm-query.lbug"))
    }

    fn assert_web_surface(payload: &serde_json::Value) {
        assert_eq!(payload["context"]["semantic_applied"], false);
        assert_eq!(
            payload["context"]["semantic_unavailable"]["reason"],
            "web_surface_unsupported"
        );
        assert_eq!(
            payload["context"]["semantic_unavailable"]["stage"],
            "route_capability"
        );
        let remedy = payload["context"]["semantic_unavailable"]["remediation"]
            .as_str()
            .unwrap();
        assert!(
            remedy.contains("CLI") && remedy.contains("MCP"),
            "{payload}"
        );
        assert!(
            !remedy.contains("rebuild") && !remedy.contains("verify"),
            "{payload}"
        );
        assert_ne!(
            payload["context"]["semantic_unavailable"]["reason"],
            "model_unavailable"
        );
        assert!(
            payload["context"]["degraded_components"]
                .as_array()
                .unwrap()
                .contains(&json!("semantic"))
        );
    }

    #[tokio::test]
    async fn llm_query_discloses_web_surface_and_honours_token_budget() {
        let state = fixture_state();

        let wide = query(
            State(state.clone()),
            Json(LlmQueryRequest {
                query: "a api id greet".into(),
                token_budget: 4000,
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        let (status, wide) = response_json(wide).await;
        assert_eq!(status, StatusCode::OK, "{wide}");
        let seeds: Vec<&str> = wide["seeds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|seed| seed.as_str().unwrap())
            .collect();
        assert_eq!(seeds, ["api", "id", "greet"], "{wide}");
        let (question, _) = extract_query_seeds("How does the auth work");
        assert_eq!(question, ["auth", "work"]);
        assert_eq!(wide["dropped_tokens"][0]["token"], "a");
        assert_eq!(wide["dropped_tokens"][0]["reason"], "single_character");
        assert!(
            wide["explanation"]
                .as_str()
                .unwrap()
                .contains("single-character"),
            "{wide}"
        );
        assert_web_surface(&wide);
        let wide_nodes = wide["context"]["seeds"].as_array().unwrap().len()
            + wide["context"]["connected"].as_array().unwrap().len();
        assert!(wide_nodes > 0, "resolved identifiers should render: {wide}");
        assert_eq!(wide["truncated"], false, "{wide}");
        assert!(wide["truncated_by"].is_null(), "{wide}");
        let wide_used = wide["tokens_used"].as_u64().unwrap();
        assert!(wide_used > 0 && wide_used <= 4000, "{wide}");
        assert_eq!(wide["token_budget"], 4000);

        let tight = query(
            State(state),
            Json(LlmQueryRequest {
                query: "a api id greet".into(),
                token_budget: 1,
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        let (status, tight) = response_json(tight).await;
        assert_eq!(status, StatusCode::OK, "{tight}");
        assert_web_surface(&tight);
        let tight_nodes = tight["context"]["seeds"].as_array().unwrap().len()
            + tight["context"]["connected"].as_array().unwrap().len();
        assert!(
            tight_nodes < wide_nodes,
            "token_budget must drop nodes: wide={wide_nodes} tight={tight_nodes} {tight}"
        );
        assert_eq!(tight["truncated"], true, "{tight}");
        assert_eq!(tight["truncated_by"], "token_budget", "{tight}");
        assert!(tight["tokens_used"].as_u64().unwrap() <= 1, "{tight}");
        assert_eq!(tight["token_budget"], 1);

        let refused = query(
            State(fixture_state()),
            Json(LlmQueryRequest {
                query: "a b c".into(),
                token_budget: 4000,
            }),
        )
        .await
        .expect_err("single-character queries have no identifier seeds");
        let (status, body) = response_json(refused.into_response()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains("single-character"), "{body}");
        assert!(!error.contains("longer than 3"), "{body}");

        let mut real_failure = BrainContextResult {
            semantic_applied: false,
            semantic_unavailable: Some(json!({
                "component": "semantic",
                "reason": "query_inference_failed",
                "stage": "query_inference",
                "remediation": "verify the configured embedding model and vector artifacts, then retry or rebuild embeddings"
            })),
            degraded_components: vec!["semantic".into()],
            ..Default::default()
        };
        let preserved = real_failure.semantic_unavailable.clone();
        disclose_web_semantic_surface(&mut real_failure);
        assert_eq!(real_failure.semantic_unavailable, preserved);

        let mut missing_model = BrainContextResult {
            semantic_unavailable: Some(json!({
                "reason": "model_unavailable",
                "stage": "model_load",
                "remediation": "verify the configured embedding model and vector artifacts, then retry or rebuild embeddings"
            })),
            ..Default::default()
        };
        disclose_web_semantic_surface(&mut missing_model);
        assert_eq!(
            missing_model.semantic_unavailable.as_ref().unwrap()["reason"],
            "web_surface_unsupported"
        );
        assert!(
            !missing_model.semantic_unavailable.as_ref().unwrap()["remediation"]
                .as_str()
                .unwrap()
                .contains("rebuild")
        );

        let unresolved = query(
            State(fixture_state()),
            Json(LlmQueryRequest {
                query: "missingidentifier".into(),
                token_budget: 4000,
            }),
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        let (status, unresolved) = response_json(unresolved).await;
        assert_eq!(status, StatusCode::OK, "{unresolved}");
        assert_web_surface(&unresolved);
        assert!(
            unresolved["explanation"]
                .as_str()
                .unwrap()
                .contains("no matching symbols found"),
            "{unresolved}"
        );

        let expired_state = fixture_state();
        let expired = expired_state
            .store
            .with_read_deadline(std::time::Instant::now(), || {
                query(
                    State(expired_state.clone()),
                    Json(LlmQueryRequest {
                        query: "greet".into(),
                        token_budget: 4000,
                    }),
                )
                .now_or_never()
                .expect("engine errors are classified before the handler awaits")
            });
        let expired = expired.expect_err("a cancellation error stays an error");
        let (status, expired) = response_json(expired.into_response()).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{expired}");
        assert_eq!(expired["error"], "context_timeout", "{expired}");
        assert!(
            !expired.to_string().contains("web_surface_unsupported"),
            "{expired}"
        );

        let identity = map_context_engine_error(anyhow::Error::new(
            nestweaver_store::StoreError::EmbeddingIdentityUnreadable {
                detail: "model hash mismatch".into(),
            },
        ));
        assert_eq!(identity.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            identity.message.contains("embedding identity"),
            "{}",
            identity.message
        );
        assert!(!identity.message.contains("web_surface_unsupported"));

        let publication = map_context_engine_error(anyhow::Error::new(
            nestweaver_store::StoreError::RankingUnavailable,
        ));
        assert_eq!(publication.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            publication.message.contains("index publication"),
            "{}",
            publication.message
        );
        assert!(!publication.message.contains("web_surface_unsupported"));

        let seed = budget_node("sym:seed", &"S".repeat(40));
        let neighbor = budget_node("sym:later", "n");
        let seed_cost = node_token_cost(&seed);
        let neighbor_cost = node_token_cost(&neighbor);
        assert!(
            seed_cost > neighbor_cost,
            "the pinned copy must be the node that does not fit"
        );
        let mut budgeted = BrainContextResult {
            seeds: vec![seed.clone()],
            connected: vec![seed.clone(), neighbor.clone()],
            ..Default::default()
        };
        let budget = seed_cost + neighbor_cost;
        let (used, truncated) = apply_token_budget(&mut budgeted, budget);
        assert_eq!(budgeted.seeds.len(), 1, "seeds stay charged");
        assert_eq!(
            budgeted
                .connected
                .iter()
                .map(|node| node.uid.as_str())
                .collect::<Vec<_>>(),
            vec![neighbor.uid.as_str()],
            "the pinned seed copy must not crowd out the later neighbor"
        );
        assert_eq!(used, budget);
        assert!(
            !truncated,
            "a budget that fits the seed and the later neighbor is not truncated"
        );
    }

    fn budget_node(uid: &str, title: &str) -> BrainNode {
        BrainNode {
            uid: uid.into(),
            kind: "function".into(),
            title: title.into(),
            location: "src/a.rs:1".into(),
            relevance: 1.0,
            inline_body: None,
            body_complete: true,
        }
    }
}

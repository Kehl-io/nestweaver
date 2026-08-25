//! JSON-RPC dispatch helpers — serialize a tool call into the daemon's
//! gRPC surface (`JsonRequest`/`JsonResponse` pass-through plus the handful
//! of typed RPCs) against ANY NestWeaver endpoint, local or upstream.

use anyhow::{Context, Result};
use serde_json::Value;
use tonic::transport::Channel;

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;
use nestweaver_proto::{JsonRequest, JsonResponse};

/// Dispatch a tool call to the gRPC daemon via `JsonRequest`/`JsonResponse`.
///
/// Most NestWeaver tools use the `JsonRequest { args_json }` /
/// `JsonResponse { result_json }` pass-through pattern. This function
/// serializes the params, calls the matching RPC, and deserializes the
/// response.
pub async fn dispatch_json_rpc(
    client: &mut NestWeaverDaemonClient<Channel>,
    tool_name: &str,
    params: &Value,
) -> Result<Value> {
    dispatch_json_rpc_authed(client, tool_name, params, None).await
}

/// Like `dispatch_json_rpc` but optionally injects a bearer token into the
/// request metadata (required for authenticated upstream servers).
pub async fn dispatch_json_rpc_authed(
    client: &mut NestWeaverDaemonClient<Channel>,
    tool_name: &str,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    // ── Typed RPCs (not JsonRequest/JsonResponse) ─────────────────────
    //
    // These five tools use typed proto requests. Handle them first so
    // we don't build an unnecessary JsonRequest.
    match tool_name {
        "brain_search" => {
            return dispatch_typed_brain_search(client, params, auth_token).await;
        }
        "brain_context" => {
            return dispatch_typed_brain_context(client, params, auth_token).await;
        }
        "project_context" => {
            return dispatch_typed_project_context(client, params, auth_token).await;
        }
        "note_get" => {
            return dispatch_typed_note_get(client, params, auth_token).await;
        }
        "hub_nodes" => {
            return dispatch_typed_hub_nodes(client, params, auth_token).await;
        }
        _ => {} // fall through to JsonRequest dispatch
    }

    // ── JsonRequest/JsonResponse pass-through RPCs ────────────────────
    let args_json = serde_json::to_string(params)?;
    let mut request = tonic::Request::new(JsonRequest { args_json });

    if let Some(token) = auth_token
        && let Ok(val) = format!("Bearer {}", token).parse::<tonic::metadata::MetadataValue<_>>()
    {
        request.metadata_mut().insert("authorization", val);
    }

    let response: JsonResponse = match tool_name {
        "backlinks" | "get_backlinks" => client.get_backlinks(request).await,
        "flow_trace" => client.flow_trace(request).await,
        "blast_radius" => client.blast_radius(request).await,
        "brain_impact" | "impact" => client.impact(request).await,
        "brain_guide" => client.brain_guide(request).await,
        "brain_diff" => client.brain_diff(request).await,
        "read_symbols" => client.read_symbols(request).await,
        "regex_search" => client.regex_search(request).await,
        "count_patterns" => client.count_patterns(request).await,
        "cross_repo_contracts" => client.cross_repo_contracts(request).await,
        "contract_drift" => client.contract_drift(request).await,
        "code_context" => client.code_context(request).await,
        "dead_code" => client.dead_code(request).await,
        "brain_broken_links" => client.brain_broken_links(request).await,
        "brain_orphan_documents" => client.brain_orphan_documents(request).await,
        "brain_topic_clusters" => client.brain_topic_clusters(request).await,
        "brain_tag_graph" => client.brain_tag_graph(request).await,
        "brain_doc_stats" => client.brain_doc_stats(request).await,
        "brain_memory_lint" => client.brain_memory_lint(request).await,
        "brain_memory_consolidate" => client.brain_memory_consolidate(request).await,
        "brain_memory_related" => client.brain_memory_related(request).await,
        "detect_changes" => client.detect_changes(request).await,
        "affected_tests" => client.affected_tests(request).await,
        "clusters" => client.clusters(request).await,
        "stale_check" => client.stale_check(request).await,
        "bridge_nodes" => client.bridge_nodes(request).await,
        "get_summary" => client.get_summary(request).await,
        "investigate" => client.investigate(request).await,
        "investigate_expand" => client.investigate_expand(request).await,
        "investigate_hydrate" => client.investigate_hydrate(request).await,
        "set_extension" => client.set_extension(request).await,
        "query_extensions" => client.query_extensions(request).await,
        "brain_status" | "brain_status_json" => client.brain_status_json(request).await,
        "export_graph" => client.export_graph(request).await,
        "search_symbols" => client.search_symbols(request).await,
        "symbol_lookup" => client.symbol_lookup(request).await,
        // Admin / listing RPCs (local-only in routing matrix, but must be
        // dispatchable so HybridClient::query can route them).
        "list_repos" => client.list_repos_json(request).await,
        "list_vaults" => client.list_vaults_json(request).await,
        "embedding_dimension" => client.embedding_dimension(request).await,
        "list_services" => client.list_services_json(request).await,
        "service_summary" => client.service_summary_json(request).await,
        "list_projects" => client.list_projects_json(request).await,
        "repo_map" => client.repo_map_json(request).await,
        "suggest_links" => client.suggest_links_json(request).await,
        "detect_implicit_projects" => client.detect_implicit_projects_json(request).await,
        "pr_impact" => client.pr_impact_json(request).await,
        _ => {
            anyhow::bail!("unsupported tool for JSON dispatch: {tool_name}");
        }
    }
    .with_context(|| format!("{tool_name} RPC failed"))?
    .into_inner();

    let parsed: Value =
        serde_json::from_str(&response.result_json).unwrap_or(Value::String(response.result_json));
    Ok(parsed)
}

/// Inject an optional bearer token into a tonic request.
fn inject_bearer_token<T>(request: &mut tonic::Request<T>, auth_token: Option<&str>) {
    if let Some(token) = auth_token
        && let Ok(val) = format!("Bearer {}", token).parse::<tonic::metadata::MetadataValue<_>>()
    {
        request.metadata_mut().insert("authorization", val);
    }
}

fn brain_search_response_to_json(
    response: &nestweaver_proto::BrainSearchResponse,
    concise: bool,
) -> Value {
    let results: Vec<Value> = response
        .results
        .iter()
        .map(|result| {
            let mut item = serde_json::json!({
                "uid": result.uid,
                "kind": result.kind,
                "title": result.title,
            });
            if !concise {
                item["score"] = Value::from(result.score);
            }
            if let Some(location) = &result.location {
                item["location"] = Value::String(location.clone());
            }
            // Parity with the local path: symbol rows carry no
            // `matched_headings` key at all — omit it when empty instead of
            // emitting a spurious `[]`.
            if !result.matched_headings.is_empty() {
                item["matched_headings"] = serde_json::json!(result.matched_headings);
            }
            if !concise && let Some(body) = &result.inline_body {
                item["inline_body"] = Value::String(body.clone());
            }
            if let Some(canonical_id) = &result.canonical_id {
                item["canonical_id"] = Value::String(canonical_id.clone());
            }
            // Parity: note rows carry their vault.
            if let Some(vault_uid) = &result.vault_uid {
                item["vault_uid"] = Value::String(vault_uid.clone());
            }
            item
        })
        .collect();
    let returned_matches = if response.returned_matches == 0 && !results.is_empty() {
        results.len() as i32
    } else {
        response.returned_matches
    };
    let relation = if response.total_matches_relation.is_empty() {
        "gte"
    } else {
        &response.total_matches_relation
    };
    let truncated =
        response.truncated || relation != "eq" || returned_matches < response.total_matches;
    serde_json::json!({
        "query": response.query,
        "engine": response.engine,
        "total_matches": response.total_matches,
        "total_matches_relation": relation,
        "returned_matches": returned_matches,
        "truncated": truncated,
        "results": results,
        "expansion_terms": response.expansion_terms,
        "semantic_applied": response.semantic_applied,
        "degraded_components": response.degraded_components,
    })
}

/// Typed dispatch for `brain_search` -> `Search` RPC.
async fn dispatch_typed_brain_search(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::BrainSearchRequest {
        query: params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        limit: params.get("limit").and_then(|v| v.as_i64()).unwrap_or(20) as i32,
        response_format: params
            .get("response_format")
            .and_then(|v| v.as_str())
            .map(String::from),
        include_bodies: params
            .get("include_bodies")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        prf: params.get("prf").and_then(|v| v.as_bool()).unwrap_or(false),
        rerank: params
            .get("rerank")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        root: params
            .get("root")
            .and_then(|v| v.as_str())
            .map(String::from),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .search(request)
        .await
        .context("brain_search RPC failed")?
        .into_inner();
    let concise = params.get("response_format").and_then(Value::as_str) == Some("concise");
    Ok(brain_search_response_to_json(&resp, concise))
}

/// Typed dispatch for `brain_context` -> `GetContext` RPC.
/// Response is `BrainContextResponse { result_json }`.
async fn dispatch_typed_brain_context(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let seeds = params
        .get("seeds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let req = nestweaver_proto::BrainContextRequest {
        seeds,
        token_budget: params
            .get("token_budget")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        response_format: params
            .get("response_format")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        repos: json_str_array(params, "repos"),
        vaults: json_str_array(params, "vaults"),
        kinds: json_str_array(params, "kinds"),
        path_prefix: params
            .get("path_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        tags: json_str_array(params, "tags"),
        exclude_tags: json_str_array(params, "exclude_tags"),
        weight_ppr: params
            .get("weight_ppr")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        weight_bm25: params
            .get("weight_bm25")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        intent: params
            .get("intent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        include_seeds: params
            .get("include_seeds")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        include_bodies: params
            .get("include_bodies")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        root: params
            .get("root")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        prf: params.get("prf").and_then(|v| v.as_bool()).unwrap_or(false),
        rerank: params
            .get("rerank")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        weight_semantic: params
            .get("weight_semantic")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        since: params
            .get("since")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        recency_weight: params
            .get("recency_weight")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        recency_half_life_days: params
            .get("recency_half_life_days")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .get_context(request)
        .await
        .context("brain_context RPC failed")?
        .into_inner();
    let mut parsed: Value =
        serde_json::from_str(&resp.result_json).unwrap_or(Value::String(resp.result_json));
    if let Some(object) = parsed.as_object_mut() {
        object.insert(
            "semantic_applied".to_string(),
            Value::Bool(resp.semantic_applied),
        );
        object.insert(
            "degraded_components".to_string(),
            serde_json::json!(resp.degraded_components),
        );
    }
    Ok(parsed)
}

/// Typed dispatch for `project_context` -> `GetProjectContext` RPC.
/// Response is `ProjectContextResponse { result_json }`.
async fn dispatch_typed_project_context(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::ProjectContextRequest {
        project: params
            .get("project")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        token_budget: params
            .get("token_budget")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        kinds: json_str_array(params, "kinds"),
        include_components: bool_or(params, "include_components", true),
        intent: params
            .get("intent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        include_seeds: params
            .get("include_seeds")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        since: params
            .get("since")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        recency_weight: params
            .get("recency_weight")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        recency_half_life_days: params
            .get("recency_half_life_days")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        response_format: params
            .get("response_format")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        repos: json_str_array(params, "repos"),
        path_prefix: params
            .get("path_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        tags: json_str_array(params, "tags"),
        exclude_tags: json_str_array(params, "exclude_tags"),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .get_project_context(request)
        .await
        .context("project_context RPC failed")?
        .into_inner();
    let parsed: Value =
        serde_json::from_str(&resp.result_json).unwrap_or(Value::String(resp.result_json));
    Ok(parsed)
}

/// Typed dispatch for `note_get` -> `GetNote` RPC.
async fn dispatch_typed_note_get(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::NoteGetRequest {
        uid: params.get("uid").and_then(|v| v.as_str()).map(String::from),
        title: params
            .get("title")
            .and_then(|v| v.as_str())
            .map(String::from),
        include_body: bool_or(params, "include_body", true),
        sections: json_str_array(params, "sections"),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .get_note(request)
        .await
        .context("note_get RPC failed")?
        .into_inner();
    let mut result = serde_json::json!({
        "uid": resp.uid,
        "title": resp.title,
        "path": resp.path,
        "note_kind": resp.note_kind,
        "word_count": resp.word_count,
        "section_count": resp.section_count,
        // Match the daemon-proxy note_get shape (tools.rs): frontmatter and
        // outline are always present (local defaults to {} / []).
        "frontmatter": serde_json::from_str::<Value>(&resp.frontmatter_json)
            .unwrap_or_else(|_| serde_json::json!({})),
        "outline": resp
            .outline
            .iter()
            .map(|h| {
                serde_json::json!({
                    "uid": h.uid,
                    "level": h.level,
                    "text": h.text,
                    "slug": h.slug,
                    "line": h.line,
                })
            })
            .collect::<Vec<_>>(),
    });
    if let Some(body) = resp.body {
        result["body"] = Value::String(body);
    }
    Ok(result)
}

/// Typed dispatch for `hub_nodes` -> `HubNodes` RPC.
/// Response is `HubNodesResponse { result_json }`.
async fn dispatch_typed_hub_nodes(
    client: &mut NestWeaverDaemonClient<Channel>,
    params: &Value,
    auth_token: Option<&str>,
) -> Result<Value> {
    let req = nestweaver_proto::HubNodesRequest {
        // The MCP schema advertises 'limit'; 'top_n' kept as a backward-compat
        // alias (and it is the proto field name).
        top_n: params
            .get("limit")
            .or_else(|| params.get("top_n"))
            .and_then(|v| v.as_i64())
            .unwrap_or(10) as i32,
        response_format: params
            .get("response_format")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    };
    let mut request = tonic::Request::new(req);
    inject_bearer_token(&mut request, auth_token);
    let resp = client
        .hub_nodes(request)
        .await
        .context("hub_nodes RPC failed")?
        .into_inner();
    let parsed: Value =
        serde_json::from_str(&resp.result_json).unwrap_or(Value::String(resp.result_json));
    Ok(parsed)
}

/// Helper: extract a `Vec<String>` from a JSON array field.
fn json_str_array(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Presence-aware bool extraction: proto3 scalar bools carry no
/// presence, so an arg the caller left unset would forward as explicit
/// `false`, and the daemon's typed handlers write that `false` back into the
/// tool args — overriding tool defaults that are TRUE
/// (`project_context.include_components`, `note_get.include_body`). Forward
/// the tool's own default when the caller did not specify the flag; an
/// explicit `false` is still honored.
fn bool_or(params: &Value, key: &str, default: bool) -> bool {
    params.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_brain_search_json_preserves_counts_and_old_response_defaults() {
        let response = nestweaver_proto::BrainSearchResponse {
            query: "needle".to_string(),
            engine: "bm25".to_string(),
            total_matches: 1,
            results: vec![nestweaver_proto::SearchResultItem {
                uid: "sym:needle".to_string(),
                canonical_id: Some("canonical-needle".to_string()),
                kind: "Symbol/Function".to_string(),
                title: "needle".to_string(),
                score: 1.0,
                location: Some("src/lib.rs:1".to_string()),
                matched_headings: vec!["Needle heading".to_string()],
                inline_body: Some("detailed body".to_string()),
                vault_uid: None,
            }],
            expansion_terms: vec!["expanded".to_string()],
            returned_matches: 0,
            total_matches_relation: String::new(),
            // Proto3 defaults from a pre-Task-7 daemon: the new scalar fields
            // decode as zero/empty/false because they were absent on the wire.
            truncated: false,
            semantic_applied: false,
            degraded_components: Vec::new(),
        };

        let value = brain_search_response_to_json(&response, false);

        assert_eq!(value["total_matches"], 1);
        assert_eq!(value["total_matches_relation"], "gte");
        assert_eq!(value["returned_matches"], 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["results"][0]["canonical_id"], "canonical-needle");
        assert_eq!(value["expansion_terms"], serde_json::json!(["expanded"]));

        let concise = brain_search_response_to_json(&response, true);
        assert_eq!(concise["results"][0]["uid"], "sym:needle");
        assert_eq!(concise["results"][0]["canonical_id"], "canonical-needle");
        assert!(
            concise["results"][0].get("score").is_none(),
            "typed federation concise rows must stay score-free: {concise}"
        );
        assert_eq!(
            concise["results"][0]["matched_headings"],
            serde_json::json!(["Needle heading"]),
            "typed federation concise note rows must retain matched headings: {concise}"
        );
        assert!(
            concise["results"][0].get("inline_body").is_none(),
            "typed federation concise rows must omit inline bodies: {concise}"
        );

        let mut title_only_note = response;
        title_only_note.results[0].uid = "note:needle".to_string();
        title_only_note.results[0].kind = "note".to_string();
        title_only_note.results[0].canonical_id = None;
        title_only_note.results[0].matched_headings.clear();
        let concise_note = brain_search_response_to_json(&title_only_note, true);
        assert!(
            concise_note["results"][0].get("matched_headings").is_none(),
            "rows with no matched headings must omit the field (parity with the \
             daemon-proxy mapper): {concise_note}"
        );
    }

    #[test]
    fn bool_or_forwards_default_true_when_arg_absent() {
        // Absent include_components/include_body must NOT collapse to
        // false (proto3 has no presence); the tool default is true.
        let empty = serde_json::json!({});
        assert!(bool_or(&empty, "include_components", true));
        assert!(bool_or(&empty, "include_body", true));

        // Explicit values are honored in both directions.
        let explicit_false = serde_json::json!({ "include_components": false });
        assert!(!bool_or(&explicit_false, "include_components", true));
        let explicit_true = serde_json::json!({ "include_body": true });
        assert!(bool_or(&explicit_true, "include_body", true));

        // Default-false bools keep their old behavior.
        assert!(!bool_or(&empty, "prf", false));
    }
}

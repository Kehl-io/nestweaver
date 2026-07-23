//! Brain tool implementations.
//!
//! Each public `tool_*` function takes the parsed JSON arguments and the
//! shared `GraphStore`, and returns either a structured `serde_json::Value`
//! (returned to MCP clients inside `tools/call` results) or an error.
//!
//! Tool descriptions are written in the "when to use" style — Claude reads
//! these to pick the right tool. Lead with the trigger, not the mechanism.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, anyhow};
use nestweaver_engine::config::DEFAULT_RESULT_LIMIT;
use nestweaver_engine::query::search_symbols_page;
use nestweaver_engine::{
    BlastRadiusOptions, BrainContextResult, DeadCodeConfidence, EmbedQueryFn, HybridSearchConfig,
    SummaryLevel, ToolDocEntry, analyze_blast_radius, attach_cluster_ids, attach_communities,
    broken_links, build_brain_context_hybrid_with_aliases, compute_clusters, detect_changes_impact,
    detect_dead_code_cancellable, doc_stats, expand_query_with_aliases, filter_by_target,
    find_bridge_nodes, find_hub_nodes, generate_agents_md_with_rules,
    generate_claude_md_with_rules, generate_cursor_rule_with_rules, generate_guide_with_tools,
    generate_skill_with_tools, generate_summaries, get_all_properties, get_last_indexed_at,
    investigate, investigate_expand, investigate_hydrate, load_alias_sidecar, load_clusters,
    load_extensions, memory_consolidate, memory_lint, memory_related, orphan_documents,
    parse_iso8601_to_epoch, populate_inline_bodies, query_by_property, render_text, tag_graph,
    tag_graph_all, topic_clusters, truncate_to_budget,
};
use nestweaver_schema::SymbolKind;
use nestweaver_store::tantivy_index::{SearchTotal, SearchTotalRelation};
use nestweaver_store::{GraphStore, SearchLogicalIdentity, TantivyIndex};
use serde_json::{Value, json};
// In non-daemon builds, brain_add_source and set_extension write directly using
// these primitives; in daemon builds those writes route through the daemon.
#[cfg(not(feature = "daemon"))]
use nestweaver_engine::{index_directory, index_markdown_directory, save_extensions, set_property};

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Resolve a symbol name to a UID. When multiple symbols share the name,
/// pick the most likely canonical definition using a composite heuristic:
/// PageRank score (if computed), then source-path priority (src/ over tests/),
/// then lowest start line (definitions before re-imports).
fn resolve_symbol_uid(store: &GraphStore, name_or_uid: &str) -> Result<String, anyhow::Error> {
    if name_or_uid.contains(':') {
        return Ok(name_or_uid.to_string());
    }
    let matches = store
        .lookup_symbols_by_name(name_or_uid)
        .map_err(|e| anyhow!("lookup_symbols_by_name: {e}"))?;
    match matches.len() {
        0 => Err(anyhow!("no symbol found: '{name_or_uid}'")),
        1 => Ok(matches.into_iter().next().unwrap().uid),
        _ => {
            let best = matches
                .into_iter()
                .max_by(|a, b| {
                    // Prefer symbols with PageRank scores (computed by hubs)
                    let pr_a = a.pagerank_score.unwrap_or(0.0);
                    let pr_b = b.pagerank_score.unwrap_or(0.0);
                    if (pr_a - pr_b).abs() > f64::EPSILON {
                        return pr_a.partial_cmp(&pr_b).unwrap_or(std::cmp::Ordering::Equal);
                    }
                    // Prefer src/ over tests/test/__tests__/migrations/
                    let non_src = |p: &str| {
                        let lp = p.to_lowercase();
                        lp.starts_with("test")
                            || lp.contains("__tests__")
                            || lp.starts_with("migrations")
                    };
                    let a_test = non_src(&a.file_path);
                    let b_test = non_src(&b.file_path);
                    if a_test != b_test {
                        return b_test.cmp(&a_test);
                    }
                    // Prefer lower start line (definition site)
                    b.start_line.cmp(&a.start_line)
                })
                .unwrap();
            Ok(best.uid)
        }
    }
}

/// Build a map from repo UID → display name for repo filter matching.
fn build_repo_name_map(store: &GraphStore) -> std::collections::HashMap<String, String> {
    store
        .list_repos(None)
        .unwrap_or_default()
        .iter()
        .map(|r| (r.uid.clone(), nestweaver_engine::repo_display_name(r)))
        .collect()
}

/// Build a map from vault UID → display name for vault filter matching.
fn build_vault_name_map(store: &GraphStore) -> std::collections::HashMap<String, String> {
    store
        .list_vaults(None)
        .unwrap_or_default()
        .iter()
        .map(|v| (v.uid.clone(), v.name.clone()))
        .collect()
}

/// Resolve authoritative symbol ownership only when repository scoping is
/// active. Restricted impact/test-selection responses fail closed if this
/// global ownership view cannot be loaded; emitting unowned rows would leak.
fn restricted_symbol_owners(
    store: &GraphStore,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Option<HashMap<String, String>>, anyhow::Error> {
    let Some(nestweaver_engine::authz::VisibleRepos::Only(_)) = visible else {
        return Ok(None);
    };
    let symbols = store
        .list_all_symbols()
        .context("loading symbol ownership for repository-scoped response")?;
    Ok(Some(
        symbols
            .into_iter()
            .map(|symbol| (symbol.uid, symbol.repo_uid))
            .collect(),
    ))
}

fn repo_is_visible(
    repo_uid: &str,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> bool {
    match visible {
        Some(nestweaver_engine::authz::VisibleRepos::Only(_)) => {
            !repo_uid.is_empty() && visible.is_some_and(|scope| scope.allows(repo_uid))
        }
        _ => true,
    }
}

// ── Tool catalogue ──────────────────────────────────────────────────────────

const LITE_TOOLS: &[&str] = &[
    "brain_context",
    "brain_search",
    "brain_impact",
    "brain_status",
    "brain_guide",
    "detect_changes",
];

/// Returns the `tools/list` payload — schemas + descriptions for every tool
/// the brain exposes. When `lite` is true only the 6 core tools are included.
/// When `--tools` was specified, only those named tools are included.
fn all_tool_schemas() -> Vec<Value> {
    vec![
        tool_schema_brain_context(),
        tool_schema_brain_search(),
        tool_schema_note_get(),
        tool_schema_backlinks(),
        tool_schema_brain_status(),
        tool_schema_brain_add_source(),
        tool_schema_brain_remove_source(),
        tool_schema_prune_stale(),
        tool_schema_cross_repo_contracts(),
        tool_schema_brain_impact(),
        tool_schema_brain_guide(),
        tool_schema_flow_trace(),
        tool_schema_detect_changes(),
        tool_schema_clusters(),
        tool_schema_stale_check(),
        tool_schema_set_extension(),
        tool_schema_query_extensions(),
        tool_schema_brain_diff(),
        tool_schema_project_context(),
        tool_schema_dead_code(),
        tool_schema_hub_nodes(),
        tool_schema_bridge_nodes(),
        tool_schema_blast_radius(),
        tool_schema_get_summary(),
        tool_schema_read_symbols(),
        tool_schema_regex_search(),
        tool_schema_count_patterns(),
        tool_schema_brain_broken_links(),
        tool_schema_brain_orphan_documents(),
        tool_schema_brain_topic_clusters(),
        tool_schema_brain_tag_graph(),
        tool_schema_brain_doc_stats(),
        tool_schema_affected_tests(),
        tool_schema_investigate(),
        tool_schema_investigate_expand(),
        tool_schema_investigate_hydrate(),
        tool_schema_contract_drift(),
        tool_schema_brain_memory_lint(),
        tool_schema_brain_memory_consolidate(),
        tool_schema_brain_memory_related(),
    ]
}

static TOOL_VALIDATORS: std::sync::OnceLock<
    std::collections::HashMap<String, jsonschema::Validator>,
> = std::sync::OnceLock::new();

fn tool_validators() -> &'static std::collections::HashMap<String, jsonschema::Validator> {
    TOOL_VALIDATORS.get_or_init(|| {
        all_tool_schemas()
            .into_iter()
            .map(|tool| {
                let name = tool["name"]
                    .as_str()
                    .expect("registered tool has a string name")
                    .to_string();
                let validator = jsonschema::options()
                    .with_draft(jsonschema::Draft::Draft202012)
                    .build(&tool["inputSchema"])
                    .unwrap_or_else(|error| panic!("invalid input schema for {name}: {error}"));
                (name, validator)
            })
            .collect()
    })
}

const MAX_VALIDATION_ITEM_BYTES: usize = 192;
const MAX_VALIDATION_ERROR_BYTES: usize = 1024;
const MAX_TOOL_NAME_IN_ERROR_BYTES: usize = 96;

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }

    const ELLIPSIS: &str = "…";
    let suffix = if max_bytes >= ELLIPSIS.len() {
        ELLIPSIS
    } else {
        ""
    };
    let mut end = max_bytes - suffix.len();
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }

    let mut bounded = String::with_capacity(max_bytes);
    bounded.push_str(&value[..end]);
    bounded.push_str(suffix);
    bounded
}

fn missing_alias_requirement(name: &str, args: &Value) -> Option<&'static str> {
    if !args.is_object() {
        return None;
    }

    let (first, second, message) = match name {
        "read_symbols" => (
            "targets",
            "uids_or_fqns",
            "missing required argument: expected 'targets' or 'uids_or_fqns'",
        ),
        "regex_search" => (
            "pattern",
            "query",
            "missing required argument: expected 'pattern' or 'query'",
        ),
        "detect_changes" => (
            "changed_files",
            "files",
            "missing required argument: expected 'changed_files' or 'files'",
        ),
        _ => return None,
    };

    (args.get(first).is_none() && args.get(second).is_none()).then_some(message)
}

fn render_validation_error(error: &jsonschema::ValidationError<'_>) -> String {
    let instance_path = error.instance_path().to_string();
    let instance_path = if instance_path.is_empty() {
        "/".to_string()
    } else {
        truncate_utf8_bytes(&instance_path, MAX_VALIDATION_ITEM_BYTES / 2)
    };
    truncate_utf8_bytes(
        &format!(
            "{instance_path}: schema keyword '{}' failed",
            error.kind().keyword()
        ),
        MAX_VALIDATION_ITEM_BYTES,
    )
}

pub fn validate_tool_arguments(name: &str, args: &Value) -> Result<(), anyhow::Error> {
    let Some(validator) = tool_validators().get(name) else {
        let name = truncate_utf8_bytes(name, MAX_TOOL_NAME_IN_ERROR_BYTES);
        let message =
            truncate_utf8_bytes(&format!("unknown tool: {name}"), MAX_VALIDATION_ERROR_BYTES);
        return Err(anyhow!(message));
    };

    let errors: Vec<String> = if let Some(message) = missing_alias_requirement(name, args) {
        vec![truncate_utf8_bytes(message, MAX_VALIDATION_ITEM_BYTES)]
    } else {
        validator
            .iter_errors(args)
            .take(3)
            .map(|error| render_validation_error(&error))
            .collect()
    };
    if errors.is_empty() {
        Ok(())
    } else {
        let name = truncate_utf8_bytes(name, MAX_TOOL_NAME_IN_ERROR_BYTES);
        let message = format!("invalid arguments for tool '{name}': {}", errors.join("; "));
        Err(anyhow!(truncate_utf8_bytes(
            &message,
            MAX_VALIDATION_ERROR_BYTES
        )))
    }
}

pub fn tool_list(lite: bool) -> Value {
    let mut tools = all_tool_schemas();
    if lite {
        tools.retain(|t| LITE_TOOLS.contains(&t["name"].as_str().unwrap_or("")));
    }
    // Apply explicit tool allowlist (--tools flag) when set.
    let allowed = ALLOWED_TOOLS.with(|c| c.borrow().clone());
    if let Some(ref names) = allowed {
        tools.retain(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| names.iter().any(|a| a == n))
        });
    }
    json!({ "tools": tools })
}

#[cfg(test)]
mod tool_schema_validation_tests {
    use super::*;
    use std::collections::BTreeSet;

    fn assert_valid(name: &str, args: Value) {
        validate_tool_arguments(name, &args)
            .unwrap_or_else(|error| panic!("expected valid arguments for {name}: {error}"));
    }

    fn assert_invalid(name: &str, args: Value) -> String {
        validate_tool_arguments(name, &args)
            .expect_err("arguments should fail schema validation")
            .to_string()
    }

    #[test]
    fn registry_contains_exactly_the_40_advertised_unique_names() {
        let expected: BTreeSet<&str> = [
            "affected_tests",
            "backlinks",
            "blast_radius",
            "brain_add_source",
            "brain_broken_links",
            "brain_context",
            "brain_diff",
            "brain_doc_stats",
            "brain_guide",
            "brain_impact",
            "brain_memory_consolidate",
            "brain_memory_lint",
            "brain_memory_related",
            "brain_orphan_documents",
            "brain_remove_source",
            "brain_search",
            "brain_status",
            "brain_tag_graph",
            "brain_topic_clusters",
            "bridge_nodes",
            "clusters",
            "contract_drift",
            "count_patterns",
            "cross_repo_contracts",
            "dead_code",
            "detect_changes",
            "flow_trace",
            "get_summary",
            "hub_nodes",
            "investigate",
            "investigate_expand",
            "investigate_hydrate",
            "note_get",
            "project_context",
            "prune_stale",
            "query_extensions",
            "read_symbols",
            "regex_search",
            "set_extension",
            "stale_check",
        ]
        .into_iter()
        .collect();

        let schemas = all_tool_schemas();
        assert_eq!(
            schemas.len(),
            40,
            "registry must contain exactly 40 schemas"
        );
        let names: BTreeSet<&str> = schemas
            .iter()
            .map(|schema| {
                schema["name"]
                    .as_str()
                    .expect("registered tool has a string name")
            })
            .collect();
        assert_eq!(names.len(), schemas.len(), "tool names must be unique");
        assert_eq!(names, expected, "registry must match the advertised tools");
    }

    #[test]
    fn every_registered_input_schema_compiles_as_draft_2020_12() {
        for tool in all_tool_schemas() {
            let name = tool["name"].as_str().unwrap();
            jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .build(&tool["inputSchema"])
                .unwrap_or_else(|error| panic!("invalid input schema for {name}: {error}"));
        }
    }

    #[test]
    fn required_arguments_reject_empty_objects() {
        for name in [
            "brain_search",
            "blast_radius",
            "read_symbols",
            "regex_search",
            "detect_changes",
        ] {
            assert!(
                assert_invalid(name, json!({})).contains("invalid arguments"),
                "{name} should reject an empty object"
            );
        }
    }

    #[test]
    fn validation_errors_include_actionable_instance_paths() {
        let error = assert_invalid("brain_search", json!({ "query": 7 }));
        assert!(
            error.contains("/query"),
            "error should identify /query: {error}"
        );
    }

    #[test]
    fn property_and_array_member_types_are_enforced() {
        assert_invalid("brain_search", json!({ "query": "x", "limit": "many" }));
        assert_invalid("brain_search", json!({ "query": "x", "include_bodies": 1 }));
        assert_invalid("read_symbols", json!({ "targets": ["sym:x", 7] }));
    }

    #[test]
    fn validation_reports_at_most_three_errors() {
        let error = assert_invalid(
            "brain_search",
            json!({
                "query": 7,
                "limit": "many",
                "include_bodies": 1,
                "prf": "yes"
            }),
        );
        assert_eq!(
            error.matches("; ").count(),
            2,
            "validation should report exactly three of the four errors: {error}"
        );
    }

    #[test]
    fn hostile_validation_values_produce_a_small_bounded_utf8_error() {
        let huge = "界".repeat(700_000);
        let error = assert_invalid(
            "brain_search",
            json!({
                "query": 7,
                "limit": huge,
                "include_bodies": "🔥".repeat(600_000),
                "prf": "é".repeat(1_000_000),
            }),
        );

        assert!(
            error.len() <= 1024,
            "validation errors must stay within 1024 UTF-8 bytes, got {}",
            error.len()
        );
        assert!(
            error.contains("/limit") || error.contains("/include_bodies"),
            "bounded error should retain an actionable instance path: {error}"
        );
    }

    #[test]
    fn oversized_unknown_tool_name_produces_a_small_bounded_utf8_error() {
        let name = "工具".repeat(500_000);
        let error = assert_invalid(&name, json!({}));

        assert!(
            error.len() <= 128,
            "unknown-tool errors must stay within 128 UTF-8 bytes, got {}",
            error.len()
        );
        assert!(error.starts_with("unknown tool: "));
    }

    #[test]
    fn missing_alias_pairs_name_every_accepted_field() {
        for (name, accepted) in [
            ("read_symbols", ["targets", "uids_or_fqns"]),
            ("regex_search", ["pattern", "query"]),
            ("detect_changes", ["changed_files", "files"]),
        ] {
            let error = assert_invalid(name, json!({}));
            for field in accepted {
                assert!(
                    error.contains(field),
                    "{name} missing-argument error must name '{field}': {error}"
                );
            }
        }
    }

    #[test]
    fn runtime_aliases_and_canonical_spellings_validate() {
        assert_valid("read_symbols", json!({ "uids_or_fqns": ["sym:x"] }));
        assert_valid("regex_search", json!({ "query": "fn\\s+x" }));
        assert_valid("detect_changes", json!({ "files": ["src/a.rs"] }));

        assert_valid("read_symbols", json!({ "targets": ["sym:x"] }));
        assert_valid("regex_search", json!({ "pattern": "fn\\s+x" }));
        assert_valid("detect_changes", json!({ "changed_files": ["src/a.rs"] }));

        assert_valid("hub_nodes", json!({ "top_n": 5 }));
        assert_valid("bridge_nodes", json!({ "top_n": 5 }));
        assert_valid("get_summary", json!({ "name": "x" }));
    }

    #[test]
    fn aliases_require_non_empty_string_arrays() {
        for (name, args) in [
            ("read_symbols", json!({ "uids_or_fqns": [] })),
            ("detect_changes", json!({ "files": [] })),
        ] {
            assert_invalid(name, args);
        }
    }

    #[test]
    fn unknown_tool_is_reported() {
        let error = assert_invalid("not_a_tool", json!({}));
        assert_eq!(error, "unknown tool: not_a_tool");
    }

    #[test]
    fn direct_dispatch_validates_arguments_and_preserves_aliases() {
        let store = GraphStore::in_memory().unwrap();

        let error = dispatch(&store, None, "brain_search", json!({ "query": 42 }), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid arguments"), "{error}");
        assert!(error.contains("/query"), "{error}");

        dispatch(
            &store,
            None,
            "brain_search",
            json!({ "query": "needle" }),
            None,
        )
        .expect("valid canonical arguments should reach the handler");
        dispatch(
            &store,
            None,
            "read_symbols",
            json!({ "uids_or_fqns": ["sym:missing"] }),
            None,
        )
        .expect("valid legacy aliases should reach the handler");
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn daemon_proxy_rejects_invalid_arguments_before_rpc() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let channel = {
            let _guard = runtime.enter();
            tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy()
        };
        let mut client = DaemonGrpcClient::new(channel);

        let error = dispatch_via_daemon(
            &mut client,
            &runtime,
            "brain_search",
            json!({ "query": 42 }),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("invalid arguments for tool"), "{error}");
        assert!(error.contains("/query"), "{error}");
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn daemon_brain_search_json_preserves_counts_and_old_response_defaults() {
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
                matched_headings: Vec::new(),
                inline_body: None,
            }],
            expansion_terms: vec!["expanded".to_string()],
            returned_matches: 0,
            total_matches_relation: String::new(),
            // Proto3 defaults from a pre-Task-7 daemon: the new scalar fields
            // decode as zero/empty/false because they were absent on the wire.
            truncated: false,
        };

        let value = daemon_brain_search_response_to_json(&response, false);

        assert_eq!(value["total_matches"], 1);
        assert_eq!(value["total_matches_relation"], "gte");
        assert_eq!(value["returned_matches"], 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["results"][0]["canonical_id"], "canonical-needle");
        assert_eq!(value["expansion_terms"], json!(["expanded"]));

        let concise = daemon_brain_search_response_to_json(&response, true);
        assert_eq!(concise["results"][0]["uid"], "sym:needle");
        assert_eq!(concise["results"][0]["canonical_id"], "canonical-needle");
    }
}

/// Returns structured documentation metadata for every registered tool.
///
/// Each entry is `(name, category, purpose, key_params)`. The category mapping
/// is maintained here alongside the tool schemas so it stays in sync with
/// `tool_list()`. The binary crate bridges these entries into the engine's
/// `ToolDocEntry` for dynamic guide generation.
pub fn tool_doc_entries() -> Vec<(String, String, String, Vec<String>)> {
    let categories: &[(&str, &str)] = &[
        ("brain_context", "Core retrieval"),
        ("brain_search", "Core retrieval"),
        ("note_get", "Core retrieval"),
        ("backlinks", "Core retrieval"),
        ("project_context", "Core retrieval"),
        ("brain_impact", "Analysis"),
        ("flow_trace", "Analysis"),
        ("detect_changes", "Analysis"),
        ("cross_repo_contracts", "Analysis"),
        ("clusters", "Analysis"),
        ("dead_code", "Analysis"),
        ("hub_nodes", "Analysis"),
        ("bridge_nodes", "Analysis"),
        ("blast_radius", "Analysis"),
        ("affected_tests", "Analysis"),
        ("contract_drift", "Analysis"),
        ("investigate", "Investigation"),
        ("investigate_expand", "Investigation"),
        ("investigate_hydrate", "Investigation"),
        ("brain_status", "Status & maintenance"),
        ("stale_check", "Status & maintenance"),
        ("brain_diff", "Status & maintenance"),
        ("brain_guide", "Status & maintenance"),
        ("brain_add_source", "Status & maintenance"),
        ("brain_remove_source", "Status & maintenance"),
        ("prune_stale", "Status & maintenance"),
        ("get_summary", "Status & maintenance"),
        ("read_symbols", "Code search"),
        ("regex_search", "Code search"),
        ("count_patterns", "Code search"),
        ("set_extension", "Extensions"),
        ("query_extensions", "Extensions"),
        ("brain_broken_links", "Vault health"),
        ("brain_orphan_documents", "Vault health"),
        ("brain_topic_clusters", "Vault health"),
        ("brain_tag_graph", "Vault health"),
        ("brain_doc_stats", "Vault health"),
        ("brain_memory_lint", "Memory"),
        ("brain_memory_consolidate", "Memory"),
        ("brain_memory_related", "Memory"),
    ];

    let cat_map: std::collections::HashMap<&str, &str> = categories.iter().copied().collect();

    let tools_json = tool_list(false);
    let tools = tools_json["tools"].as_array().unwrap();

    tools
        .iter()
        .map(|t| {
            let name = t["name"].as_str().unwrap().to_string();
            let desc = t["description"].as_str().unwrap_or("");
            // Take just the first sentence/line as purpose
            let purpose = desc.split('\n').next().unwrap_or(desc).to_string();
            let category = cat_map.get(name.as_str()).unwrap_or(&"Other").to_string();
            let key_params: Vec<String> = t["inputSchema"]["required"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            (name, category, purpose, key_params)
        })
        .collect()
}

/// Dispatch a `tools/call` to the named tool. The optional `tantivy`
/// index, when present, drives hybrid retrieval in `brain_context` and
/// upgrades `brain_search` from substring to BM25.
///
/// When `--tools` was specified, calls to tools outside the allowlist
/// are rejected with a descriptive error.
pub fn dispatch(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
    embed_model: Option<&dyn EmbedQueryFn>,
) -> Result<Value, anyhow::Error> {
    dispatch_cancellable(store, tantivy, name, args, embed_model, None, None)
}

/// Like [`dispatch`], but threads a cooperative cancellation flag into the
/// heavy traversals (e.g. `brain_context`/`project_context` vector fan-out) so a
/// query timeout or client disconnect can stop the work. `cancel = None` is the
/// original behavior.
///
/// `visible` carries the caller's per-repo visibility (R9/R9b), resolved by the
/// HTTP boundary from the bearer identity. `None` (and `Some(VisibleRepos::All)`)
/// means no scoping — the backward-compatible single-trust-domain default, in
/// which repo-scoped authorization is a no-op. `brain_search`, `brain_impact`,
/// `blast_radius`, and `affected_tests` enforce it; tools whose data is not
/// repo-scoped ignore it.
pub fn dispatch_cancellable(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
    embed_model: Option<&dyn EmbedQueryFn>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    // Enforce tool allowlist when configured.
    let allowed = ALLOWED_TOOLS.with(|c| c.borrow().clone());
    if let Some(ref names) = allowed
        && !names.iter().any(|a| a == name)
    {
        return Err(anyhow!(
            "tool '{name}' is not in the allowed tools list; allowed: {}",
            names.join(", ")
        ));
    }

    validate_tool_arguments(name, &args)?;

    // F16: serve cacheable read tools from (or populate) the response cache.
    // Correctness rests on the cache KEY — see `maybe_cached`.
    if is_cacheable_tool(name) && !cache_bypassed(&args) {
        return maybe_cached(store, tantivy, name, args, embed_model, cancel, visible);
    }

    dispatch_uncached(store, tantivy, name, args, embed_model, cancel, visible)
}

/// The actual tool dispatch table, after cache handling.
fn dispatch_uncached(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
    embed_model: Option<&dyn EmbedQueryFn>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    match name {
        "brain_context" => tool_brain_context(store, tantivy, args, embed_model, cancel),
        "brain_search" => tool_brain_search(store, tantivy, args, visible),
        "note_get" => tool_note_get(store, args),
        "backlinks" => tool_backlinks(store, args),
        "brain_status" => tool_brain_status(store, tantivy),
        "brain_add_source" => tool_brain_add_source(store, args),
        "brain_remove_source" => tool_brain_remove_source(store, args),
        "prune_stale" => tool_prune_stale(store),
        "cross_repo_contracts" => tool_cross_repo_contracts(store, args),
        "brain_impact" => tool_brain_impact(store, args, cancel, visible),
        "brain_guide" => tool_brain_guide(store, args),
        "flow_trace" => tool_flow_trace(store, args, cancel),
        "detect_changes" => tool_detect_changes(store, args),
        "clusters" => tool_clusters(store, args),
        "stale_check" => tool_stale_check(store),
        "set_extension" => tool_set_extension(args),
        "query_extensions" => tool_query_extensions(args),
        "brain_diff" => tool_brain_diff(store, args),
        "project_context" => tool_project_context(store, tantivy, args, embed_model, cancel),
        "dead_code" => tool_dead_code(store, args, cancel),
        "hub_nodes" => tool_hub_nodes(store, args),
        "bridge_nodes" => tool_bridge_nodes(store, args),
        "blast_radius" => tool_blast_radius(store, args, cancel, visible),
        "get_summary" => tool_get_summary(store, args),
        "read_symbols" => tool_read_symbols(store, args),
        "regex_search" => tool_regex_search(store, args),
        "count_patterns" => tool_count_patterns(store, args),
        "brain_broken_links" => tool_brain_broken_links(store, args),
        "brain_orphan_documents" => tool_brain_orphan_documents(store, args),
        "brain_topic_clusters" => tool_brain_topic_clusters(store, args),
        "brain_tag_graph" => tool_brain_tag_graph(store, args),
        "brain_doc_stats" => tool_brain_doc_stats(store, args),
        "affected_tests" => tool_affected_tests(store, args, visible),
        "investigate" => tool_investigate(store, tantivy, args, embed_model),
        "investigate_expand" => tool_investigate_expand(store, args),
        "investigate_hydrate" => tool_investigate_hydrate(store, args),
        "contract_drift" => tool_contract_drift(store, args),
        "brain_memory_lint" => tool_brain_memory_lint(store, args),
        "brain_memory_consolidate" => tool_brain_memory_consolidate(store, args),
        "brain_memory_related" => tool_brain_memory_related(store, args),
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

// ── F16: response cache ──────────────────────────────────────────────────────

/// Deterministic READ tools whose responses are safe to cache. A tool qualifies
/// only if, given the same graph (same generation + scope digest) and the same
/// normalized args, it returns the same response.
///
/// Deliberately EXCLUDED:
/// - write/mutation tools (`brain_add_source`, `set_extension`) — they change
///   state, so caching them would be wrong;
/// - stateful bundle tools (`investigate`, `investigate_expand`,
///   `investigate_hydrate`) — they accumulate per-session state;
/// - `brain_status` / `stale_check` — they report live process/lock state and
///   the cache's own stats, which must not be frozen;
/// - `read_symbols` — its body text is read from the FILESYSTEM (resolved from
///   cwd/`root`, or empty under a bare-clone/server), so the response is NOT a
///   pure function of the graph generation + scope digest that keys this cache.
///   A single call made from a wrong cwd (or a bare clone) would cache an EMPTY
///   body and then serve it for the correct args forever — a silent-wrong result
///   on a core retrieval tool. It's a cheap disk-span read, so leave it uncached.
/// - `query_extensions` — reads the extensions sidecar, which `set_extension`
///   mutates WITHOUT bumping the graph generation, so a cached result would serve
///   stale values after a write (nw-089). Cheap sidecar read; leave it uncached.
const CACHEABLE_TOOLS: &[&str] = &[
    "brain_context",
    "brain_search",
    "note_get",
    "backlinks",
    "cross_repo_contracts",
    "brain_impact",
    "flow_trace",
    "clusters",
    "brain_diff",
    "project_context",
    "dead_code",
    "hub_nodes",
    "bridge_nodes",
    "blast_radius",
    "get_summary",
    "regex_search",
    "count_patterns",
    "brain_broken_links",
    "brain_orphan_documents",
    "brain_topic_clusters",
    "brain_tag_graph",
    "brain_doc_stats",
    "affected_tests",
    "contract_drift",
    "brain_memory_related",
];

/// True when `tool` is a deterministic read tool eligible for response caching.
fn is_cacheable_tool(tool: &str) -> bool {
    CACHEABLE_TOOLS.contains(&tool)
}

/// True when the caller asked to skip the cache: MCP arg `cache: "bypass"` or
/// `no_cache: true` (the latter is the shape the CLI `--no-cache` flag maps to).
fn cache_bypassed(args: &Value) -> bool {
    let bypass_str = args
        .get("cache")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("bypass"))
        .unwrap_or(false);
    let no_cache = args
        .get("no_cache")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    bypass_str || no_cache
}

/// Whole-DB scope digest: a hash over the content-hashes in `<db>.filemeta.json`.
/// Using the whole-DB digest (rather than per-query file scope) is simpler and
/// still correct — a wider scope only ever causes MORE conservative misses,
/// never an incorrect hit. Returns 0 when the filemeta sidecar is absent
/// (consistent across calls, so caching still works on the generation key
/// alone).
fn whole_db_scope_digest(db_path: &Path) -> u64 {
    let filemeta_path = nestweaver_engine::sidecar_path(db_path, ".filemeta.json");
    let sidecar = nestweaver_engine::load_filemeta_sidecar(&filemeta_path);
    // Repo-qualify each pair: identical rel paths in two repos are distinct
    // inputs (and same-path+same-hash pairs must not XOR-cancel).
    let pairs: Vec<(String, String)> = sidecar
        .repos
        .iter()
        .flat_map(|(ruid, files)| {
            files
                .iter()
                .map(move |(p, m)| (format!("{ruid}\u{0}{p}"), m.content_hash.clone()))
        })
        .collect();
    nestweaver_store::cache::scope_digest_from_hashes(
        pairs.iter().map(|(p, h)| (p.as_str(), h.as_str())),
    )
}

/// Run a cacheable tool through the F16 response cache.
///
/// On a HIT (same persisted `graph_generation` AND same scope digest AND not
/// expired) the stored, byte-identical response is returned without running the
/// tool. On a MISS the tool runs and its response is inserted.
///
/// Why no daemon is needed: the generation is persisted to `<db>.generation`
/// (P0.2) and bumped at the end of every index/reindex. A fresh process loads
/// that value on open, so any entry written under an older generation misses —
/// the cache is self-invalidating on reindex without any sweep.
///
/// If the db path is unknown (e.g. in tests with no server-set path), caching
/// is skipped and the tool runs directly.
/// Number of cache misses between periodic disk flushes.
const CACHE_FLUSH_INTERVAL: u32 = 50;

/// Salt folded into the response-cache key to keep redacted results from leaking
/// across identities. `None`/`All` (no scoping) returns 0 — the key is unchanged
/// and existing entries still hit, preserving zero behavior change when no
/// `[authz]` policy is configured. A restricting `Only(set)` hashes its sorted
/// repo_uids so each distinct visibility scope keys its own cache slot.
fn visibility_cache_salt(visible: Option<&nestweaver_engine::authz::VisibleRepos>) -> u64 {
    use nestweaver_engine::authz::VisibleRepos;
    use std::hash::{Hash, Hasher};
    match visible {
        None | Some(VisibleRepos::All) => 0,
        Some(VisibleRepos::Only(set)) => {
            let mut uids: Vec<&str> = set.iter().map(String::as_str).collect();
            uids.sort_unstable();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            "visible_only".hash(&mut hasher);
            for uid in uids {
                uid.hash(&mut hasher);
            }
            hasher.finish()
        }
    }
}

/// Fold the visibility salt into the base cache key. A salt of 0 (disabled/`All`)
/// returns the base key byte-for-byte so existing entries still hit — zero
/// behavior change for the single-trust-domain default. A non-zero salt is mixed
/// through a hasher rather than XORed: XOR is linear and commutative, so distinct
/// `(query, scope)` pairs could in principle collide; a hash mix avoids that.
fn mix_visibility_cache_key(base: u64, salt: u64) -> u64 {
    if salt == 0 {
        return base;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    base.hash(&mut hasher);
    salt.hash(&mut hasher);
    hasher.finish()
}

fn maybe_cached(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
    embed_model: Option<&dyn EmbedQueryFn>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    let Ok(db_path) = current_db_path(store) else {
        return dispatch_uncached(store, tantivy, name, args, embed_model, cancel, visible);
    };
    if store.is_index_publication_dirty() {
        return dispatch_uncached(store, tantivy, name, args, embed_model, cancel, visible);
    }

    let max_mb = CACHE_MAX_SIZE_MB.with(|c| c.get());
    // Fold the caller's repo-visibility into the cache key so a redacted
    // blast_radius result is never served across identities (R9b). A `None`/`All`
    // visibility (the unconfigured single-trust-domain default) contributes salt
    // 0, so the key is byte-identical to before and existing entries still hit —
    // zero behavior change when no `[authz]` policy is set. A restricting
    // `Only(set)` mixes a stable digest of its sorted repo_uids, giving each
    // visibility scope its own cache slot.
    let key = mix_visibility_cache_key(
        nestweaver_store::cache::ResponseCache::key(name, &args),
        visibility_cache_salt(visible),
    );
    let generation = store.graph_generation();
    let scope_digest = whole_db_scope_digest(&db_path);

    // Lazily initialise the in-process cache for this db path, then check for a hit.
    let hit_bytes = RESPONSE_CACHE.with(|map| {
        let mut map = map.borrow_mut();
        let cache = map
            .entry(db_path.clone())
            .or_insert_with(|| nestweaver_store::cache::ResponseCache::open(&db_path, max_mb));
        cache.get(key, generation, scope_digest)
    });

    if let Some(bytes) = hit_bytes
        && !store.is_index_publication_dirty()
        && store.graph_generation() == generation
    {
        CACHE_HITS.with(|c| c.set(c.get() + 1));
        // No save() on hit — LRU timestamp update is not worth a disk round-trip.
        let value: Value =
            serde_json::from_slice(&bytes).with_context(|| "decode cached response")?;
        return Ok(value);
    }

    CACHE_MISSES.with(|c| c.set(c.get() + 1));
    let result = dispatch_uncached(store, tantivy, name, args, embed_model, cancel, visible)?;
    if store.is_index_publication_dirty() || store.graph_generation() != generation {
        return Ok(result);
    }
    match serde_json::to_vec(&result) {
        Ok(bytes) => {
            // Insert into the in-process cache, then decide whether to flush.
            let should_flush = RESPONSE_CACHE.with(|map| {
                let mut map = map.borrow_mut();
                let cache = map.entry(db_path.clone()).or_insert_with(|| {
                    nestweaver_store::cache::ResponseCache::open(&db_path, max_mb)
                });
                cache.insert(key, name, &bytes, generation, scope_digest);
                let count = FLUSH_COUNTER.with(|c| {
                    let next = c.get() + 1;
                    c.set(next);
                    next
                });
                count >= CACHE_FLUSH_INTERVAL
            });
            if should_flush {
                flush_response_cache();
            }
        }
        Err(e) => {
            tracing::debug!(
                tool = name,
                "cache: serialization failed, skipping insert: {e}"
            );
        }
    }
    Ok(result)
}

/// Flush all in-process response caches to disk and reset the flush counter.
/// Called periodically (every `CACHE_FLUSH_INTERVAL` misses) and can be
/// called explicitly on clean shutdown.
pub fn flush_response_cache() {
    RESPONSE_CACHE.with(|map| {
        for cache in map.borrow().values() {
            cache.flush();
        }
    });
    FLUSH_COUNTER.with(|c| c.set(0));
}

/// Session cache stats `(size_bytes, entries, hit_rate)` for `brain_status`.
/// `hit_rate` is `hits / (hits + misses)` over this process's lifetime;
/// it is `None` when no cacheable calls have been made yet. Honest framing:
/// this hit-rate is unproven and should be measured in real usage.
fn cache_stats(db_path: &Path) -> (u64, usize, Option<f64>) {
    let max_mb = CACHE_MAX_SIZE_MB.with(|c| c.get());
    // Prefer the in-process cache for accurate stats; fall back to disk read
    // if the in-process cache hasn't been seeded for this path yet.
    let (size_bytes, len) = RESPONSE_CACHE.with(|map| {
        let map = map.borrow();
        if let Some(cache) = map.get(db_path) {
            (cache.size_bytes(), cache.len())
        } else {
            let cache = nestweaver_store::cache::ResponseCache::open(db_path, max_mb);
            (cache.size_bytes(), cache.len())
        }
    });
    let hits = CACHE_HITS.with(|c| c.get());
    let misses = CACHE_MISSES.with(|c| c.get());
    let total = hits + misses;
    let hit_rate = if total > 0 {
        Some(hits as f64 / total as f64)
    } else {
        None
    };
    (size_bytes, len, hit_rate)
}

/// F5: read a symbol's source span (not the whole file). Resolves UIDs/names/
/// FQNs, optionally includes adjacent symbols, and respects a token budget.
/// Bound a list of client-supplied identifiers (symbol UIDs/names/FQNs, repo-
/// relative paths) so an oversized entry or an over-long list can't be echoed
/// back verbatim (a response-amplification lever) or waste work. A real
/// identifier is at most a few hundred bytes, so truncating a huge one keeps it
/// non-matching (→ not_found) while capping the response. Truncation is
/// char-boundary safe.
fn bound_identifiers(mut v: Vec<String>) -> Vec<String> {
    const MAX_LEN: usize = 512;
    const MAX_COUNT: usize = 1000;
    v.truncate(MAX_COUNT);
    for s in &mut v {
        if s.len() > MAX_LEN {
            let mut end = MAX_LEN;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
        }
    }
    v
}

fn tool_read_symbols(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let targets: Vec<String> = bound_identifiers(
        args.get("targets")
            .or_else(|| args.get("uids_or_fqns"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    );
    if targets.is_empty() {
        return Err(anyhow!(
            "'targets' must be a non-empty array of symbol UIDs, names, or FQNs"
        ));
    }
    let neighbors = args
        .get("include_neighbors")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(u8::MAX as u64) as u8;
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let root = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // In server mode, try to read source spans from bare clones via
    // GitBareReader instead of the filesystem (which has no checkout tree).
    let value = if is_server_mode() {
        let bare_result = try_read_symbols_from_bare(store, &targets, neighbors, token_budget);
        match bare_result {
            Some(res) => serde_json::to_value(res)?,
            None => {
                let reader = nestweaver_engine::content_reader::FilesystemReader::new(&root);
                let res = nestweaver_engine::read_symbols::read_symbols(
                    store,
                    &targets,
                    &reader,
                    neighbors,
                    token_budget,
                );
                serde_json::to_value(res)?
            }
        }
    } else {
        let reader = nestweaver_engine::content_reader::FilesystemReader::new(&root);
        let res = nestweaver_engine::read_symbols::read_symbols(
            store,
            &targets,
            &reader,
            neighbors,
            token_budget,
        );
        serde_json::to_value(res)?
    };
    let mut value = value;

    // If we're in server mode and the result has no symbols with bodies,
    // add a diagnostic note for AI agents.
    if is_server_mode() {
        let has_empty_bodies = value
            .get("symbols")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| {
                arr.iter().all(|s| {
                    s.get("body")
                        .and_then(|b| b.as_str())
                        .is_none_or(|b| b.is_empty())
                })
            });
        if has_empty_bodies {
            value["server_note"] = serde_json::json!(
                "Running in server mode — source files are in bare clones without \
                 checkout trees. The bare clone workspace could not be located \
                 (expected at <db_parent>/workspace/). Symbol metadata (name, kind, \
                 location, edges) is available but source spans are empty. \
                 Alternatives: use brain_search or brain_context for content lookup, \
                 or connect a local client with filesystem access for full source spans."
            );
        }
    }

    // Non-server mode: if any returned symbol's source span could not be read
    // (file not found from the working directory), the body is an empty string
    // that looks identical to a genuinely empty symbol. Surface an honest note so
    // an agent in the wrong cwd knows to pass `root` instead of trusting "".
    if !is_server_mode() {
        let unreadable = value
            .get("symbols")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|s| s.get("body_available").and_then(|b| b.as_bool()) == Some(false))
                    .count()
            })
            .unwrap_or(0);
        if unreadable > 0 {
            value["note"] = serde_json::json!(format!(
                "{unreadable} symbol(s) returned an empty body because their source file could \
                 not be read from the working directory ({}). Pass `root` (the repo path) or run \
                 from the repo root to get source spans.",
                root.display()
            ));
        }
    }

    Ok(value)
}

/// Attempt to read symbol source spans from bare git clones in server mode.
///
/// Derives the bare clone workspace root from the current DB path
/// (convention: `<db_parent>/workspace/`). Groups targets by repo so that
/// symbols from different repos each get their own `GitBareReader`.
/// Returns `None` if the workspace directory doesn't exist or if no repo
/// can be resolved for any target.
fn try_read_symbols_from_bare(
    store: &GraphStore,
    targets: &[String],
    neighbors: u8,
    token_budget: Option<usize>,
) -> Option<nestweaver_engine::read_symbols::ReadSymbolsResult> {
    use std::collections::HashMap;

    // Derive workspace root from the thread-local db_path.
    let db_path = current_db_path(store).ok()?;
    let workspace_root = db_path.parent()?.join("workspace");
    if !workspace_root.is_dir() {
        return None;
    }

    // Group targets by repo_uid, preserving input order within each group.
    // Targets that cannot be resolved go into a special "unresolved" bucket
    // so they appear in the final not_found list.
    let mut repo_groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut repo_index: HashMap<String, usize> = HashMap::new();
    let mut unresolved: Vec<String> = Vec::new();

    for spec in targets {
        if let Some(repo_uid) = resolve_repo_for_spec(store, spec) {
            if let Some(&idx) = repo_index.get(&repo_uid) {
                repo_groups[idx].1.push(spec.clone());
            } else {
                let idx = repo_groups.len();
                repo_index.insert(repo_uid.clone(), idx);
                repo_groups.push((repo_uid, vec![spec.clone()]));
            }
        } else {
            unresolved.push(spec.clone());
        }
    }

    if repo_groups.is_empty() {
        return None;
    }

    // Read symbols per repo group and merge results.
    let mut merged = nestweaver_engine::read_symbols::ReadSymbolsResult::default();
    merged.not_found.extend(unresolved);
    let mut remaining_budget = token_budget;

    for (repo_uid, group_targets) in &repo_groups {
        let reader = match bare_reader_for_repo(store, &workspace_root, repo_uid) {
            Some(r) => r,
            None => {
                // Cannot open this repo's bare clone — mark all targets as not_found.
                merged.not_found.extend(group_targets.iter().cloned());
                continue;
            }
        };

        let partial = nestweaver_engine::read_symbols::read_symbols(
            store,
            group_targets,
            &reader,
            neighbors,
            remaining_budget,
        );

        // Subtract consumed budget.
        if let Some(budget) = remaining_budget {
            let used: usize = partial.symbols.iter().map(|s| s.body.len() / 4 + 16).sum();
            remaining_budget = Some(budget.saturating_sub(used));
        }

        merged.symbols.extend(partial.symbols);
        merged.not_found.extend(partial.not_found);
        merged.ambiguous.extend(partial.ambiguous);
        merged.dropped.extend(partial.dropped);
        merged.truncated = merged.truncated || partial.truncated;
    }

    Some(merged)
}

/// Build a `GitBareReader` for the given repo_uid.
fn bare_reader_for_repo(
    store: &GraphStore,
    workspace_root: &std::path::Path,
    repo_uid: &str,
) -> Option<nestweaver_engine::content_reader::GitBareReader> {
    let repo = store.lookup_repo(repo_uid).ok().flatten()?;
    // The bare clone is named solely from the URL by `ensure_clone` (it never
    // sees the explicit repo name), so resolve the on-disk dir the same way —
    // via the URL-hashed clone-dir name, not the display/identity basename.
    let clone_dir = nestweaver_engine::pull::clone_dir_name_from_url(&repo.url);
    let bare_path = workspace_root.join(format!("{clone_dir}.git"));
    if !bare_path.is_dir() {
        return None;
    }
    // Read source at the repo's recorded indexed_sha — symbol spans come from the
    // graph indexed at that commit, and the bare clone's HEAD may have been
    // fetched past it. Fall back to HEAD only when no server sha is recorded
    // (local repos store "local" or an empty sha).
    if repo.indexed_sha.is_empty() || repo.indexed_sha == "local" {
        nestweaver_engine::content_reader::GitBareReader::from_head(&bare_path).ok()
    } else {
        Some(nestweaver_engine::content_reader::GitBareReader::new(
            &bare_path,
            &repo.indexed_sha,
        ))
    }
}

/// Build an inline-body reader resolver for the current mode.
///
/// In server mode this yields a `GitBareReader` per `repo_uid` (mirroring how
/// `read_symbols` selects a reader per repo), so opt-in inline symbol bodies are
/// read from the bare clone instead of a non-existent working tree. In local
/// mode it returns `None`, so `populate_inline_bodies` falls back to a
/// `FilesystemReader` and behavior is unchanged.
type BoxedInlineBodyResolver<'a> =
    Box<dyn Fn(&str) -> Option<Box<dyn nestweaver_engine::content_reader::ContentReader>> + 'a>;

fn inline_body_reader_resolver(store: &GraphStore) -> Option<BoxedInlineBodyResolver<'_>> {
    if !is_server_mode() {
        return None;
    }
    let db_path = current_db_path(store).ok()?;
    let workspace_root = db_path.parent()?.join("workspace");
    if !workspace_root.is_dir() {
        return None;
    }
    Some(Box::new(move |repo_uid: &str| {
        bare_reader_for_repo(store, &workspace_root, repo_uid)
            .map(|r| Box::new(r) as Box<dyn nestweaver_engine::content_reader::ContentReader>)
    }))
}

/// Resolve a symbol spec to its `repo_uid` by looking up the symbol in the store.
fn resolve_repo_for_spec(store: &GraphStore, spec: &str) -> Option<String> {
    if spec.starts_with("sym:") {
        return store.lookup_symbol(spec).ok().map(|s| s.repo_uid);
    }
    let name = spec
        .rsplit("::")
        .next()
        .unwrap_or(spec)
        .rsplit('.')
        .next()
        .unwrap_or(spec);
    store
        .lookup_symbols_by_name(name)
        .ok()
        .and_then(|syms| syms.into_iter().next())
        .map(|s| s.repo_uid)
}

fn tool_schema_read_symbols() -> Value {
    json!({
        "name": "read_symbols",
        "description": "Read a symbol's source code span (start_line..end_line) without loading the entire file.\n\nGuidelines:\n- Accepts UIDs (sym:...), bare names, or FQNs; ambiguous names return candidate UIDs to disambiguate\n- Use include_neighbors to also return adjacent symbols in the same file\n- Use token_budget to cap combined output size\n\nLimitations:\n- Only reads indexed code symbols, not markdown notes (use note_get for those)\n- Requires the repo root to resolve file paths (defaults to server working directory)\n\nIn server mode (bare clones), bodies may be empty with a server_note explaining the limitation.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "targets": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Symbol UIDs (sym:...), names, or FQNs to read."
                },
                "uids_or_fqns": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Backward-compatible alias for targets."
                },
                "include_neighbors": {
                    "type": "integer",
                    "description": "Include N adjacent symbols in the same file (default 0)."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Approximate token cap for the combined output. The first requested symbol is always returned in full; subsequent symbols are dropped once the budget is exceeded. Omit for no cap; a budget of 0 therefore returns just the first symbol."
                },
                "root": {
                    "type": "string",
                    "description": "Repository root for resolving file paths (default: server working directory)."
                }
            },
            "anyOf": [
                { "required": ["targets"] },
                { "required": ["uids_or_fqns"] }
            ]
        }
    })
}

/// F3: trigram-accelerated regex search over indexed text. Lets agents run a
/// real regex against Section bodies, Note titles, and Symbol signatures
/// without shelling out to rg/grep.
fn tool_regex_search(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let pattern = args
        .get("pattern")
        .or_else(|| args.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'pattern' must be a string"))?;

    // Note: regex_search works in server mode — GraphStore::regex_search
    // searches over indexed symbol text, not raw source files on disk.

    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let kinds = parse_string_array(&args, "kinds");
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let max_millis = args.get("max_millis").and_then(|v| v.as_u64());

    let res = store
        .regex_search(pattern, path_prefix, kinds.as_deref(), limit, max_millis)
        .map_err(|e| anyhow!("regex_search: {e}"))?;
    let mut resp = serde_json::to_value(res)?;
    if resp
        .get("results")
        .and_then(|r| r.as_array())
        .is_some_and(|a| a.is_empty())
        && resp
            .get("truncated")
            .and_then(|t| t.as_bool())
            .unwrap_or(false)
    {
        resp["note"] = json!(
            "Pattern matched no candidates within the scan budget. Results may exist beyond the scanned range."
        );
    }
    Ok(resp)
}

fn tool_schema_regex_search() -> Value {
    json!({
        "name": "regex_search",
        "description": "Run a Rust regex against indexed text (section bodies, note titles, symbol signatures) with trigram-accelerated pre-filtering.\n\nGuidelines:\n- Use for exact pattern matching; for fuzzy/semantic lookup use brain_search instead\n- Output includes {results:[{uid, kind, title, location, line, snippet}], truncated, scanned_fallback}\n- scanned_fallback is set when no trigram index exists or the pattern has no usable literals\n\nLimitations:\n- Candidate cap of 5000 or time budget (default 2000ms) may truncate results\n- Does not search binary files or unindexed content",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust regex pattern. Example: \"fn\\\\s+authenticate\" or \"(?i)todo\"." },
                "query": { "type": "string", "description": "Backward-compatible alias for pattern." },
                "path_prefix": { "type": "string", "description": "Restrict to nodes whose file path starts with this prefix." },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict to these node kinds: Section, Note, Symbol (case-insensitive)."
                },
                "limit": { "type": "integer", "description": "Maximum results to return. Default: unlimited (capped by the candidate budget)." },
                "max_millis": { "type": "integer", "description": "Wall-clock time budget in milliseconds. Default 2000." }
            },
            "anyOf": [
                { "required": ["pattern"] },
                { "required": ["query"] }
            ]
        }
    })
}

/// F4: counts-only companion to regex_search. Counts matches per pattern across
/// indexed text and reports the busiest files.
fn tool_count_patterns(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    const MAX_PATTERNS: usize = 64;
    let patterns = parse_string_array(&args, "patterns")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow!("'patterns' must be a non-empty array of regex strings"))?;
    // Each pattern is a full O(corpus) scan, so an unbounded array is a cheap
    // CPU/response-amplification lever; an empty-string pattern matches everything.
    if patterns.len() > MAX_PATTERNS {
        anyhow::bail!(
            "too many patterns ({}); maximum is {MAX_PATTERNS}",
            patterns.len()
        );
    }
    if patterns.iter().any(|p| p.trim().is_empty()) {
        anyhow::bail!("empty pattern strings are not allowed");
    }
    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let kinds = parse_string_array(&args, "kinds");

    let counts = store
        .count_patterns(&patterns, path_prefix, kinds.as_deref())
        .map_err(|e| anyhow!("count_patterns: {e}"))?;
    Ok(json!({ "patterns": serde_json::to_value(counts)? }))
}

fn tool_schema_count_patterns() -> Value {
    json!({
        "name": "count_patterns",
        "description": "Count regex matches across indexed text without returning the matches themselves — useful for frequency analysis.\n\nGuidelines:\n- Pass multiple patterns to compare counts in one call\n- Returns per-pattern {pattern, total_matches, files_matched, top_files:[{path,count}]}\n- For actual match text, use regex_search instead\n\nLimitations:\n- Counts one match per node, not per occurrence within a node\n- Same trigram/fallback behavior as regex_search",
        "inputSchema": {
            "type": "object",
            "properties": {
                "patterns": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One or more Rust regex patterns to count."
                },
                "path_prefix": { "type": "string", "description": "Restrict to nodes whose file path starts with this prefix." },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict to these node kinds: Section, Note, Symbol (case-insensitive)."
                }
            },
            "required": ["patterns"]
        }
    })
}

// ── F9: document-graph tools ──────────────────────────────────────────────

fn tool_brain_broken_links(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let max_suggestions = args
        .get("max_suggestions")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(5);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    let all_links = broken_links(store, max_suggestions)?;
    let total = all_links.len();
    let links: Vec<_> = all_links.into_iter().take(limit).collect();
    Ok(
        json!({ "broken_links": serde_json::to_value(&links)?, "total": total, "returned": links.len() }),
    )
}

fn tool_schema_brain_broken_links() -> Value {
    json!({
        "name": "brain_broken_links",
        "description": "Find wikilinks in the vault that did not resolve cleanly — ambiguous or low-confidence targets (confidence < 1.0).\n\nGuidelines:\n- Use when auditing vault health or before bulk link repairs\n- Each result includes fuzzy-matched suggested target UIDs for repair\n- Returns empty when no vault is indexed\n\nLimitations:\n- Only detects wikilink resolution issues, not broken external URLs\n- Suggestions are fuzzy title matches, not guaranteed correct targets",
        "inputSchema": {
            "type": "object",
            "properties": {
                "max_suggestions": {
                    "type": "integer",
                    "description": "Max suggested target UIDs per broken link (default 5).",
                    "default": 5
                },
                "limit": {
                    "type": "integer",
                    "description": "Max broken links to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_brain_orphan_documents(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let vault = args.get("vault").and_then(|v| v.as_str());
    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let allowlist = parse_string_array(&args, "allowlist").unwrap_or_default();
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    let all_orphans = orphan_documents(store, vault, path_prefix, &allowlist)?;
    let total = all_orphans.len();
    let orphans: Vec<_> = all_orphans.into_iter().take(limit).collect();
    Ok(
        json!({ "orphans": serde_json::to_value(&orphans)?, "total": total, "returned": orphans.len() }),
    )
}

fn tool_schema_brain_orphan_documents() -> Value {
    json!({
        "name": "brain_orphan_documents",
        "description": "Find notes with zero inbound and zero outbound wikilinks — disconnected from the knowledge graph.\n\nGuidelines:\n- Candidates to link up or archive; index/MOC notes are excluded via a configurable allowlist\n- Use vault and path_prefix filters to scope the search\n- Returns empty when no vault is indexed\n\nLimitations:\n- Only considers wikilinks, not tag co-occurrence or other relationships\n- Default allowlist excludes Projects.md, index.md, README.md, and MOC-containing paths",
        "inputSchema": {
            "type": "object",
            "properties": {
                "vault": { "type": "string", "description": "Restrict to this vault UID." },
                "path_prefix": { "type": "string", "description": "Restrict to notes whose file path starts with this prefix." },
                "allowlist": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Note paths/titles to exclude (overrides the default index/MOC allowlist when provided)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max orphan documents to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_brain_topic_clusters(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let resolution = args
        .get("resolution")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    let all_clusters = topic_clusters(store, resolution)?;
    let total = all_clusters.len();
    let clusters: Vec<_> = all_clusters.into_iter().take(limit).collect();
    Ok(
        json!({ "clusters": serde_json::to_value(&clusters)?, "total": total, "returned": clusters.len() }),
    )
}

fn tool_schema_brain_topic_clusters() -> Value {
    json!({
        "name": "brain_topic_clusters",
        "description": "Discover thematic structure of a vault by running Leiden community detection over the note-to-note wikilink graph.\n\nGuidelines:\n- Each cluster is labelled by its most central member (highest PageRank)\n- Adjust resolution parameter: higher yields more, smaller clusters\n- Returns empty when no vault is indexed\n\nLimitations:\n- Only considers wikilink edges between notes, not tags or code references\n- Label quality depends on the most-central note having a descriptive title",
        "inputSchema": {
            "type": "object",
            "properties": {
                "resolution": {
                    "type": "number",
                    "description": "Leiden resolution — higher yields more, smaller clusters (default 0.5).",
                    "default": 0.5
                },
                "limit": {
                    "type": "integer",
                    "description": "Max clusters to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_brain_tag_graph(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    // `tag` is optional. When present we accept only a string (reject other
    // JSON types); when absent we return the whole tag co-occurrence graph.
    match args.get("tag") {
        Some(Value::Null) | None => {
            let all_tags = tag_graph_all(store)?;
            let total = all_tags.len();
            let tags: Vec<_> = all_tags.into_iter().take(limit).collect();
            Ok(
                json!({ "tags": serde_json::to_value(&tags)?, "total": total, "returned": tags.len() }),
            )
        }
        Some(Value::String(tag)) => {
            let tg = tag_graph(store, tag)?;
            Ok(serde_json::to_value(&tg)?)
        }
        Some(_) => Err(anyhow!("'tag' must be a string")),
    }
}

fn tool_schema_brain_tag_graph() -> Value {
    json!({
        "name": "brain_tag_graph",
        "description": "Explore tag relationships in a vault via co-occurrence analysis. Two modes: (1) with tag — returns co-occurring tags for a focus tag; (2) without tag — returns the full tag co-occurrence graph.\n\nGuidelines:\n- Use without tag for taxonomy-drift detection across the vault\n- The tag argument may include or omit a leading #\n- Results sorted by shared-note count (with tag) or note count descending (without)\n\nLimitations:\n- Co-occurrence is based on shared notes, not semantic similarity\n- Returns count 0 / empty when the tag or vault is absent",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tag": { "type": "string", "description": "Optional focus tag (with or without leading #). When omitted, returns the full tag co-occurrence graph for all tags." },
                "limit": {
                    "type": "integer",
                    "description": "Max tags to return in the all-tags listing (default 50). Ignored when a specific tag is queried.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_brain_doc_stats(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let top_tags_limit = args
        .get("top_tags_limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);
    let stats = doc_stats(store, top_tags_limit)?;
    Ok(serde_json::to_value(&stats)?)
}

fn tool_schema_brain_doc_stats() -> Value {
    json!({
        "name": "brain_doc_stats",
        "description": "Get a one-shot health summary of a vault's document graph — note counts, broken links, orphans, tag distribution, and notes-by-year.\n\nGuidelines:\n- Call once for a quick vault health overview before deeper analysis\n- All seven keys are always returned, even on an empty vault (zeros/empty collections)\n- Output: {total_notes, total_wikilinks, broken_wikilinks, orphans, avg_outdegree, top_tags, notes_by_year}\n\nLimitations:\n- Aggregates other brain document tools; for detailed broken links use brain_broken_links directly",
        "inputSchema": {
            "type": "object",
            "properties": {
                "top_tags_limit": {
                    "type": "integer",
                    "description": "Max entries in top_tags (default 10).",
                    "default": 10
                }
            }
        }
    })
}

// ── F11: memory-bank tools ───────────────────────────────────────────────────

/// Current wall-clock time as Unix epoch seconds (f64). Falls back to 0.0
/// (pre-epoch) only if the system clock is before 1970, which never happens.
fn now_epoch_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn tool_brain_memory_lint(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    let mut report = serde_json::to_value(memory_lint(store, now_epoch_secs())?)?;
    // Truncate each lint category to `limit` and report totals.
    if let Some(obj) = report.as_object_mut() {
        let mut totals = serde_json::Map::new();
        for (key, val) in obj.iter_mut() {
            if let Some(arr) = val.as_array_mut() {
                let total = arr.len();
                arr.truncate(limit);
                totals.insert(format!("{key}_total"), json!(total));
            }
        }
        for (k, v) in totals {
            obj.insert(k, v);
        }
        obj.insert("limit".to_string(), json!(limit));
    }
    Ok(report)
}

fn tool_schema_brain_memory_lint() -> Value {
    json!({
        "name": "brain_memory_lint",
        "description": "Audit a memory-bank vault for health problems across seven categories: stale notes, contradictions, orphans, broken wikilinks, supersession chains, schema drift, and dangling relationships.\n\nGuidelines:\n- All seven keys always present in output; empty on a no-vault database\n- Use limit to cap results per category; totals are always reported\n- Schema drift checks against _templates/<kind>.md templates\n\nLimitations:\n- Stale detection uses a fixed 90-day threshold for status:active notes\n- Schema drift requires template files to exist in _templates/",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max results per lint category (default 50). Totals are always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_brain_memory_consolidate(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let apply = args.get("apply").and_then(|v| v.as_bool()).unwrap_or(false);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    let mut manifest = serde_json::to_value(memory_consolidate(store, apply, now_epoch_secs())?)?;
    // Truncate proposals to limit and report total.
    if let Some(obj) = manifest.as_object_mut() {
        let (total, returned) =
            if let Some(proposals) = obj.get_mut("proposals").and_then(|v| v.as_array_mut()) {
                let total = proposals.len();
                proposals.truncate(limit);
                (total, proposals.len())
            } else {
                (0, 0)
            };
        obj.insert("proposals_total".to_string(), json!(total));
        obj.insert("proposals_returned".to_string(), json!(returned));
    }
    Ok(manifest)
}

fn tool_schema_brain_memory_consolidate() -> Value {
    json!({
        "name": "brain_memory_consolidate",
        "description": "Propose promotions of vault notes up memory tiers (daily logs to ideas to project files). DRY-RUN by default.\n\nGuidelines:\n- Set apply:true to execute moves; default is safe dry-run\n- Promotes daily logs referenced by 3+ idea notes (>14 days old) and ideas referenced by both sync.md and status.md\n- Returns proposals with source paths, destinations, and evidence\n\nLimitations:\n- Only proposes promotions along the predefined tier path\n- Requires specific vault structure (_logs/, _ideas/) to detect candidates",
        "inputSchema": {
            "type": "object",
            "properties": {
                "apply": {
                    "type": "boolean",
                    "description": "Opt into write-mode: move files to their promoted destinations (default false = safe dry-run).",
                    "default": false
                },
                "limit": {
                    "type": "integer",
                    "description": "Max proposals to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_brain_memory_related(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let uid = args
        .get("uid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'uid' (string) is required"))?;
    let edge_types = parse_string_array(&args, "edge_types").unwrap_or_default();
    let depth = args
        .get("depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let related = memory_related(store, uid, &edge_types, depth)?;
    Ok(json!({ "related": serde_json::to_value(&related)?, "total": related.len() }))
}

fn tool_schema_brain_memory_related() -> Value {
    json!({
        "name": "brain_memory_related",
        "description": "Walk the typed relationship graph from a note — Supersedes, DependsOn, CausedBy, RelatesTo — without generic wikilink noise.\n\nGuidelines:\n- BFS traversal from seed uid over chosen edge_types to configurable depth (default 2)\n- Returns only typed neighbours, not generic wikilinks\n- Empty on unknown node or no-vault database\n\nLimitations:\n- Only follows the four typed edge types, not wikilinks or tag co-occurrence\n- Maximum traversal depth may miss distant relationships",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Seed note UID to traverse from." },
                "edge_types": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Edge types to follow (Supersedes, DependsOn, CausedBy, RelatesTo; case/format-insensitive). Default: all four."
                },
                "depth": { "type": "integer", "description": "Max BFS depth (default 2).", "default": 2 }
            },
            "required": ["uid"]
        }
    })
}

/// Parse a JSON string array argument into `Vec<String>`; returns `None` when
/// the key is absent.
fn parse_string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    })
}

/// Wrap a tool's structured output in MCP's `content` envelope. Returns
/// both a human-readable text block (rendering the JSON) and the
/// structured value via `structuredContent`, so clients can use either.
pub fn wrap_tool_result(value: Value) -> Value {
    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": pretty }],
        "structuredContent": value,
        "isError": false,
    })
}

/// Wrap an error as a tool-call result so the client receives a proper MCP
/// error indication (rather than a JSON-RPC-level error which terminates
/// the call sequence).
pub fn wrap_tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// Check whether the caller requested concise output. Returns `true` when
/// `response_format` is explicitly `"concise"`. Defaults to `false`
/// (detailed) for backward compatibility.
fn is_concise(args: &Value) -> bool {
    args.get("response_format")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("concise"))
}

// ── 1. brain_context ────────────────────────────────────────────────────────

fn tool_schema_brain_context() -> Value {
    json!({
        "name": "brain_context",
        "description": "Retrieve PPR-ranked structural context from the knowledge graph, seeded by symbol names, note titles, or keywords. Returns mixed-kind results (Symbol, Note, Section, Tag, Heading) within a token budget.\n\nGuidelines:\n- Primary entry point for understanding a topic — use before reading files\n- Seed with specific names (e.g. 'AuthService.validate'), not broad terms\n- Filter with repos, tags, path_prefix, kinds for precision; use response_format 'concise' unless you need full bodies\n\nLimitations:\n- Only searches indexed repos/vaults — check stale_check if results seem stale\n- Ranked by graph proximity, not recency (use recency_weight to add time decay)",
        "inputSchema": {
            "type": "object",
            "properties": {
                "seeds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One or more seed strings to anchor the PPR walk. Accepts note titles, tag names (with or without #), symbol names, free-text terms, or UIDs (sym:/note:/head:/sec:/tag:)."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Approximate cap on the connected list (chars / 4). Default 2000. Increase for broader context, decrease for focused results.",
                    "default": 2000
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns names and relationships only; \"detailed\" (default) adds file paths, relevance scores, and UIDs."
                },
                "repos": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter to specific repo UIDs or names (post-PPR). Only nodes whose location matches one of these strings are kept."
                },
                "vaults": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter to specific vault UIDs or names (post-PPR). Only note/heading/section nodes whose UID or location matches are kept."
                },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Include only nodes with these kind prefixes (e.g. Symbol, Note, Section, Tag, Heading). Case-insensitive prefix match against the node's kind field."
                },
                "path_prefix": {
                    "type": "string",
                    "description": "Include only nodes whose location (file path) starts with this prefix."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Include only nodes tagged with any of these tags (applies to Note and Section nodes; Symbol nodes are always kept)."
                },
                "exclude_tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exclude nodes tagged with any of these tags (applies to Note and Section nodes)."
                },
                "weight_ppr": {
                    "type": "number",
                    "description": "PPR ranking weight for hybrid RRF fusion. Default 0.7."
                },
                "weight_bm25": {
                    "type": "number",
                    "description": "BM25 text search weight for hybrid RRF fusion. Default 0.3."
                },
                "weight_semantic": {
                    "type": "number",
                    "description": "Semantic embedding weight for hybrid RRF fusion. Default 0.0 (disabled until embeddings are generated)."
                },
                "since": {
                    "type": "string",
                    "description": "ISO 8601 timestamp. Only return Note/Section nodes modified after this time. Symbol nodes always kept."
                },
                "recency_weight": {
                    "type": "number",
                    "default": 0.0,
                    "description": "Multiplier for age-decay boost. 0 = disabled. 1.0 = same-day node ranks ~2x a year-old node."
                },
                "recency_half_life_days": {
                    "type": "number",
                    "default": 30.0,
                    "description": "Half-life for age-decay in days."
                },
                "intent": {
                    "type": "string",
                    "enum": ["find-definition", "understand-architecture", "analyze-impact", "general-context"],
                    "description": "Optional query intent hint that adjusts ranking strategy. 'find-definition' boosts exact name matches; 'understand-architecture' broadens to structural neighbors; 'analyze-impact' follows dependency edges; 'general-context' uses balanced defaults."
                },
                "include_seeds": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, include the full seeds array in the response. Default false — only seeds_expanded (count) is returned to keep responses small."
                },
                "include_bodies": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, embed each high-relevance result's source body inline (under `inline_body`) so you can skip a follow-up read. Only results whose normalized relevance clears the configured threshold (default 0.75) get a body, and bodies count against `token_budget` in rank order. Default false."
                },
                "root": {
                    "type": "string",
                    "description": "Filesystem root used to read symbol source spans for inline bodies. Defaults to the server's working directory. Only relevant with include_bodies=true."
                },
                "prf": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, run pseudo-relevance-feedback query expansion on the BM25 leg: mine high-IDF terms from the top hits and re-run BM25 with them down-weighted. Improves recall on natural-language seeds. Mined terms are returned under `expansion_terms`. Default false."
                },
                "rerank": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, rerank the top-N retrieved candidates before truncation (Feature F17). OFF by default; output is byte-identical when off. The default scorer is a transparent MONOTONIC heuristic — an UNVALIDATED reordering, NOT a proven nDCG win. An optional learned-weights file `<db>.rerank.json` is used instead if present and version-matched, but a learned model should only be trusted after the eval harness + accumulated interaction labels gate it at >= 5% nDCG@10. Reranking only reorders an already-retrieved set; recall is unchanged. Default false."
                }
            },
            "required": ["seeds"]
        }
    })
}

fn tool_brain_context(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    args: Value,
    embed_model: Option<&dyn EmbedQueryFn>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Value, anyhow::Error> {
    let seeds: Vec<String> = args
        .get("seeds")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("'seeds' must be an array of strings"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if seeds.is_empty() {
        return Err(anyhow!("'seeds' must contain at least one string"));
    }
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(2000);

    // RFC #2: optional post-PPR filter parameters.
    let filter_kinds: Option<Vec<String>> =
        args.get("kinds").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect()
        });
    let filter_repos: Option<Vec<String>> =
        args.get("repos").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
    let filter_vaults: Option<Vec<String>> =
        args.get("vaults").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
    let path_prefix: Option<String> = args
        .get("path_prefix")
        .and_then(|v| v.as_str())
        .map(String::from);

    // RFC #6: optional hybrid search weight overrides.
    // Read blend weights and limits from the instance config's [embedding]
    // section when available, falling back to compiled defaults.
    let defaults = {
        match current_instance_config() {
            Some(cfg) => HybridSearchConfig {
                weight_ppr: cfg.embedding.weight_ppr,
                weight_bm25: cfg.embedding.weight_bm25,
                weight_semantic: cfg.embedding.weight_semantic,
                semantic_limit: cfg.embedding.semantic_search_limit,
                always_blend_semantic: cfg.embedding.always_blend_semantic,
                semantic_seed_limit: cfg.embedding.semantic_seed_limit,
                ..HybridSearchConfig::default()
            },
            None => HybridSearchConfig::default(),
        }
    };
    let weight_ppr = args
        .get("weight_ppr")
        .and_then(|v| v.as_f64())
        .unwrap_or(defaults.weight_ppr)
        .max(0.0);
    let weight_bm25 = args
        .get("weight_bm25")
        .and_then(|v| v.as_f64())
        .unwrap_or(defaults.weight_bm25)
        .max(0.0);
    let weight_semantic = args
        .get("weight_semantic")
        .and_then(|v| v.as_f64())
        .unwrap_or(defaults.weight_semantic)
        .max(0.0);
    // If all weights are zero fall back to the defaults so PPR still fires.
    let (weight_ppr, weight_bm25, weight_semantic) =
        if weight_ppr == 0.0 && weight_bm25 == 0.0 && weight_semantic == 0.0 {
            (
                defaults.weight_ppr,
                defaults.weight_bm25,
                defaults.weight_semantic,
            )
        } else {
            (weight_ppr, weight_bm25, weight_semantic)
        };
    // Feature F7 (PRF half): opt in to pseudo-relevance-feedback query
    // expansion on the BM25 leg via `prf: true`. Off by default.
    let prf = args.get("prf").and_then(|v| v.as_bool()).unwrap_or(false);
    let config = HybridSearchConfig {
        weight_ppr,
        weight_bm25,
        weight_semantic,
        prf,
        ..defaults
    };

    // Parse optional intent parameter.
    let intent: Option<nestweaver_store::QueryIntent> = args
        .get("intent")
        .and_then(|v| v.as_str())
        .map(|s| s.parse::<nestweaver_store::QueryIntent>())
        .transpose()
        .map_err(|e| anyhow!("invalid intent: {e}"))?;

    // Load taxonomy aliases so vault-defined name variants resolve correctly.
    let db_path = current_db_path(store).unwrap_or_default();
    let aliases = load_alias_sidecar(&db_path);

    // Hybrid retrieval whenever the Tantivy index is open. When absent
    // (cold start, index missing), falls through to pure-PPR — still
    // correct, just less recall on text-only relevance.
    let mut result: BrainContextResult = build_brain_context_hybrid_with_aliases(
        store,
        &seeds,
        tantivy,
        &config,
        &aliases,
        Some(&db_path),
        intent,
        embed_model,
        cancel,
    )?;

    // Feature F6 (per-path ranking priors) is a deliberate no-op here: the MCP
    // server holds no InstanceConfig at the call site (same as F8, which uses
    // ResponseConfig::default()), so there is no `[ranking]` to load. Priors are
    // applied on the CLI `brain context` / `brain search` paths instead.

    // Build name maps once if filters are present.
    let repo_names = if filter_repos.is_some() {
        build_repo_name_map(store)
    } else {
        std::collections::HashMap::new()
    };
    let vault_names = if filter_vaults.is_some() {
        build_vault_name_map(store)
    } else {
        std::collections::HashMap::new()
    };

    // RFC #2: apply post-PPR filters to seeds and connected lists.
    let apply_filters = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
        if let Some(ref kinds) = filter_kinds {
            nodes.retain(|n| {
                let kind_lower = n.kind.to_lowercase();
                kinds.iter().any(|k| kind_lower.starts_with(k.as_str()))
            });
        }
        if let Some(ref repos) = filter_repos {
            nodes.retain(|n| {
                let filter_lower: Vec<String> = repos.iter().map(|r| r.to_lowercase()).collect();
                // Extract repo_uid from symbol UIDs (sym:repo:{inst}:{hash}:...)
                let node_repo_uid = if n.uid.starts_with("sym:") {
                    let parts: Vec<&str> = n.uid[4..].splitn(4, ':').collect();
                    if parts.len() >= 3 {
                        Some(format!("{}:{}:{}", parts[0], parts[1], parts[2]))
                    } else {
                        None
                    }
                } else {
                    None
                };
                filter_lower.iter().any(|r| {
                    // Match by repo display name
                    if let Some(ref repo_uid) = node_repo_uid
                        && let Some(name) = repo_names.get(repo_uid)
                        && name.to_lowercase().contains(r)
                    {
                        return true;
                    }
                    // Fallback: UID or location substring
                    n.uid.to_lowercase().contains(r) || n.location.to_lowercase().contains(r)
                })
            });
        }
        if let Some(ref vaults) = filter_vaults {
            nodes.retain(|n| {
                let filter_lower: Vec<String> = vaults.iter().map(|v| v.to_lowercase()).collect();
                // Extract vault_uid from note UIDs (note:vlt:{inst}:{hash}:...)
                // or section/heading UIDs (sec:note:vlt:... / head:note:vlt:...)
                let node_vault_uid = {
                    let search = if n.uid.starts_with("note:") {
                        Some(&n.uid[5..])
                    } else if n.uid.starts_with("sec:note:") {
                        Some(&n.uid[9..])
                    } else if n.uid.starts_with("head:note:") {
                        Some(&n.uid[10..])
                    } else {
                        None
                    };
                    search.and_then(|s| {
                        let parts: Vec<&str> = s.splitn(4, ':').collect();
                        if parts.len() >= 3 {
                            Some(format!("{}:{}:{}", parts[0], parts[1], parts[2]))
                        } else {
                            None
                        }
                    })
                };
                filter_lower.iter().any(|v| {
                    if let Some(ref vault_uid) = node_vault_uid
                        && let Some(name) = vault_names.get(vault_uid)
                        && name.to_lowercase().contains(v)
                    {
                        return true;
                    }
                    n.uid.to_lowercase().contains(v) || n.location.to_lowercase().contains(v)
                })
            });
        }
        if let Some(ref prefix) = path_prefix {
            nodes.retain(|n| n.location.starts_with(prefix.as_str()));
        }
    };
    apply_filters(&mut result.seeds);
    apply_filters(&mut result.connected);

    // tags filter: keep only note/section nodes tagged with any of these tags.
    // Symbol nodes are always kept (no tag concept for code).
    if let Some(tags) = args.get("tags").and_then(|v| v.as_array()) {
        let tag_names: Vec<String> = tags
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !tag_names.is_empty() {
            let tagged_notes = store
                .list_note_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_note_uids_with_tags: {e}"))?;
            let tagged_sections = store
                .list_section_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_section_uids_with_tags: {e}"))?;
            let filter_tagged = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                nodes.retain(|item| {
                    if item.kind.to_lowercase().contains("symbol") {
                        return true;
                    }
                    tagged_notes.contains(&item.uid) || tagged_sections.contains(&item.uid)
                });
            };
            filter_tagged(&mut result.seeds);
            filter_tagged(&mut result.connected);
        }
    }

    // exclude_tags filter: remove note/section nodes tagged with any of these tags.
    if let Some(exclude_tags) = args.get("exclude_tags").and_then(|v| v.as_array()) {
        let tag_names: Vec<String> = exclude_tags
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !tag_names.is_empty() {
            let excluded_notes = store
                .list_note_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_note_uids_with_tags: {e}"))?;
            let excluded_sections = store
                .list_section_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_section_uids_with_tags: {e}"))?;
            let filter_excluded = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                nodes.retain(|item| {
                    !excluded_notes.contains(&item.uid) && !excluded_sections.contains(&item.uid)
                });
            };
            filter_excluded(&mut result.seeds);
            filter_excluded(&mut result.connected);
        }
    }

    // since filter: hard filter Note/Section nodes by modified_at.
    if let Some(since) = args.get("since").and_then(|v| v.as_str()) {
        let recent_notes = store
            .list_note_uids_modified_since(since)
            .map_err(|e| anyhow!("list_note_uids_modified_since: {e}"))?;
        let recent_sections = store
            .list_section_uids_modified_since(since)
            .map_err(|e| anyhow!("list_section_uids_modified_since: {e}"))?;
        let filter_since = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
            nodes.retain(|item| {
                if item.kind.to_lowercase().contains("symbol") {
                    return true;
                }
                recent_notes.contains(&item.uid) || recent_sections.contains(&item.uid)
            });
        };
        filter_since(&mut result.seeds);
        filter_since(&mut result.connected);
    }

    // recency bias: soft boost based on note modified_at age.
    let recency_weight = args
        .get("recency_weight")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let recency_half_life_days = args
        .get("recency_half_life_days")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0);
    if recency_weight > 0.0 {
        apply_recency_bias(
            store,
            &mut result.connected,
            recency_weight,
            recency_half_life_days,
        );
        apply_recency_bias(
            store,
            &mut result.seeds,
            recency_weight,
            recency_half_life_days,
        );
    }

    if result.connected.is_empty() && !result.seeds.is_empty() {
        let mut present: std::collections::HashSet<String> =
            result.connected.iter().map(|n| n.uid.clone()).collect();
        for seed in result.seeds.iter().cloned() {
            if present.insert(seed.uid.clone()) {
                result.connected.push(seed);
            }
        }
    }

    // Feature F8: embed high-relevance bodies inline when the caller opted in
    // via `include_bodies: true`. Off by default → output unchanged. Threshold
    // and per-body cap come from [response] config when supplied, else defaults.
    // Feature F17: rerank the top-N retrieved candidates. OFF by default →
    // byte-identical output. Applied after fusion + filters + recency, BEFORE
    // truncation/inline-bodies. The default scorer is a transparent monotonic
    // heuristic (NOT a validated nDCG win); an optional `<db>.rerank.json`
    // learned-weights file is used if present and version-matched. Reranking
    // only reorders an already-retrieved set; recall is unchanged.
    let do_rerank = args
        .get("rerank")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if do_rerank {
        let reranker = nestweaver_engine::select_reranker(Some(&db_path));
        nestweaver_engine::rerank(
            &mut result.connected,
            reranker.as_ref(),
            store,
            nestweaver_engine::RERANK_DEFAULT_TOP_N,
        );
    }

    let include_bodies = args
        .get("include_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if include_bodies {
        let response_config = configured_response();
        let root = args
            .get("root")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        let reader_resolver = inline_body_reader_resolver(store);
        populate_inline_bodies(
            store,
            &mut result.connected,
            &root,
            response_config.inline_body_threshold,
            response_config.inline_max_body_tokens,
            Some(token_budget),
            reader_resolver.as_deref(),
        );
    }

    let concise = is_concise(&args);

    let (cut, used_tokens) = budgeted_cut(&result.connected, token_budget, concise);

    let connected_json: Vec<Value> = result
        .connected
        .iter()
        .take(cut)
        .map(|n| {
            if concise {
                json!({
                    "kind": n.kind,
                    "title": n.title,
                })
            } else {
                let mut obj = json!({
                    "uid": n.uid,
                    "kind": n.kind,
                    "title": n.title,
                    "location": n.location,
                    "relevance": n.relevance,
                });
                if let Some(body) = &n.inline_body {
                    obj["inline_body"] = json!(body);
                }
                obj
            }
        })
        .collect();

    let include_seeds = args
        .get("include_seeds")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut resp = json!({
        "seeds_expanded": result.seeds.len(),
        "connected": connected_json,
        "tokens_used": used_tokens,
        "token_budget": token_budget,
    });

    if include_seeds {
        let seeds_json: Vec<Value> = result
            .seeds
            .iter()
            .map(|n| {
                if concise {
                    json!({
                        "kind": n.kind,
                        "title": n.title,
                    })
                } else {
                    json!({
                        "uid": n.uid,
                        "kind": n.kind,
                        "title": n.title,
                        "location": n.location,
                        "relevance": n.relevance,
                    })
                }
            })
            .collect();
        resp["seeds"] = json!(seeds_json);
    }

    if !result.unresolved_seeds.is_empty() {
        resp["unresolved_seeds"] = json!(result.unresolved_seeds);
    }

    // Feature F7: surface the PRF-mined expansion terms for auditing. Only
    // present when PRF was enabled and mined at least one term.
    if !result.expansion_terms.is_empty() {
        resp["expansion_terms"] = json!(result.expansion_terms);
    }

    Ok(resp)
}

/// Apply age-decay score boost to non-Symbol nodes.
///
/// For each Note/Section node, look up its `modified_at` from the store's notes,
/// compute exponential decay, and multiply `relevance` by `1 + weight * decay`.
/// After adjustment, re-sorts by descending relevance.
fn apply_recency_bias(
    store: &GraphStore,
    nodes: &mut [nestweaver_engine::BrainNode],
    recency_weight: f64,
    recency_half_life_days: f64,
) {
    if recency_weight <= 0.0 {
        return;
    }
    let note_timestamps: std::collections::HashMap<String, f64> = store
        .list_notes(None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| n.modified_at.map(|t| (n.uid, parse_iso8601_to_epoch(&t))))
        .collect();
    let section_note_map: std::collections::HashMap<String, String> = store
        .list_all_sections()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.uid, s.note_uid))
        .collect();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64;

    let ln2 = std::f64::consts::LN_2;
    let half_life_secs = recency_half_life_days * 86_400.0;

    for node in nodes.iter_mut() {
        if node.kind.to_lowercase().contains("symbol") {
            continue;
        }
        let modified_at_secs = if let Some(&ts) = note_timestamps.get(&node.uid) {
            ts
        } else if let Some(note_uid) = section_note_map.get(&node.uid) {
            note_timestamps.get(note_uid).copied().unwrap_or(0.0)
        } else {
            0.0
        };
        if modified_at_secs <= 0.0 {
            continue;
        }
        let age_secs = (now - modified_at_secs).max(0.0);
        let boost = 1.0 + recency_weight * (-(age_secs * ln2) / half_life_secs).exp();
        node.relevance *= boost;
    }

    nodes.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn budgeted_cut(
    nodes: &[nestweaver_engine::BrainNode],
    budget: usize,
    concise: bool,
) -> (usize, usize) {
    let mut used = 0usize;
    let mut taken = 0usize;
    for n in nodes {
        let cost = render_cost(n, concise);
        if used + cost > budget {
            break;
        }
        used += cost;
        taken += 1;
    }
    (taken, used)
}

fn render_cost(n: &nestweaver_engine::BrainNode, concise: bool) -> usize {
    if concise {
        // Concise renderers emit only {kind, title, location} (brain_context
        // omits location too, but one conservative model keeps this simple).
        (n.title.len() + n.kind.len() + n.location.len() + 50).div_ceil(4)
    } else {
        // UID + title + kind + location + relevance (~10 chars) + JSON overhead
        (n.uid.len() + n.title.len() + n.kind.len() + n.location.len() + 10 + 80).div_ceil(4)
    }
}

// ── 2. brain_search ─────────────────────────────────────────────────────────

const BRAIN_SEARCH_MAX_PER_KIND: usize = 1_000;
const BRAIN_SEARCH_COUNT_CANDIDATES: usize = 10_000;

fn combine_search_totals(notes: SearchTotal, symbols: SearchTotal) -> SearchTotal {
    let value = notes.value.saturating_add(symbols.value);
    if notes.relation == SearchTotalRelation::Exact
        && symbols.relation == SearchTotalRelation::Exact
    {
        SearchTotal::exact(value)
    } else {
        SearchTotal::lower_bound(value)
    }
}

fn search_results_are_truncated(total: SearchTotal, returned: usize) -> bool {
    total.relation == SearchTotalRelation::LowerBound || returned < total.value
}

fn search_total_relation_label(relation: SearchTotalRelation) -> &'static str {
    match relation {
        SearchTotalRelation::Exact => "eq",
        SearchTotalRelation::LowerBound => "gte",
    }
}

fn authorized_symbol_total(
    global: SearchTotal,
    fetched_candidates: usize,
    authorized_entities: usize,
) -> SearchTotal {
    if global.relation == SearchTotalRelation::Exact && global.value == fetched_candidates {
        SearchTotal::exact(authorized_entities)
    } else {
        SearchTotal::lower_bound(authorized_entities)
    }
}

fn tool_schema_brain_search() -> Value {
    json!({
        "name": "brain_search",
        "description": "Find notes, headings, sections, tags, and code symbols by keyword or phrase using BM25 full-text search.\n\nGuidelines:\n- Use for keyword/phrase lookup; for structural context ('what's connected to X') use brain_context instead\n- Returns both notes and code symbols in a single call, with UIDs for follow-up queries\n- Use response_format 'concise' for scanning many results; limit is applied per-kind\n- total_matches counts distinct note/tag and symbol entities independently of the display limit; total_matches_relation 'gte' marks a stable lower bound from bounded counting\n- returned_matches is the actual response length, and truncated is true for every lower bound or when fewer rows are returned than total_matches\n\nLimitations:\n- Does not read full note bodies — use note_get after finding the note here\n- Falls back to substring matching when the Tantivy BM25 index is unavailable",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Free-text query — natural language works. Example: \"database migration\" or \"AuthService\"."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return. Default 20. Set lower for focused lookups, higher for broad discovery.",
                    "default": 20,
                    "minimum": 1,
                    "maximum": 1000
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns stable entity UIDs, titles, and kinds; \"detailed\" (default) adds section text excerpts, BM25 scores, and location metadata."
                },
                "include_bodies": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true (detailed mode only), embed each high-relevance hit's source body inline (under `inline_body`) so you can skip a follow-up note_get / read. Only hits whose normalized score clears the configured threshold (default 0.75) get a body. Default false."
                },
                "root": {
                    "type": "string",
                    "description": "Filesystem root used to read symbol source spans for inline bodies. Defaults to the server's working directory. Only relevant with include_bodies=true."
                },
                "prf": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, run pseudo-relevance-feedback query expansion: mine high-IDF terms from the top hits and re-run BM25 with them down-weighted. Improves recall on natural-language queries. Mined terms are returned under `expansion_terms`. Default false."
                },
                "rerank": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true (detailed mode only), rerank the top-N hits before truncation (Feature F17). OFF by default; output is byte-identical when off. The default scorer is a transparent MONOTONIC heuristic — an UNVALIDATED reordering, NOT a proven nDCG win. An optional learned-weights file `<db>.rerank.json` is used instead if present and version-matched, but a learned model should only be trusted after the eval harness + accumulated interaction labels gate it at >= 5% nDCG@10. Reranking only reorders an already-retrieved set; recall is unchanged. Default false."
                }
            },
            "required": ["query"]
        }
    })
}

fn tool_brain_search(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    args: Value,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    let raw_query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'query' must be a string"))?
        .to_string();
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(20)
        // Schema validation rejects out-of-range MCP calls; keep a defensive
        // clamp for direct unit/internal calls that bypass dispatch validation.
        .clamp(1, BRAIN_SEARCH_MAX_PER_KIND);
    let concise = is_concise(&args);

    // Expand the query with taxonomy aliases for better recall.
    let db_path = current_db_path(store).unwrap_or_default();
    let aliases = load_alias_sidecar(&db_path);
    let query = expand_query_with_aliases(&raw_query, &aliases);

    // Feature F7 (PRF half): opt in to pseudo-relevance-feedback query
    // expansion via `prf: true`. Off by default.
    let prf = args.get("prf").and_then(|v| v.as_bool()).unwrap_or(false);

    // ── Vault note results ──────────────────────────────────────────────

    let mut expansion_terms: Vec<String> = Vec::new();
    let (grouped_notes, engine) = if let Some(idx) = tantivy {
        // Tantivy BM25 path (preferred).
        let (hits, total) = if prf {
            let page = idx
                .search_prf_page(
                    &query,
                    BRAIN_SEARCH_COUNT_CANDIDATES,
                    nestweaver_engine::query::nestweaver_store_stoplist(),
                )
                .map_err(|e| anyhow!("tantivy prf search: {e}"))?;
            expansion_terms = page.expansion_terms;
            (page.hits, page.total)
        } else {
            let page = idx
                .search_page(&query, BRAIN_SEARCH_COUNT_CANDIDATES)
                .map_err(|e| anyhow!("tantivy search: {e}"))?;
            (page.hits, page.total)
        };
        let results = group_search_hits_by_note(store, &hits, total, limit, concise)?;
        (results, "bm25")
    } else {
        // Substring fallback: search note titles, heading text, and section bodies.
        let needle = query.to_lowercase();

        struct RawHit {
            kind: String,
            title: String,
            note_uid: String,
            score: f32,
        }

        let mut raw_hits: Vec<RawHit> = Vec::new();

        // Note title matches.
        let notes = store.list_notes(None).context("list_notes")?;
        for n in &notes {
            if n.title.to_lowercase().contains(&needle) {
                raw_hits.push(RawHit {
                    kind: "note".to_string(),
                    title: n.title.clone(),
                    note_uid: n.uid.clone(),
                    score: 1.0,
                });
            }
        }

        // Heading text matches.
        let headings = store.list_all_headings().context("list_all_headings")?;
        for h in &headings {
            if h.text.to_lowercase().contains(&needle) {
                raw_hits.push(RawHit {
                    kind: "heading".to_string(),
                    title: h.text.clone(),
                    note_uid: h.note_uid.clone(),
                    score: 0.8,
                });
            }
        }

        // Section body matches.
        let sections = store.list_all_sections().context("list_all_sections")?;
        for s in &sections {
            if s.text_content.to_lowercase().contains(&needle) {
                let title = s
                    .heading_uid
                    .as_deref()
                    .and_then(|h_uid| store.lookup_heading(h_uid).ok())
                    .map(|h| h.text)
                    .unwrap_or_else(|| "(untitled section)".to_string());
                raw_hits.push(RawHit {
                    kind: "section".to_string(),
                    title,
                    note_uid: s.note_uid.clone(),
                    score: 0.6,
                });
            }
        }

        // Group by parent note.
        use std::collections::HashMap;
        struct NoteGroup {
            note_uid: String,
            best_score: f32,
            best_title: String,
            matched_headings: Vec<String>,
        }
        let mut groups: HashMap<String, NoteGroup> = HashMap::new();
        let mut note_order: Vec<String> = Vec::new();

        for hit in &raw_hits {
            let group = groups.entry(hit.note_uid.clone()).or_insert_with(|| {
                note_order.push(hit.note_uid.clone());
                NoteGroup {
                    note_uid: hit.note_uid.clone(),
                    best_score: hit.score,
                    best_title: String::new(),
                    matched_headings: Vec::new(),
                }
            });
            if hit.score > group.best_score {
                group.best_score = hit.score;
            }
            if hit.kind == "note" {
                group.best_title = hit.title.clone();
            }
            if hit.kind == "heading" || hit.kind == "section" {
                group.matched_headings.push(hit.title.clone());
            }
        }

        for group in groups.values_mut() {
            if group.best_title.is_empty() {
                group.best_title = store
                    .lookup_note(&group.note_uid)
                    .map(|n| n.title)
                    .unwrap_or_else(|_| {
                        if group.note_uid.starts_with("tag:") {
                            store
                                .lookup_tag(&group.note_uid)
                                .map(|t| t.name)
                                .unwrap_or_else(|_| group.note_uid.clone())
                        } else {
                            group.note_uid.clone()
                        }
                    });
            }
        }

        note_order.sort_by(|a, b| {
            let sa = groups.get(a).map(|g| g.best_score).unwrap_or(0.0);
            let sb = groups.get(b).map(|g| g.best_score).unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let matched_entities = groups.len();
        let rows: Vec<Value> = note_order
            .iter()
            .take(limit)
            .filter_map(|nuid| groups.get(nuid))
            .map(|g| {
                if concise {
                    json!({
                        "uid": g.note_uid,
                        "kind": "note",
                        "title": g.best_title,
                        "matched_headings": g.matched_headings,
                    })
                } else {
                    json!({
                        "uid": g.note_uid,
                        "kind": "note",
                        "title": g.best_title,
                        "score": g.best_score,
                        "matched_headings": g.matched_headings,
                    })
                }
            })
            .collect();

        (
            GroupedNoteResults {
                rows,
                total: SearchTotal::exact(matched_entities),
            },
            "substring",
        )
    };
    let note_total = grouped_notes.total;
    let mut note_results = grouped_notes.rows;

    // ── Code symbol results ─────────────────────────────────────────────

    let restricted_repos = match visible {
        Some(nestweaver_engine::authz::VisibleRepos::Only(repos)) => Some(repos),
        None | Some(nestweaver_engine::authz::VisibleRepos::All) => None,
    };
    let symbol_fetch_limit = if restricted_repos.is_some() {
        BRAIN_SEARCH_COUNT_CANDIDATES
    } else {
        limit
    };
    let symbol_page =
        search_symbols_page(store, &query, symbol_fetch_limit).context("search code symbols")?;
    let fetched_symbol_candidates = symbol_page.results.len();
    let mut code_hits = symbol_page.results;
    if let Some(repos) = restricted_repos {
        code_hits.retain(|candidate| {
            store
                .lookup_symbol(&candidate.uid)
                .ok()
                .is_some_and(|symbol| {
                    !symbol.repo_uid.trim().is_empty() && repos.contains(&symbol.repo_uid)
                })
        });
    }
    let symbol_total = if restricted_repos.is_some() {
        authorized_symbol_total(
            symbol_page.total,
            fetched_symbol_candidates,
            code_hits.len(),
        )
    } else {
        symbol_page.total
    };

    // Titles never define identity. A note and a code symbol with the same
    // display title remain distinct entities and distinct rows.
    for sym in code_hits.iter().take(limit) {
        let location = format!("{}:{}", sym.file_path, sym.start_line);
        let kind = format!("Symbol/{}", sym.kind);
        let mut row = if concise {
            json!({
                "uid": sym.uid,
                "kind": kind,
                "title": sym.name,
                "location": location,
            })
        } else {
            json!({
                "uid": sym.uid,
                "kind": kind,
                "title": sym.name,
                "score": 0.5,
                "location": location,
            })
        };
        if let Some(canonical_id) = &sym.canonical_id {
            row["canonical_id"] = json!(canonical_id);
        }
        note_results.push(row);
    }

    // Stable sort by score descending so notes and symbols interleave by relevance.
    note_results.sort_by(|a, b| {
        let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Feature F17: rerank the top-N before truncation. OFF by default →
    // byte-identical output. Detailed mode only (concise rows intentionally
    // omit scores used by the reranker). The default scorer is a transparent monotonic
    // heuristic, NOT a validated nDCG win; an optional `<db>.rerank.json`
    // learned-weights file is used if present and version-matched. Reranking
    // only reorders an already-retrieved set; recall is unchanged.
    let do_rerank = args
        .get("rerank")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if do_rerank && !concise {
        // Build BrainNodes mirroring the JSON rows (UID-keyed; rows without a
        // UID — none in detailed mode — are dropped from the reorder set).
        let nodes: Vec<nestweaver_engine::BrainNode> = note_results
            .iter()
            .filter_map(|v| {
                let uid = v.get("uid").and_then(|u| u.as_str())?;
                Some(nestweaver_engine::BrainNode {
                    uid: uid.to_string(),
                    kind: v
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    title: String::new(),
                    location: v
                        .get("location")
                        .and_then(|l| l.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    relevance: v.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0),
                    inline_body: None,
                    body_complete: true,
                })
            })
            .collect();
        if nodes.len() == note_results.len() {
            let mut nodes = nodes;
            let reranker = nestweaver_engine::select_reranker(Some(&db_path));
            nestweaver_engine::rerank(
                &mut nodes,
                reranker.as_ref(),
                store,
                nestweaver_engine::RERANK_DEFAULT_TOP_N,
            );
            // Reorder note_results to match the reranked UID order.
            let mut by_uid: std::collections::HashMap<String, Value> = note_results
                .drain(..)
                .filter_map(|v| {
                    v.get("uid")
                        .and_then(|u| u.as_str())
                        .map(|u| (u.to_string(), v.clone()))
                })
                .collect();
            note_results = nodes.iter().filter_map(|n| by_uid.remove(&n.uid)).collect();
        }
    }

    // Feature F6: per-path `[ranking]` priors from the daemon-installed
    // InstanceConfig (set via `set_current_instance_config`). Mirrors the
    // direct-disk CLI handler: project each row into a `BrainNode`, apply
    // multiplicative priors keyed by file-path glob, then fold the adjusted
    // relevance back into the row's `score`. No config → no-op (byte-identical
    // output to the pre-F6 path).
    if !concise
        && let Some(cfg) = current_instance_config()
        && !cfg.ranking.is_empty()
    {
        let mut probe: Vec<nestweaver_engine::BrainNode> = Vec::new();
        for v in &note_results {
            let uid = v.get("uid").and_then(|u| u.as_str()).unwrap_or("");
            if uid.is_empty() {
                continue;
            }
            let kind = v
                .get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                .to_string();
            let score = v.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
            // For note rows the JSON carries no `location`; resolve the note's
            // file_path so the ranking-glob can match. Symbol rows already
            // carry `"location": "<path>:<line>"`.
            let location = v
                .get("location")
                .and_then(|l| l.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    store
                        .lookup_note(uid)
                        .map(|note| note.file_path)
                        .unwrap_or_default()
                });
            probe.push(nestweaver_engine::BrainNode {
                uid: uid.to_string(),
                kind,
                title: String::new(),
                location,
                relevance: score,
                inline_body: None,
                body_complete: true,
            });
        }
        nestweaver_engine::apply_ranking_priors(&mut probe, &cfg.ranking);
        let adjusted: std::collections::HashMap<String, f64> =
            probe.into_iter().map(|n| (n.uid, n.relevance)).collect();
        for item in note_results.iter_mut() {
            if let Some(uid) = item.get("uid").and_then(|u| u.as_str()).map(String::from)
                && let Some(&adj) = adjusted.get(&uid)
            {
                item["score"] = json!(adj);
            }
        }
        // Re-sort after priors mutate scores so the highest-ranked rows
        // surface first within the merged list.
        note_results.sort_by(|a, b| {
            let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // Bug C / symbol-parity fix: `limit` is interpreted per-kind. Notes
    // (capped at `limit` in `group_search_hits_by_note` / substring `.take`)
    // and symbols (capped at `limit` from `search_symbols_page`) are each bounded
    // upstream, so we deliberately skip a cross-kind truncate here. A merged
    // cap would evict every symbol whenever ≥ `limit` notes match because
    // symbols carry a fixed 0.5 score while BM25 notes score 15+. Callers
    // that need a hard total cap should pass a smaller `limit`.

    // Feature F8: embed high-relevance bodies inline when opted in. Off by
    // default. Concise mode carries no score, so inline bodies are skipped
    // there. Bodies are computed via the shared engine helper for parity with
    // brain_context (normalized-relevance threshold + per-body truncation).
    let include_bodies = args
        .get("include_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if include_bodies && !concise {
        let response_config = configured_response();
        let root = args
            .get("root")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        let mut nodes: Vec<nestweaver_engine::BrainNode> = note_results
            .iter()
            .filter_map(|v| {
                let uid = v.get("uid").and_then(|u| u.as_str())?;
                let score = v.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
                Some(nestweaver_engine::BrainNode {
                    uid: uid.to_string(),
                    kind: v
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    title: String::new(),
                    location: String::new(),
                    relevance: score,
                    inline_body: None,
                    body_complete: true,
                })
            })
            .collect();
        let reader_resolver = inline_body_reader_resolver(store);
        populate_inline_bodies(
            store,
            &mut nodes,
            &root,
            response_config.inline_body_threshold,
            response_config.inline_max_body_tokens,
            None,
            reader_resolver.as_deref(),
        );
        let bodies: std::collections::HashMap<String, String> = nodes
            .into_iter()
            .filter_map(|n| n.inline_body.map(|b| (n.uid, b)))
            .collect();
        for item in note_results.iter_mut() {
            if let Some(uid) = item.get("uid").and_then(|u| u.as_str())
                && let Some(body) = bodies.get(uid)
            {
                item["inline_body"] = json!(body);
            }
        }
    }

    let total = combine_search_totals(note_total, symbol_total);
    let returned_matches = note_results.len();
    let truncated = search_results_are_truncated(total, returned_matches);
    let mut response = json!({
        "query": query,
        "engine": engine,
        "results": note_results,
        "total_matches": total.value,
        "total_matches_relation": search_total_relation_label(total.relation),
        "returned_matches": returned_matches,
        "truncated": truncated,
    });
    if engine == "substring" {
        response["engine_warning"] = json!(
            "tantivy_unavailable: BM25 index could not be opened (another process may hold the writer lock, or it has not been built yet). Results are substring matches only. Run `nestweaver brain reindex-search` to build the index."
        );
    }
    // Feature F7: surface the PRF-mined expansion terms for auditing.
    if !expansion_terms.is_empty() {
        response["expansion_terms"] = json!(expansion_terms);
    }
    Ok(response)
}

/// Group BM25 search hits by parent Note.
///
/// For each note, picks the highest-scoring hit and collects matched
/// heading/section titles. Returns at most `limit` note-level results
/// sorted by best score.
struct GroupedNoteResults {
    rows: Vec<Value>,
    total: SearchTotal,
}

fn group_search_hits_by_note(
    store: &GraphStore,
    hits: &[nestweaver_store::SearchHit],
    total: SearchTotal,
    limit: usize,
    concise: bool,
) -> Result<GroupedNoteResults, anyhow::Error> {
    use std::collections::HashMap;

    struct NoteGroup {
        note_uid: String,
        best_score: f32,
        best_title: String,
        vault_uid: String,
        file_path: String,
        matched_headings: Vec<String>,
    }

    let mut groups: HashMap<SearchLogicalIdentity, NoteGroup> = HashMap::new();
    let mut note_order: Vec<SearchLogicalIdentity> = Vec::new();

    for h in hits {
        // Use the exact validated indexed identity used by Task 5's logical
        // count collector. Graph lookups can drift independently and must not
        // split one indexed note into fragment-UID presentation groups.
        let identity = h
            .logical_identity()
            .map_err(|error| anyhow!("invalid counted search hit identity: {error}"))?;
        let entity_uid = match &identity {
            SearchLogicalIdentity::Note(uid) | SearchLogicalIdentity::Standalone { uid, .. } => {
                uid.clone()
            }
        };

        let group = groups.entry(identity.clone()).or_insert_with(|| {
            note_order.push(identity);
            let file_path = store
                .lookup_note(&entity_uid)
                .map(|n| n.file_path)
                .unwrap_or_default();
            NoteGroup {
                note_uid: entity_uid,
                best_score: 0.0,
                best_title: String::new(),
                vault_uid: h.vault_uid.clone(),
                file_path,
                matched_headings: Vec::new(),
            }
        });

        if h.score > group.best_score {
            group.best_score = h.score;
        }
        if h.kind == "note" {
            group.best_title = h.title.clone();
        }
        if h.kind == "heading" || h.kind == "section" {
            group.matched_headings.push(h.title.clone());
        }
    }

    // For groups that had no direct note title match, look up the note title.
    // Tag UIDs (tag:...) won't resolve via lookup_note — resolve the tag name
    // from the store (the last UID segment is a content hash, not the name).
    for group in groups.values_mut() {
        if group.best_title.is_empty() {
            group.best_title = store
                .lookup_note(&group.note_uid)
                .map(|n| n.title)
                .unwrap_or_else(|_| {
                    if group.note_uid.starts_with("tag:") {
                        store
                            .lookup_tag(&group.note_uid)
                            .map(|t| t.name)
                            .unwrap_or_else(|_| group.note_uid.clone())
                    } else {
                        group.note_uid.clone()
                    }
                });
        }
    }

    // Sort by best_score descending, then take `limit`.
    note_order.sort_by(|a, b| {
        let sa = groups.get(a).map(|g| g.best_score).unwrap_or(0.0);
        let sb = groups.get(b).map(|g| g.best_score).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let rows = note_order
        .iter()
        .take(limit)
        .filter_map(|nuid| groups.get(nuid))
        .map(|g| {
            if concise {
                json!({
                    "uid": g.note_uid,
                    "kind": "note",
                    "title": g.best_title,
                    "matched_headings": g.matched_headings,
                })
            } else {
                json!({
                    "uid": g.note_uid,
                    "kind": "note",
                    "title": g.best_title,
                    "score": g.best_score,
                    "location": g.file_path,
                    "vault_uid": g.vault_uid,
                    "matched_headings": g.matched_headings,
                })
            }
        })
        .collect();
    Ok(GroupedNoteResults { rows, total })
}

// ── 3. note_get ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod brain_search_total_contract_tests {
    use super::*;
    use nestweaver_engine::authz::VisibleRepos;
    use nestweaver_schema::{
        Heading, Note, NoteKind, Section, Symbol, SymbolKind, Tag, Visibility,
        uid::{canonical_symbol_id, repo_uid, symbol_uid},
    };
    use nestweaver_store::tantivy_index::{SearchTotal, SearchTotalRelation};

    const QUERY: &str = "searchneedle";

    fn note(uid: &str, title: &str) -> Note {
        Note {
            uid: uid.to_string(),
            vault_uid: "vlt:search".to_string(),
            file_path: format!("{uid}.md"),
            title: title.to_string(),
            note_kind: NoteKind::General,
            word_count: 20,
            content_hash: format!("hash-{uid}"),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        }
    }

    fn heading(uid: &str, note_uid: &str, text: &str) -> Heading {
        Heading {
            uid: uid.to_string(),
            note_uid: note_uid.to_string(),
            level: 2,
            text: text.to_string(),
            slug: uid.to_string(),
            start_line: 2,
            end_line: 4,
            content_hash: format!("hash-{uid}"),
            embedding: None,
        }
    }

    fn section(uid: &str, note_uid: &str, heading_uid: &str, text: &str) -> Section {
        Section {
            uid: uid.to_string(),
            note_uid: note_uid.to_string(),
            heading_uid: Some(heading_uid.to_string()),
            start_line: 3,
            end_line: 4,
            text_hash: format!("hash-{uid}"),
            text_content: text.to_string(),
            word_count: 5,
            pagerank_score: None,
        }
    }

    fn symbol(uid: &str, repo_uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: format!("src/{uid}.rs"),
            start_line: 1,
            end_line: 2,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("hash-{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn search_fixture() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        for n in [
            note("note:fragment-rich", "SearchneedleShared"),
            note("note:second", "Second Searchneedle Note"),
        ] {
            store.insert_note(&n).unwrap();
        }
        for h in [
            heading(
                "heading:first",
                "note:fragment-rich",
                "Searchneedle First Heading",
            ),
            heading(
                "heading:second",
                "note:fragment-rich",
                "Searchneedle Second Heading",
            ),
        ] {
            store.insert_heading(&h).unwrap();
        }
        for s in [
            section(
                "section:first",
                "note:fragment-rich",
                "heading:first",
                "searchneedle orchid migration details",
            ),
            section(
                "section:second",
                "note:fragment-rich",
                "heading:second",
                "searchneedle quartz rollout details",
            ),
        ] {
            store.insert_section(&s).unwrap();
        }
        for s in [
            symbol("sym:shared-title", "repo:visible", "SearchneedleShared"),
            symbol("sym:second", "repo:visible", "SearchneedleBeta"),
            symbol("sym:hidden", "repo:hidden", "SearchneedleGamma"),
        ] {
            store.insert_symbol(&s).unwrap();
        }
        store
    }

    fn index_fixture(index: &TantivyIndex) {
        index
            .update_note(
                "note:fragment-rich",
                "SearchneedleShared",
                "vlt:search",
                &["searchneedle orchid quartz migration rollout".to_string()],
                &[
                    (
                        "heading:first".to_string(),
                        "Searchneedle First Heading".to_string(),
                    ),
                    (
                        "heading:second".to_string(),
                        "Searchneedle Second Heading".to_string(),
                    ),
                ],
                &[
                    (
                        "section:first".to_string(),
                        "searchneedle orchid migration details".to_string(),
                        "Searchneedle First Heading".to_string(),
                    ),
                    (
                        "section:second".to_string(),
                        "searchneedle quartz rollout details".to_string(),
                        "Searchneedle Second Heading".to_string(),
                    ),
                ],
                &[],
            )
            .unwrap();
        index
            .update_note(
                "note:second",
                "Second Searchneedle Note",
                "vlt:search",
                &["searchneedle cedar deployment guide".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
    }

    #[test]
    fn brain_search_combines_exact_and_lower_bound_domain_totals() {
        let cases = [
            (
                SearchTotal::exact(4),
                SearchTotal::exact(3),
                7,
                SearchTotalRelation::Exact,
            ),
            (
                SearchTotal::lower_bound(4),
                SearchTotal::exact(3),
                7,
                SearchTotalRelation::LowerBound,
            ),
            (
                SearchTotal::exact(4),
                SearchTotal::lower_bound(3),
                7,
                SearchTotalRelation::LowerBound,
            ),
            (
                SearchTotal::lower_bound(4),
                SearchTotal::lower_bound(3),
                7,
                SearchTotalRelation::LowerBound,
            ),
        ];

        for (notes, symbols, expected_value, expected_relation) in cases {
            let combined = combine_search_totals(notes, symbols);
            assert_eq!(combined.value, expected_value);
            assert_eq!(combined.relation, expected_relation);
        }

        assert_eq!(
            combine_search_totals(SearchTotal::exact(usize::MAX), SearchTotal::exact(1)).value,
            usize::MAX
        );
    }

    #[test]
    fn brain_search_truncation_respects_relation_and_returned_count() {
        assert!(!search_results_are_truncated(SearchTotal::exact(4), 4));
        assert!(search_results_are_truncated(SearchTotal::exact(4), 3));
        assert!(search_results_are_truncated(SearchTotal::lower_bound(4), 4));
        assert_eq!(
            search_total_relation_label(SearchTotalRelation::Exact),
            "eq"
        );
        assert_eq!(
            search_total_relation_label(SearchTotalRelation::LowerBound),
            "gte"
        );
    }

    #[test]
    fn brain_search_total_is_limit_independent_and_collapses_note_fragments() {
        let store = search_fixture();
        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_fixture(&index);

        let small = dispatch(
            &store,
            Some(&index),
            "brain_search",
            json!({ "query": QUERY, "limit": 1 }),
            None,
        )
        .unwrap();
        let large = dispatch(
            &store,
            Some(&index),
            "brain_search",
            json!({ "query": QUERY, "limit": 10 }),
            None,
        )
        .unwrap();

        assert_eq!(small["total_matches"], large["total_matches"]);
        assert_eq!(small["total_matches"], json!(5));
        assert_eq!(small["total_matches_relation"], "eq");
        assert_eq!(large["total_matches_relation"], "eq");
        assert!(
            small["returned_matches"].as_u64().unwrap()
                < large["returned_matches"].as_u64().unwrap()
        );
        assert_eq!(small["returned_matches"], json!(2));
        assert_eq!(large["returned_matches"], json!(5));
        assert_eq!(small["truncated"], json!(true));
        assert_eq!(large["truncated"], json!(false));

        let rows = large["results"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .filter(|row| row["uid"] == "note:fragment-rich")
                .count(),
            1,
            "one note row must represent every matching heading/section fragment"
        );
        let shared_title: Vec<&Value> = rows
            .iter()
            .filter(|row| row["title"] == "SearchneedleShared")
            .collect();
        assert_eq!(shared_title.len(), 2);
        assert!(shared_title.iter().any(|row| row["kind"] == "note"));
        assert!(
            shared_title
                .iter()
                .any(|row| row["kind"].as_str().unwrap().starts_with("Symbol/"))
        );
    }

    #[test]
    fn brain_search_concise_rows_keep_stable_uids_without_detailed_scores() {
        let store = search_fixture();
        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_fixture(&index);

        for tantivy in [None, Some(&index)] {
            let result = dispatch(
                &store,
                tantivy,
                "brain_search",
                json!({
                    "query": QUERY,
                    "limit": 10,
                    "response_format": "concise"
                }),
                None,
            )
            .unwrap();
            let rows = result["results"].as_array().unwrap();
            assert!(!rows.is_empty());
            assert!(
                rows.iter()
                    .all(|row| row["uid"].as_str().is_some_and(|uid| !uid.is_empty())),
                "every concise row must preserve its canonical entity UID: {result}"
            );
            assert!(
                rows.iter().all(|row| row.get("score").is_none()),
                "concise presentation must not gain detailed scores: {result}"
            );
        }
    }

    #[test]
    fn brain_search_symbol_rows_carry_canonical_id_in_both_formats() {
        let store = GraphStore::in_memory().unwrap();
        let repo_url = "https://github.com/acme/api";
        let file_path = "src/search.rs";
        let name = "CanonicalNeedle";
        let repo = repo_uid("local", repo_url);
        let uid = symbol_uid(&repo, file_path, name, 7);
        let canonical = canonical_symbol_id(repo_url, file_path, name, "module::CanonicalNeedle");
        let mut searched = symbol(&uid, &repo, name);
        searched.file_path = file_path.to_string();
        searched.start_line = 7;
        searched.canonical_id = Some(canonical.clone());
        store.insert_symbol(&searched).unwrap();

        for response_format in [None, Some("concise")] {
            let mut args = json!({ "query": "CanonicalNeedle", "limit": 10 });
            if let Some(response_format) = response_format {
                args["response_format"] = json!(response_format);
            }
            let result = dispatch(&store, None, "brain_search", args, None).unwrap();
            let row = result["results"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["uid"] == uid)
                .expect("symbol row");

            assert_eq!(row["canonical_id"], canonical);
            if response_format.is_some() {
                assert!(row.get("score").is_none());
            }
        }
    }

    #[test]
    fn brain_search_groups_missing_fragments_by_indexed_owner_and_keeps_tag_distinct() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&note("note:index-owner", "Indexed Owner"))
            .unwrap();
        store
            .insert_tag(&Tag {
                uid: "tag:searchneedle".to_string(),
                vault_uid: "vlt:search".to_string(),
                name: "searchneedle".to_string(),
            })
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open_or_create(dir.path()).unwrap();
        index.reindex_from_store(&store).unwrap();
        index
            .update_note(
                "note:index-owner",
                "Indexed Owner",
                "vlt:search",
                &["unrelated body".to_string()],
                &[(
                    "heading:missing".to_string(),
                    "searchneedle heading".to_string(),
                )],
                &[(
                    "section:missing".to_string(),
                    "searchneedle section".to_string(),
                    "Missing heading".to_string(),
                )],
                &[],
            )
            .unwrap();

        let result = dispatch(
            &store,
            Some(&index),
            "brain_search",
            json!({ "query": QUERY, "limit": 10 }),
            None,
        )
        .unwrap();

        assert_eq!(result["total_matches"], json!(2));
        assert_eq!(result["total_matches_relation"], "eq");
        assert_eq!(result["returned_matches"], json!(2));
        assert_eq!(result["truncated"], json!(false));
        let uids: HashSet<&str> = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["uid"].as_str())
            .collect();
        assert_eq!(
            uids,
            HashSet::from(["note:index-owner", "tag:searchneedle"])
        );
    }

    #[test]
    fn brain_search_substring_fallback_counts_full_note_groups() {
        let store = search_fixture();
        let small = dispatch(
            &store,
            None,
            "brain_search",
            json!({ "query": QUERY, "limit": 1 }),
            None,
        )
        .unwrap();
        let large = dispatch(
            &store,
            None,
            "brain_search",
            json!({ "query": QUERY, "limit": 10 }),
            None,
        )
        .unwrap();

        assert_eq!(small["engine"], "substring");
        assert_eq!(small["total_matches"], json!(5));
        assert_eq!(small["total_matches"], large["total_matches"]);
        assert_eq!(small["total_matches_relation"], "eq");
        assert_eq!(large["returned_matches"], json!(5));
    }

    #[test]
    fn brain_search_prf_uses_counted_final_query_totals() {
        let store = search_fixture();
        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_fixture(&index);

        let small = dispatch(
            &store,
            Some(&index),
            "brain_search",
            json!({ "query": QUERY, "limit": 1, "prf": true }),
            None,
        )
        .unwrap();
        let large = dispatch(
            &store,
            Some(&index),
            "brain_search",
            json!({ "query": QUERY, "limit": 10, "prf": true }),
            None,
        )
        .unwrap();

        assert_eq!(small["engine"], "bm25");
        assert_eq!(small["total_matches"], large["total_matches"]);
        assert_eq!(small["total_matches_relation"], "eq");
        assert!(small["truncated"].as_bool().unwrap());
    }

    #[test]
    fn brain_search_zero_matches_are_exact_and_not_truncated() {
        let store = search_fixture();
        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_fixture(&index);
        for tantivy in [None, Some(&index)] {
            let result = dispatch(
                &store,
                tantivy,
                "brain_search",
                json!({ "query": "absentneedle", "limit": 1 }),
                None,
            )
            .unwrap();
            assert_eq!(result["total_matches"], json!(0));
            assert_eq!(result["total_matches_relation"], "eq");
            assert_eq!(result["returned_matches"], json!(0));
            assert_eq!(result["truncated"], json!(false));
        }
    }

    #[test]
    fn brain_search_visibility_never_counts_hidden_symbols() {
        let store = search_fixture();
        store
            .insert_symbol(&symbol("sym:unknown-owner", "", "SearchneedleUnknown"))
            .unwrap();
        let visible = VisibleRepos::Only(
            ["repo:visible".to_string(), String::new()]
                .into_iter()
                .collect(),
        );
        let result = dispatch_cancellable(
            &store,
            None,
            "brain_search",
            json!({ "query": QUERY, "limit": 10 }),
            None,
            None,
            Some(&visible),
        )
        .unwrap();

        assert_eq!(result["total_matches"], json!(4));
        assert_eq!(result["total_matches_relation"], "eq");
        assert_eq!(result["returned_matches"], json!(4));
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["uid"] != "sym:hidden" && row["uid"] != "sym:unknown-owner")
        );

        assert_eq!(
            authorized_symbol_total(SearchTotal::exact(4), 4, 1),
            SearchTotal::exact(1)
        );
        assert_eq!(
            authorized_symbol_total(SearchTotal::exact(10_001), 10_000, 1),
            SearchTotal::lower_bound(1),
            "a bounded authorized scan must not reveal the global hidden total"
        );
    }

    #[test]
    fn brain_search_schema_bounds_limit_and_documents_total_contract() {
        let schema = tool_schema_brain_search();
        assert_eq!(schema["inputSchema"]["properties"]["limit"]["minimum"], 1);
        assert_eq!(
            schema["inputSchema"]["properties"]["limit"]["maximum"],
            1000
        );
        let description = schema["description"].as_str().unwrap();
        assert!(description.contains("total_matches_relation"));
        assert!(description.contains("returned_matches"));
        assert!(
            schema["inputSchema"]["properties"]["response_format"]["description"]
                .as_str()
                .unwrap()
                .contains("stable entity UIDs")
        );
        assert!(
            validate_tool_arguments("brain_search", &json!({ "query": QUERY, "limit": 0 }))
                .is_err()
        );
        assert!(
            validate_tool_arguments("brain_search", &json!({ "query": QUERY, "limit": 1001 }))
                .is_err()
        );
    }
}

fn tool_schema_note_get() -> Value {
    json!({
        "name": "note_get",
        "description": "Fetch a vault note's full markdown body or specific sections, plus structural metadata (frontmatter, heading outline, tags).\n\nRequires either 'uid' or 'title' (at least one must be provided).\n\nGuidelines:\n- Use after brain_search or brain_context identifies a relevant note\n- Pass uid for unambiguous lookup, or title for case-insensitive first-match\n- Use sections parameter to retrieve only specific heading sections — much more token-efficient for large notes\n\nLimitations:\n- Markdown notes only — for code symbols use read_symbols\n- Not a discovery tool — use brain_search or brain_context to find notes first",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Note UID (e.g. note:vlt:MyVault:abc123). Preferred over title for unambiguous lookup." },
                "title": { "type": "string", "description": "Note title (case-insensitive). Returns the first match if multiple notes share the same title." },
                "include_body": {
                    "type": "boolean",
                    "description": "Include the full markdown body. Default true. Set to false to get only metadata (outline, frontmatter, section count).",
                    "default": true
                },
                "sections": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of heading names. If provided, returns only those sections instead of the full body. Case-insensitive match."
                }
            }
        }
    })
}

fn tool_note_get(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let include_body = args
        .get("include_body")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Parse optional section filter.
    let section_filter: Option<Vec<String>> =
        args.get("sections").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });

    let note = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        store
            .lookup_note(uid)
            .with_context(|| format!("failed to look up note with uid '{uid}'"))?
    } else if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        let mut matches = store
            .lookup_notes_by_title(title)
            .with_context(|| format!("failed to look up notes with title '{title}'"))?;
        // Slug-tolerant fallback: case-insensitive + slug normalization.
        // Uses list_notes_lite to avoid loading full note bodies during scan.
        if matches.is_empty() {
            let needle = title.to_lowercase();
            if let Ok(all_notes) = store.list_notes_lite(None)
                && let Some(hit) = all_notes.iter().find(|n| {
                    n.title.to_lowercase() == needle
                        || slug_normalize(&n.title) == slug_normalize(title)
                })
                && let Ok(note) = store.lookup_note(&hit.uid)
            {
                matches.push(note);
            }
        }
        match matches.into_iter().next() {
            Some(n) => n,
            None => return Err(anyhow!("no note found with title '{title}'")),
        }
    } else {
        return Err(anyhow!("provide either 'uid' or 'title'"));
    };

    // Load all headings and sections (needed for both outline and section filter).
    let headings_raw = store
        .headings_in_note(&note.uid)
        .map_err(|e| anyhow!("headings_in_note: {e}"))?;
    let sections_raw = store
        .sections_in_note(&note.uid)
        .map_err(|e| anyhow!("sections_in_note: {e}"))?;

    // Resolve body: either filtered sections or full file contents.
    let body = if let Some(ref names) = section_filter {
        // Section-filter mode: return only the text_content of sections whose
        // heading matches one of the requested names (case-insensitive).
        let mut parts: Vec<String> = Vec::new();
        for heading in &headings_raw {
            if names.iter().any(|n| heading.text.eq_ignore_ascii_case(n)) {
                // Find the section that belongs to this heading.
                if let Some(sec) = sections_raw
                    .iter()
                    .find(|s| s.heading_uid.as_deref() == Some(&heading.uid))
                {
                    // Reconstruct the section with its heading prefix.
                    let prefix = "#".repeat(heading.level as usize);
                    parts.push(format!("{prefix} {}\n\n{}", heading.text, sec.text_content));
                }
            }
        }
        if parts.is_empty() {
            Some(String::new())
        } else {
            Some(parts.join("\n\n"))
        }
    } else if include_body {
        // Full body mode: load from disk.
        match store.lookup_vault(&note.vault_uid) {
            Ok(vault) => {
                let path = Path::new(&vault.root_path).join(&note.file_path);
                // Defense-in-depth: verify the resolved path stays inside
                // the vault root. Prevents exfiltration via symlinks even
                // if one slipped past the indexer.
                let safe = match (
                    std::fs::canonicalize(&path),
                    std::fs::canonicalize(&vault.root_path),
                ) {
                    (Ok(resolved), Ok(root)) => resolved.starts_with(&root),
                    _ => false,
                };
                if !safe {
                    tracing::warn!(
                        "note_get: resolved path escapes vault root, refusing to read: {}",
                        path.display()
                    );
                    None
                } else {
                    match std::fs::read_to_string(&path) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tracing::warn!("note_get: failed to read {}: {e}", path.display());
                            None
                        }
                    }
                }
            }
            Err(_) => None,
        }
    } else {
        None
    };

    let headings = headings_raw
        .into_iter()
        .map(|h| {
            json!({
                "uid": h.uid,
                "level": h.level,
                "text": h.text,
                "slug": h.slug,
                "line": h.start_line,
            })
        })
        .collect::<Vec<_>>();

    let section_count = sections_raw.len();

    let frontmatter: Value = note
        .frontmatter
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(json!({}));

    Ok(json!({
        "uid": note.uid,
        "title": note.title,
        "path": note.file_path,
        "note_kind": note.note_kind.to_string(),
        "word_count": note.word_count,
        "frontmatter": frontmatter,
        "outline": headings,
        "section_count": section_count,
        "body": body,
    }))
}

// ── 4. backlinks ────────────────────────────────────────────────────────────

fn tool_schema_backlinks() -> Value {
    json!({
        "name": "backlinks",
        "description": "Find every note that wiki-links TO a specific target note, revealing the reverse link graph.\n\nRequires either 'uid' or 'title' (at least one must be provided).\n\nGuidelines:\n- Pass uid or title (case-insensitive, first match) to identify the target\n- Returns source note paths, linking sections, confidence scores, and display text\n- For forward links (what a note links to), read the note body with note_get instead\n\nLimitations:\n- Only considers vault wikilinks, not code symbol dependencies (use brain_impact for those)\n- Confidence reflects link resolution quality, not semantic relevance",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Note UID (e.g. note:vlt:MyVault:abc123). Preferred for unambiguous lookup." },
                "title": { "type": "string", "description": "Note title (case-insensitive match). Returns backlinks for the first matching note." }
            }
        }
    })
}

fn slug_normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn tool_backlinks(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let target_uid = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        uid.to_string()
    } else if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        let mut matches = store.lookup_notes_by_title(title)?;
        // Fallback: case-insensitive + slug normalization.
        // Uses list_notes_lite to avoid loading full note bodies during scan.
        if matches.is_empty() {
            let needle = title.to_lowercase();
            if let Ok(all_notes) = store.list_notes_lite(None)
                && let Some(hit) = all_notes.iter().find(|n| {
                    n.title.to_lowercase() == needle
                        || slug_normalize(&n.title) == slug_normalize(title)
                })
                && let Ok(note) = store.lookup_note(&hit.uid)
            {
                matches.push(note);
            }
        }
        match matches.into_iter().next() {
            Some(n) => n.uid,
            None => return Err(anyhow!("no note found with title '{title}'")),
        }
    } else {
        return Err(anyhow!("provide either 'uid' or 'title'"));
    };

    let backlinks = store
        .wikilink_sources_to_note(&target_uid)
        .context("wikilink_sources_to_note")?;

    let rows: Vec<Value> = backlinks
        .iter()
        .map(|b| {
            json!({
                "source_note_uid": b.source_note_uid,
                "source_note_title": b.source_note_title,
                "source_note_path": b.source_note_path,
                "source_section_uid": b.source_section_uid,
                "confidence": b.confidence,
                "display": b.display,
            })
        })
        .collect();

    Ok(json!({
        "target_uid": target_uid,
        "count": rows.len(),
        "backlinks": rows,
    }))
}

// ── 5. brain_status ─────────────────────────────────────────────────────────

fn tool_schema_brain_status() -> Value {
    json!({
        "name": "brain_status",
        "description": "Show what knowledge sources are indexed: vault/repo counts, note/tag/wikilink totals, staleness warnings, and search engine availability. No parameters required.\n\nGuidelines:\n- Call at session start to verify expected vaults and repos are loaded\n- Surfaces staleness warnings when repos are behind git HEAD\n- If counts are zero, use brain_add_source to index content\n\nLimitations:\n- Metadata-only — does not search content (use brain_search for that)\n- For detailed per-repo staleness, use stale_check\n\nIn server mode, includes additional fields: server_mode, indexing_active, indexing_repo, queue_depth.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

fn tool_brain_status(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
) -> Result<Value, anyhow::Error> {
    let vaults = store
        .list_vaults(None)
        .map_err(|e| anyhow::anyhow!("brain_status: failed to list vaults: {e}"))?;
    let repos = store
        .list_repos(None)
        .map_err(|e| anyhow::anyhow!("brain_status: failed to list repos: {e}"))?;
    let notes = store.count_notes().unwrap_or(0);
    let headings = store.count_headings().unwrap_or(0);
    let sections = store.count_sections().unwrap_or(0);
    let tags = store.count_tags().unwrap_or(0);
    let wikilinks = store.count_wikilink_edges().unwrap_or(0);

    let db_path = match current_db_path(store) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(
                "brain_status: db_path unavailable ({e}), extension store lookups will be skipped"
            );
            None
        }
    };

    let vaults_json: Vec<Value> = vaults
        .iter()
        .map(|v| {
            let notes = store.list_notes(Some(&v.uid)).unwrap_or_default();
            let note_count = notes.len();
            // Prefer the extension-store timestamp (actual indexer run);
            // fall back to max(note.modified_at) for older databases.
            let ext_ts = db_path
                .as_deref()
                .and_then(|p| get_last_indexed_at(p, &v.uid));
            if ext_ts.is_none() {
                tracing::debug!(
                    vault_uid = %v.uid,
                    db_path = ?db_path,
                    "no extension-store timestamp; falling back to max(modified_at)"
                );
            }
            let (last_indexed, last_indexed_source) = if let Some(ts) = ext_ts {
                (Some(ts), "extension_store")
            } else {
                let fallback = notes
                    .iter()
                    .filter_map(|n| n.modified_at.as_deref())
                    .max()
                    .map(|s| s.to_string());
                if fallback.is_some() {
                    (fallback, "file_mtime")
                } else {
                    (None, "none")
                }
            };
            json!({
                // `uid` + `instance_id` let callers disambiguate rows that
                // share a name/root_path (collision state) and target precise
                // operations like `brain remove --instance <id>`.
                "uid": v.uid,
                "instance_id": v.instance_id,
                "name": v.name,
                "root_path": v.root_path,
                "note_count": note_count,
                "last_indexed": last_indexed,
                "last_indexed_source": last_indexed_source,
            })
        })
        .collect();

    // Detect duplicate-root collisions. The CLI's local (non-daemon) path
    // emits these warnings to stderr; forward them through the JSON-RPC
    // response so daemon-routed callers and `--json` consumers see the
    // same diagnostic.
    let mut root_to_rows: std::collections::HashMap<&str, Vec<&nestweaver_schema::Vault>> =
        std::collections::HashMap::new();
    for v in &vaults {
        root_to_rows
            .entry(v.root_path.as_str())
            .or_default()
            .push(v);
    }
    let warnings: Vec<Value> = root_to_rows
        .iter()
        .filter(|(_, rows)| rows.len() > 1)
        .map(|(root, rows)| {
            // Pair each row with its note count so we can both render the
            // entries and pick the keeper for the remediation hint.
            let rows_with_counts: Vec<(&&nestweaver_schema::Vault, usize)> = rows
                .iter()
                .map(|v| {
                    (
                        v,
                        store.list_notes(Some(&v.uid)).unwrap_or_default().len(),
                    )
                })
                .collect();
            let entries: Vec<Value> = rows_with_counts
                .iter()
                .map(|(v, n)| {
                    json!({
                        "uid": v.uid,
                        "instance_id": v.instance_id,
                        "name": v.name,
                        "note_count": n,
                    })
                })
                .collect();

            // Suggest a concrete merge command. The keeper is the row with
            // the highest note_count (most data); ties break on the
            // lexicographically smallest instance_id so the suggestion is
            // deterministic. Emit one command per non-keeper row so callers
            // collapse all ghosts into the canonical instance.
            let keeper = rows_with_counts
                .iter()
                .max_by(|a, b| {
                    a.1.cmp(&b.1)
                        .then_with(|| b.0.instance_id.cmp(&a.0.instance_id))
                })
                .map(|(v, _)| v);
            let (remediation_commands, remediation_hint) = match keeper {
                Some(keeper_v) => {
                    let cmds: Vec<String> = rows_with_counts
                        .iter()
                        .filter(|(v, _)| v.instance_id != keeper_v.instance_id)
                        .map(|(v, _)| {
                            format!(
                                "nestweaver instance merge --from {} --to {}",
                                v.instance_id, keeper_v.instance_id,
                            )
                        })
                        .collect();
                    let hint = format!(
                        "Multiple instance_ids share this vault root. Keep '{}' (has the most data) and merge the others into it using the commands below. Take a snapshot of {} first.",
                        keeper_v.instance_id,
                        db_path
                            .as_deref()
                            .and_then(|p| p.to_str())
                            .unwrap_or("the database"),
                    );
                    (cmds, hint)
                }
                None => (Vec::new(), String::new()),
            };

            json!({
                "kind": "duplicate_vault_root",
                "root_path": root,
                "entries": entries,
                "remediation_commands": remediation_commands,
                "remediation_hint": remediation_hint,
            })
        })
        .collect();
    let repos_json: Vec<Value> = repos
        .iter()
        .map(|r| json!({ "url": r.url, "sha": r.indexed_sha }))
        .collect();

    // P0-3: Surface staleness warnings proactively.
    let mut staleness_warnings: Vec<Value> = Vec::new();
    for repo in &repos {
        if repo.indexed_sha == "local" || repo.indexed_sha.is_empty() {
            staleness_warnings.push(json!({
                "repo": repo.name.as_deref().unwrap_or(&repo.url),
                "warning": "indexed without git tracking — staleness unknown",
                "action": "re-index with git tracking to enable staleness detection"
            }));
            continue;
        }
        // Disk checks only apply to repos with a known local working tree;
        // remote-identity repos without one (root_path: None) are skipped.
        let Some(path) = repo.local_root() else {
            continue;
        };
        let repo_path = std::path::Path::new(path);
        if !repo_path.exists() {
            staleness_warnings.push(json!({
                "repo": repo.name.as_deref().unwrap_or(&repo.url),
                "warning": "path does not exist on disk",
                "action": "run `nestweaver prune-stale` to clean up"
            }));
            continue;
        }
        // Check git HEAD vs indexed SHA — fast (no network), just reads .git/HEAD
        if let Ok(output) = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_path)
            .output()
            && output.status.success()
        {
            let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !head.is_empty() && head != repo.indexed_sha {
                staleness_warnings.push(json!({
                    "repo": repo.name.as_deref().unwrap_or(&repo.url),
                    "indexed_sha": &repo.indexed_sha[..8.min(repo.indexed_sha.len())],
                    "head_sha": &head[..8.min(head.len())],
                    "warning": "index is behind git HEAD",
                    "action": "run `nestweaver index --repo <path>` to update"
                }));
            }
        }
    }

    // Report Tantivy availability so clients can tell whether brain_search
    // will use BM25 or fall back to substring matching.
    let tantivy_available = tantivy.is_some();
    let tantivy_doc_count = tantivy.map(|t| t.doc_count()).unwrap_or(0);

    // Check whether a watcher process holds the lock file.
    let watcher_pid: Option<u32> = db_path.as_deref().and_then(|p| {
        let mut lock = p.as_os_str().to_owned();
        lock.push(".lock");
        let lock_path = std::path::PathBuf::from(lock);
        std::fs::read_to_string(lock_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    });

    // F16: response-cache stats. Correctness is key-based (persisted
    // generation + filemeta scope digest), so the cache reflects the LAST
    // INDEX — same staleness as the graph itself, which is the correct
    // semantic. The session hit-rate below is unproven and should be measured.
    let (cache_size, cache_entries, cache_hit_rate) =
        db_path.as_deref().map(cache_stats).unwrap_or((0, 0, None));
    let cache_hit_rate_pct = cache_hit_rate.map(|r| (r * 100.0).round() as u64);

    Ok(json!({
        "vaults": vaults_json,
        "vault_count": vaults.len(),
        "notes": notes,
        "headings": headings,
        "sections": sections,
        "tags": tags,
        "wikilinks": wikilinks,
        "repos": repos_json,
        "repo_count": repos.len(),
        "server_mode": is_server_mode(),
        "tantivy_available": tantivy_available,
        "tantivy_doc_count": tantivy_doc_count,
        "watcher_pid": watcher_pid,
        // F16 response cache. `hit_rate_pct` is the session hit-rate
        // (hits/(hits+misses)); null until the first cacheable call. The
        // cache's correctness is key-based (persisted graph_generation +
        // filemeta scope digest), so results are consistent with the last
        // index — the same staleness as the graph. The hit-rate is unproven
        // and should be measured in real usage.
        "cache": {
            "size_bytes": cache_size,
            "entries": cache_entries,
            "hit_rate_pct": cache_hit_rate_pct,
        },
        // Structured diagnostics. Each entry describes a vault-level
        // anomaly (duplicate root, missing index, etc.) so clients can
        // render an actionable warning without re-deriving it.
        "warnings": warnings,
        "staleness_warnings": staleness_warnings,
    }))
}

// ── 6. brain_add_source ─────────────────────────────────────────────────────

fn tool_schema_brain_add_source() -> Value {
    json!({
        "name": "brain_add_source",
        "description": "Index a new vault, code repo, or markdown folder into the brain graph. Auto-detects source type from directory contents.\n\nGuidelines:\n- Check brain_status first to avoid re-indexing already-indexed sources\n- Path must be absolute or start with ~/ (tilde expanded to $HOME)\n- Optional name sets a friendly display name for vaults (ignored for repos)\n\nLimitations:\n- Cannot index remote URLs directly — only local filesystem paths\n- Re-indexing an existing source overwrites the previous index",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or ~/-relative directory path to the vault, repo, or markdown folder to index." },
                "name": {
                    "type": "string",
                    "description": "Friendly display name (vaults only). Defaults to the directory name. Has no effect for code repos."
                }
            },
            "required": ["path"]
        }
    })
}

fn tool_brain_add_source(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // Always route through the daemon (start it if needed). This ensures
    // consistent write serialization whether or not the MCP server itself
    // was started in daemon mode.
    //
    // We cannot depend on `nestweaver-client` here because that crate
    // depends on `nestweaver-daemon`, which depends back on this crate
    // (`nestweaver-mcp`). Instead, we inline the minimal socket-path
    // derivation and process-spawn logic that mirrors
    // `nestweaver_client::autostart::ensure_daemon`.
    #[cfg(feature = "daemon")]
    {
        let db_path = current_db_path(store)?;
        let db_path_buf = std::path::PathBuf::from(&db_path);

        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| anyhow::anyhow!("failed to create tokio runtime: {e}"))?;

        let sock_path = inline_ensure_daemon(&db_path_buf)
            .map_err(|e| anyhow::anyhow!("failed to start daemon: {e}"))?;

        let mut client = rt.block_on(inline_connect_daemon(&sock_path))?;

        dispatch_add_source_via_daemon(&mut client, &rt, args)
    }

    // Non-daemon fallback (daemon feature not compiled in).
    #[cfg(not(feature = "daemon"))]
    {
        if !ALLOW_ADD_SOURCES.with(|c| c.get()) {
            return Err(anyhow!(
                "brain_add_source is disabled in --no-daemon mode. \
             Use daemon mode (the default) or pass --allow-mcp-add-sources."
            ));
        }
        let raw_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("'path' must be a string"))?;
        let expanded = expand_tilde(raw_path);
        let path = Path::new(&expanded);
        if !path.exists() {
            return Err(anyhow!("path does not exist: {}", path.display()));
        }
        if !path.is_dir() {
            return Err(anyhow!("path is not a directory: {}", path.display()));
        }
        // SECURITY: refuse paths that contain `..` components after
        // canonicalisation. Stops the MCP caller (or a prompt-injected
        // Claude) from descending into system directories via traversal.
        let canonical =
            std::fs::canonicalize(path).map_err(|e| anyhow!("could not canonicalize path: {e}"))?;
        if canonical
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(anyhow!(
                "path contains '..' components after canonicalisation: {}",
                canonical.display()
            ));
        }

        let has_obsidian = path.join(".obsidian").is_dir();
        // `.exists()`, not `.is_dir()`: in a git worktree `.git` is a FILE
        // pointing at the real gitdir — consistent with every other guard.
        let has_git = path.join(".git").exists();
        let has_any_md = walk_has_markdown(path);

        // Detection priority: Obsidian vault > markdown folder > git repo.
        if has_obsidian || has_any_md {
            let kind = if has_obsidian { "obsidian" } else { "markdown" };
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("vault")
                        .to_string()
                });
            // We need a db_path for index_markdown_directory; but the server
            // already opened one. Reuse it indirectly: call the in-memory
            // primitive? No — that doesn't persist. Reopen the same DB by
            // path. The store doesn't currently expose its underlying path,
            // so we re-index via the public function and accept that the
            // server's open handle and the indexer's open handle are two
            // connections to the same DB.
            let db_path = current_db_path(store)?;
            let result = index_markdown_directory(path, &db_path, "default", &name)
                .context("index vault")?;
            // Record the indexer run timestamp for this vault.
            if let Err(e) = nestweaver_engine::record_last_indexed_at(&db_path, &result.vault_uid) {
                tracing::warn!("failed to record last_indexed_at: {e}");
            }
            return Ok(json!({
                "kind": kind,
                "name": result.vault_name,
                "vault_uid": result.vault_uid,
                "notes": result.notes_count,
                "headings": result.headings_count,
                "sections": result.sections_count,
                "tags": result.tags_count,
                "wikilinks_resolved": result.wikilinks_resolved,
                "wikilinks_unresolved": result.wikilinks_unresolved,
                "skipped_count": result.skipped.len(),
            }));
        }

        if has_git {
            let db_path = current_db_path(store)?;
            // Identity: prefer the git origin remote when configured (used
            // only as an identity string — never fetched); fall back to a
            // file:// URL. The engine persists the disk location as
            // `root_path` on the Repo node.
            let url = nestweaver_engine::mint_repo_identity(path);
            let result =
                index_directory(path, &db_path, "default", &url, "local").context("index repo")?;
            return Ok(json!({
                "kind": "repo",
                "url": url,
                "files": result.files_count,
                "symbols": result.symbols_count,
                "edges": result.edges_count,
                "skipped_count": result.skipped_files.len(),
            }));
        }

        Err(anyhow!(
            "no .md files, no .git/, no .obsidian/ found at {} — nothing to index",
            path.display()
        ))
    } // end #[cfg(not(feature = "daemon"))]
}

// ── 6b. brain_remove_source ─────────────────────────────────────────────────

fn tool_schema_brain_remove_source() -> Value {
    json!({
        "name": "brain_remove_source",
        "description": "Remove an indexed code repository or markdown vault from the brain graph permanently.\n\nGuidelines:\n- Accepts repo name, vault name, filesystem path, file:// URL, or UID\n- Auto-detects whether the target is a repo or vault\n- To re-index (not remove), use brain_add_source instead\n\nLimitations:\n- Removal is permanent — the source must be re-indexed with brain_add_source to restore\n- Ambiguous targets (matching multiple sources) require a UID to disambiguate",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Repo name, vault name, filesystem path, file:// URL, or UID of the source to remove."
                }
            },
            "required": ["target"]
        }
    })
}

/// Match a user-supplied `target` (repo UID, name, filesystem path, `file://`
/// URL, or git-origin URL) against the indexed repos. Shared by the daemon-side
/// `tool_brain_remove_source` and the client-proxy `dispatch_via_daemon` path so
/// both resolve a target to a repo IDENTICALLY — the client proxy used to send
/// the raw target as a `repo_uid`, which never matched a path/name and silently
/// removed nothing (nw-089).
fn match_repo_target<'a>(
    repos: &'a [nestweaver_schema::Repo],
    target: &str,
) -> Vec<&'a nestweaver_schema::Repo> {
    let expand = |input: &str| -> String {
        if let Some(stripped) = input.strip_prefix("~/")
            && let Ok(home) = std::env::var("HOME")
        {
            return format!("{home}/{stripped}");
        }
        input.to_string()
    };
    let target_trimmed = target.trim_end_matches('/');
    let canonical_target = std::fs::canonicalize(expand(target_trimmed))
        .map(|p| format!("file://{}", p.display()))
        .unwrap_or_default();
    let url_target = if target_trimmed.starts_with("file://") {
        target_trimmed.to_string()
    } else if std::path::Path::new(target_trimmed).is_absolute() || target_trimmed.starts_with("~/")
    {
        let expanded = expand(target_trimmed);
        std::fs::canonicalize(&expanded)
            .map(|p| format!("file://{}", p.display()))
            .unwrap_or_else(|_| format!("file://{expanded}"))
    } else {
        String::new()
    };
    // A path target may refer to a repo identified by its git origin remote
    // rather than a file:// URL — try that identity too (read from git config,
    // never fetched).
    let origin_target = std::fs::canonicalize(expand(target_trimmed))
        .ok()
        .filter(|p| p.join(".git").exists())
        .and_then(|p| nestweaver_engine::read_origin_url(&p).ok())
        .unwrap_or_default();
    let canonical_path = canonical_target
        .strip_prefix("file://")
        .unwrap_or_default()
        .to_string();

    repos
        .iter()
        .filter(|r| {
            let r_url = r.url.trim_end_matches('/');
            r.uid == target
                || r.name.as_deref() == Some(target_trimmed)
                || r_url == url_target.trim_end_matches('/')
                || r_url == canonical_target.trim_end_matches('/')
                || (!origin_target.is_empty() && r_url == origin_target.trim_end_matches('/'))
                || (!canonical_path.is_empty()
                    && r.local_root().map(|p| p.trim_end_matches('/'))
                        == Some(canonical_path.trim_end_matches('/')))
                || r_url.ends_with(&format!("/{target_trimmed}"))
        })
        .collect()
}

fn tool_brain_remove_source(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'target' is required"))?
        .to_string();

    // Try to resolve as a repo first, then as a vault.
    let repos = store.list_repos(None)?;
    let canonical_target = std::fs::canonicalize(&target)
        .map(|p| format!("file://{}", p.display()))
        .unwrap_or_default();
    let matched_repo = match_repo_target(&repos, &target);

    if matched_repo.len() == 1 {
        #[cfg(feature = "daemon")]
        {
            let repo = matched_repo[0];
            let repo_uid = repo.uid.clone();
            let display = repo.name.clone().unwrap_or_else(|| repo.url.clone());
            let db_path = current_db_path(store)?;
            let db_path_buf = std::path::PathBuf::from(&db_path);
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| anyhow::anyhow!("failed to create tokio runtime: {e}"))?;
            let sock_path = inline_ensure_daemon(&db_path_buf)
                .map_err(|e| anyhow::anyhow!("failed to start daemon: {e}"))?;
            let mut client = rt.block_on(inline_connect_daemon(&sock_path))?;
            let resp = rt
                .block_on(client.remove_repo(nestweaver_proto::RemoveRepoRequest {
                    repo_uid: repo_uid.clone(),
                }))
                .map_err(|e| anyhow!("remove_repo RPC failed: {e}"))?;
            let inner = resp.into_inner();
            return Ok(json!({
                "kind": "repo",
                "name": display,
                "uid": repo_uid,
                "files_deleted": inner.files_deleted,
                "symbols_deleted": inner.symbols_deleted
            }));
        }
        #[cfg(not(feature = "daemon"))]
        return Err(anyhow!("brain_remove_source requires daemon mode"));
    }

    // Try vaults
    let vaults = store.list_vaults(None)?;
    let matched_vault: Vec<&nestweaver_schema::Vault> = vaults
        .iter()
        .filter(|v| {
            v.uid == target
                || v.name == target
                || v.root_path == target
                || v.root_path == canonical_target.strip_prefix("file://").unwrap_or("")
                || std::fs::canonicalize(&v.root_path)
                    .map(|p| format!("file://{}", p.display()))
                    .unwrap_or_default()
                    == canonical_target
        })
        .collect();

    if matched_vault.len() == 1 {
        #[cfg(feature = "daemon")]
        {
            let vault = matched_vault[0];
            let vault_uid = vault.uid.clone();
            let display = vault.name.clone();
            let db_path = current_db_path(store)?;
            let db_path_buf = std::path::PathBuf::from(&db_path);
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| anyhow::anyhow!("failed to create tokio runtime: {e}"))?;
            let sock_path = inline_ensure_daemon(&db_path_buf)
                .map_err(|e| anyhow::anyhow!("failed to start daemon: {e}"))?;
            let mut client = rt.block_on(inline_connect_daemon(&sock_path))?;
            let resp = rt
                .block_on(client.remove_vault(nestweaver_proto::RemoveVaultRequest {
                    vault_uid: vault_uid.clone(),
                }))
                .map_err(|e| anyhow!("remove_vault RPC failed: {e}"))?;
            let inner = resp.into_inner();
            return Ok(json!({
                "kind": "vault",
                "name": display,
                "uid": vault_uid,
                "notes_deleted": inner.notes_deleted
            }));
        }
        #[cfg(not(feature = "daemon"))]
        return Err(anyhow!("brain_remove_source requires daemon mode"));
    }

    // No match or ambiguous
    if matched_repo.len() > 1 || matched_vault.len() > 1 {
        return Err(anyhow!(
            "'{target}' matches multiple sources. Use a UID to disambiguate."
        ));
    }
    Err(anyhow!("no repo or vault matching '{target}' found"))
}

// ── 6c. prune_stale ─────────────────────────────────────────────────────────

fn tool_schema_prune_stale() -> Value {
    json!({
        "name": "prune_stale",
        "description": "Remove all indexed repos and vaults whose source directories no longer exist on disk. No parameters required.\n\nGuidelines:\n- Use after moving, renaming, or deleting project directories\n- Returns the list of removed repos and vaults\n\nLimitations:\n- Only checks filesystem existence, not content staleness (use stale_check for that)\n- Cannot undo — removed sources must be re-indexed with brain_add_source",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "required": []
        }
    })
}

fn tool_prune_stale(store: &GraphStore) -> Result<Value, anyhow::Error> {
    #[cfg(feature = "daemon")]
    {
        let db_path = current_db_path(store)?;
        let db_path_buf = std::path::PathBuf::from(&db_path);
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| anyhow::anyhow!("failed to create tokio runtime: {e}"))?;
        let sock_path = inline_ensure_daemon(&db_path_buf)
            .map_err(|e| anyhow::anyhow!("failed to start daemon: {e}"))?;
        let mut client = rt.block_on(inline_connect_daemon(&sock_path))?;
        let resp = rt
            .block_on(client.prune_stale(nestweaver_proto::PruneStaleRequest {}))
            .map_err(|e| anyhow!("prune_stale RPC failed: {e}"))?;
        let inner = resp.into_inner();
        Ok(json!({
            "removed_repos": inner.removed_repos,
            "removed_vaults": inner.removed_vaults
        }))
    }
    #[cfg(not(feature = "daemon"))]
    {
        let _ = store;
        Err(anyhow!("prune_stale requires daemon mode"))
    }
}

// ── Daemon connection helper ────────────────────────────────────────────────

/// Connect to a running daemon via UDS. Shared by brain_remove_source and
/// prune_stale (and brain_add_source's inline block).
#[cfg(feature = "daemon")]
async fn inline_connect_daemon(
    sock_path: &std::path::Path,
) -> Result<
    nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient<tonic::transport::Channel>,
    anyhow::Error,
> {
    use tonic::transport::{Endpoint, Uri};

    let path = sock_path.to_path_buf();
    let channel = Endpoint::try_from("http://[::]:50051")
        .map_err(|e| anyhow::anyhow!("failed to create endpoint: {e}"))?
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to daemon: {e}"))?;

    Ok(
        nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient::new(channel)
            .max_decoding_message_size(256 * 1024 * 1024)
            .max_encoding_message_size(256 * 1024 * 1024),
    )
}

/// Ensure the daemon is running (spawning it if needed) and return the
/// socket path. Mirrors `nestweaver_client::autostart::ensure_daemon`.
///
/// The socket-path derivation is inlined from `nestweaver_daemon::lifecycle`
/// to avoid a dependency cycle (nestweaver-mcp → nestweaver-daemon → nestweaver-mcp).
// TODO: deduplicate with nestweaver_daemon::lifecycle::socket_path()
#[cfg(feature = "daemon")]
fn inline_ensure_daemon(db_path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};

    // Compute the 8-char hex instance ID (same SHA-256 algorithm as lifecycle.rs).
    let canonical = if let Ok(c) = std::fs::canonicalize(db_path) {
        c
    } else if let (Some(parent), Some(file_name)) = (db_path.parent(), db_path.file_name())
        && let Ok(cp) = std::fs::canonicalize(parent)
    {
        cp.join(file_name)
    } else {
        db_path.to_path_buf()
    };
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    let instance_id = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3]
    );

    // Must match nestweaver_daemon::lifecycle::runtime_dir() exactly.
    // $TMPDIR is deliberately NOT consulted: on macOS, different launchers
    // see different TMPDIR values, causing socket-path mismatch.
    let rt_dir = if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::PathBuf::from(xdg)
            .join("nestweaver")
            .join(&instance_id)
    } else {
        dirs::state_dir()
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                    .join(".local/state")
            })
            .join("nestweaver")
            .join(&instance_id)
    };
    let sock = rt_dir.join("daemon.sock");

    // The socket inode can appear before the daemon starts accepting connections.
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        return Ok(sock);
    }

    // Spawn the daemon: `nestweaver daemon --db <path> start`
    std::fs::create_dir_all(&rt_dir).ok();
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("failed to determine current exe: {e}"))?;
    std::process::Command::new(&exe)
        .args(["daemon", "--db"])
        .arg(db_path)
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn daemon: {e}"))?;

    // Poll for the socket to accept connections (up to 5s, same as autostart.rs).
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(5);
    let mut delay = std::time::Duration::from_millis(50);
    while start.elapsed() < timeout {
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            return Ok(sock);
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_millis(500));
    }
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        return Ok(sock);
    }
    Err(anyhow::anyhow!(
        "daemon socket did not accept connections within 5s at {}",
        sock.display()
    ))
}

// ── 7. cross_repo_contracts ─────────────────────────────────────────────────

fn tool_schema_cross_repo_contracts() -> Value {
    json!({
        "name": "cross_repo_contracts",
        "description": "Find cross-repository references to a symbol — other repos that import, re-export, or implement the same symbol name.\n\nRequires either 'uid' or 'name' (at least one must be provided).\n\nGuidelines:\n- Use when modifying a shared symbol to understand cross-repo blast radius\n- Pass uid or name; returns other repos with confidence scores and link types\n- Only useful when multiple repos are indexed in the same brain\n\nLimitations:\n- For single-repo impact use brain_impact; for general search use brain_search\n- Contract links are hypotheses — check confidence scores before acting\n\nIn server mode, the server has the full org-wide view of cross-repo contracts. Through the hybrid client, results include _meta.sources indicating which data sources contributed; a raw single-daemon connection returns local results only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Symbol UID (e.g. sym:repo:...:hash:42). Preferred for unambiguous lookup." },
                "name": { "type": "string", "description": "Symbol name (e.g. \"UserService\"). Uses first match if multiple symbols share the name." },
                "limit": {
                    "type": "integer",
                    "description": "Max contract links to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_cross_repo_contracts(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let uid = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        uid.to_string()
    } else if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
        resolve_symbol_uid(store, name)?
    } else {
        return Err(anyhow!("provide either 'uid' or 'name'"));
    };
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);

    let refs = store
        .cross_repo_links(&uid)
        .map_err(|e| anyhow!("cross_repo_links: {e}"))?;

    let mut rows: Vec<Value> = refs
        .iter()
        .map(|r| {
            json!({
                "source_uid": r.source_uid,
                "source_name": r.source_name,
                "target_uid": r.target_uid,
                "target_name": r.target_name,
                "link_type": r.link_type,
                "confidence": r.confidence,
            })
        })
        .collect();

    // F2-core: also surface API contracts this symbol implements (HTTP route /
    // gRPC method / GraphQL operation). These are HYPOTHESES, not ground truth
    // — the confidence reflects match quality (1.0 exact verb+path, 0.8
    // base-path-inferred).
    let contract_links = store
        .contracts_implemented_by(&uid)
        .map_err(|e| anyhow!("contracts_implemented_by: {e}"))?;
    for (contract_uid, confidence) in &contract_links {
        rows.push(json!({
            "source_uid": uid,
            "target_uid": contract_uid,
            "link_type": "contract",
            "confidence": confidence,
        }));
    }

    let total = rows.len();
    rows.truncate(limit);

    Ok(json!({
        "uid": uid,
        "total": total,
        "returned": rows.len(),
        "note": "Links are hypotheses, not ground truth — check confidence. \
                 link_type \"contract\" denotes an implemented API contract.",
        "contracts": rows,
    }))
}

// ── 35. contract_drift ──────────────────────────────────────────────────────

fn tool_schema_contract_drift() -> Value {
    json!({
        "name": "contract_drift",
        "description": "Audit API contract drift: routes declared in specs (OpenAPI, .proto, GraphQL) but not implemented, and routes implemented but not declared in any spec.\n\nGuidelines:\n- Use to spot missing endpoints or undocumented APIs\n- Optional repo filter scopes to a single repository\n- Returns two buckets: declared_not_implemented and implemented_not_declared\n\nLimitations:\n- Contract links are hypotheses derived from spec parsing and handler heuristics (same-repo only)\n- Only supports OpenAPI/Swagger, .proto, and GraphQL spec formats\n\nIn server mode, the server has the full org-wide view of contract drift. Through the hybrid client, results include _meta.sources indicating which data sources contributed; a raw single-daemon connection returns local results only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Optional repo UID to scope the analysis to a single repository." },
                "limit": {
                    "type": "integer",
                    "description": "Max results per drift bucket (default 50). Totals are always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
        }
    })
}

fn tool_contract_drift(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let repo = args.get("repo").and_then(|v| v.as_str());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    let report = nestweaver_engine::contracts::drift_for_store(store, repo)
        .map_err(|e| anyhow!("drift_for_store: {e}"))?;
    let dni_total = report.declared_not_implemented.len();
    let ind_total = report.implemented_not_declared.len();
    let dni: Vec<_> = report
        .declared_not_implemented
        .into_iter()
        .take(limit)
        .collect();
    let ind: Vec<_> = report
        .implemented_not_declared
        .into_iter()
        .take(limit)
        .collect();
    Ok(json!({
        "note": "Contract links are hypotheses, not ground truth.",
        "declared_not_implemented": dni,
        "declared_not_implemented_total": dni_total,
        "implemented_not_declared": ind,
        "implemented_not_declared_total": ind_total,
        "clean": dni_total == 0 && ind_total == 0,
        "limit": limit,
    }))
}

// ── 8. brain_impact ─────────────────────────────────────────────────────────

fn tool_schema_brain_impact() -> Value {
    json!({
        "name": "brain_impact",
        "description": "Trace reverse dependencies of a symbol to understand what might break if it changes. Returns confidence-weighted impact scores (0.0-1.0) decaying through the call graph.\n\nGuidelines:\n- Use BEFORE modifying a function, class, or interface\n- Results sorted by impact_score (highest risk first); type-aware resolution follows class hierarchies\n- Use response_format 'concise' for names only, 'detailed' for full metadata\n\nLimitations:\n- For forward call chains use flow_trace; for file-level impact use detect_changes or blast_radius\n- For cross-repo impact use cross_repo_contracts\n\nWhen queried through the hybrid client (a local daemon connected to an upstream server), returns two-tier results (local_impact + org_wide_impact) with _meta.sources indicating provenance; a raw MCP connection to a single daemon returns single-tier local results.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name (e.g. \"validateUser\") or full UID (e.g. \"sym:repo:...:hash:42\"). Names are resolved via first-match lookup." },
                "depth": { "type": "integer", "description": "Max traversal depth. Higher values find more transitive dependents but take longer. Default 3.", "default": 3 },
                "limit": {
                    "type": "integer",
                    "description": "Max impact nodes to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns affected symbol names only; \"detailed\" (default) adds file paths, edge types, confidence scores, and depth levels."
                }
            },
            "required": ["symbol"]
        }
    })
}

fn tool_brain_impact(
    store: &GraphStore,
    args: Value,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'symbol' is required"))?;
    let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);
    let concise = is_concise(&args);
    let owners = restricted_symbol_owners(store, visible)?;
    let uid_is_visible = |uid: &str| {
        owners.as_ref().is_none_or(|owners| {
            owners
                .get(uid)
                .is_some_and(|repo_uid| repo_is_visible(repo_uid, visible))
        })
    };

    // Resolve with an explicit status so the CLI can honor the not-found/ambiguous exit-code
    // contract in daemon mode, instead of the daemon path silently returning the best of
    // several matches (which diverged from the direct path).
    let uid = if symbol.contains(':') {
        if uid_is_visible(symbol) {
            symbol.to_string()
        } else {
            return Ok(json!({
                "status": "not_found",
                "symbol": symbol,
                "impact_nodes": [],
                "total": 0,
                "returned": 0,
            }));
        }
    } else {
        let mut matches = store
            .lookup_symbols_by_name(symbol)
            .map_err(|e| anyhow!("lookup_symbols_by_name: {e}"))?;
        matches.retain(|candidate| repo_is_visible(&candidate.repo_uid, visible));
        match matches.len() {
            0 => {
                return Ok(json!({
                    "status": "not_found",
                    "symbol": symbol,
                    "impact_nodes": [],
                    "total": 0,
                    "returned": 0,
                }));
            }
            1 => matches.into_iter().next().unwrap().uid,
            _ => {
                let candidates: Vec<Value> = matches
                    .iter()
                    .map(|s| {
                        json!({
                            "uid": s.uid,
                            "name": s.name,
                            "file_path": s.file_path,
                            "start_line": s.start_line,
                        })
                    })
                    .collect();
                return Ok(json!({
                    "status": "ambiguous",
                    "symbol": symbol,
                    "candidates": candidates,
                }));
            }
        }
    };

    let mut nodes = if let Some(owners) = &owners {
        let allowed: HashSet<String> = owners
            .iter()
            .filter(|(_, repo_uid)| repo_is_visible(repo_uid, visible))
            .map(|(uid, _)| uid.clone())
            .collect();
        store.impact_cancellable_within(&uid, depth, 0.0, &allowed, cancel)?
    } else {
        store.impact_cancellable(&uid, depth, 0.0, cancel)?
    };
    nodes.retain(|node| uid_is_visible(&node.uid));
    let total = nodes.len();

    let rows: Vec<Value> = nodes
        .iter()
        .take(limit)
        .map(|n| {
            if concise {
                json!({
                    "name": n.name,
                    "depth": n.depth,
                    "impact_score": n.impact_score,
                })
            } else {
                json!({
                    "uid": n.uid,
                    "name": n.name,
                    "file_path": n.file_path,
                    "start_line": n.start_line,
                    "edge_type": n.edge_type,
                    "confidence": n.confidence,
                    "depth": n.depth,
                    "impact_score": n.impact_score,
                })
            }
        })
        .collect();

    Ok(json!({
        "status": "ok",
        "target": uid,
        "impact_nodes": rows,
        "total": total,
        "returned": rows.len(),
    }))
}

// ── 9. brain_guide ──────────────────────────────────────────────────────────

fn tool_schema_brain_guide() -> Value {
    json!({
        "name": "brain_guide",
        "description": "Generate a comprehensive orientation guide covering all indexed repos, vaults, cross-repo relationships, and available tools.\n\nGuidelines:\n- Call at session start for a read-once overview before issuing specific queries\n- Regenerated from current graph state on each call\n- The tools section is generated from the live MCP registry, so it never drifts from the actual tool set\n- Not a query tool — use brain_context or brain_search for specific lookups\n\nLimitations:\n- Can be expensive on large graphs; prefer brain_status for lightweight session initialization\n- Output size scales with number of indexed sources",
        "inputSchema": {
            "type": "object",
            "properties": {
                "format": {
                    "type": "string",
                    "enum": ["markdown", "skill", "cursor-rule", "agents-md", "claude-md"],
                    "default": "markdown",
                    "description": "Output format. \"markdown\" (default) is the full orientation guide; \"skill\" emits a Claude skill; the others emit the matching agent-instruction file. All formats render the tool list from the live registry."
                }
            }
        }
    })
}

fn tool_brain_guide(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("markdown");

    // Build tool docs from the live MCP registry so the tools section always
    // reflects the real tool set rather than a hand-curated subset.
    let tool_docs: Vec<ToolDocEntry> = tool_doc_entries()
        .into_iter()
        .map(|(name, category, purpose, key_params)| ToolDocEntry {
            name,
            category,
            purpose,
            key_params,
        })
        .collect();

    // The MCP server does not hold an InstanceConfig at runtime; cross-repo
    // edges from the graph are still included via the store query.
    let guide = match format {
        "skill" => generate_skill_with_tools(store, None, None, &tool_docs)?,
        "cursor-rule" => generate_cursor_rule_with_rules(store, None, None)?,
        "agents-md" => generate_agents_md_with_rules(store, None, None, Some(tool_docs.len()))?,
        "claude-md" => generate_claude_md_with_rules(store, None, None)?,
        _ => generate_guide_with_tools(store, None, None, &tool_docs)?,
    };
    Ok(json!({ "guide": guide }))
}

// ── 10. flow_trace ─────────────────────────────────────────────────────────

fn tool_schema_flow_trace() -> Value {
    json!({
        "name": "flow_trace",
        "description": "Trace forward execution flow from a symbol: what it calls, what those call, and so on. Returns a tree of callees.\n\nGuidelines:\n- Best for tracing from entry points (main, request handlers) to understand execution paths\n- Cycles are detected and pruned; use max_depth to control tree depth (default 10)\n- Classes are auto-expanded to their methods since classes have no direct CALLS edges\n\nLimitations:\n- For reverse dependencies ('what calls this?') use brain_impact instead\n- For general structural context use brain_context",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name (e.g. \"handleRequest\") or full UID (e.g. \"sym:repo:...:hash:42\") to trace from." },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum traversal depth. Default 10. Higher values trace deeper call chains but produce larger results.",
                    "default": 10
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns function name chain only; \"detailed\" (default) adds file paths, UIDs, and depth at each node."
                }
            },
            "required": ["symbol"]
        }
    })
}

fn tool_flow_trace(
    store: &GraphStore,
    args: Value,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Value, anyhow::Error> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'symbol' must be a string"))?;
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);
    let concise = is_concise(&args);

    let resolved_uid = resolve_symbol_uid(store, symbol)?;

    let root = store
        .lookup_symbol(&resolved_uid)
        .map_err(|_| anyhow!("symbol '{symbol}' not found"))?;

    let mut visited = HashSet::new();
    visited.insert(root.uid.clone());

    let opts = FlowTraceOpts {
        max_depth,
        concise,
        cancel,
    };

    // Classes don't have CALLS edges — only their methods do. When the root
    // symbol is a class, expand to its methods and return a flow tree per method.
    if root.kind == SymbolKind::Class {
        let direct_callees = store
            .callees_of(&root.uid)
            .map_err(|e| anyhow!("callees_of: {e}"))?;
        if direct_callees.is_empty() {
            // Prefer MEMBER_OF edges — these correctly scope inner-class methods.
            let members = store
                .members_of(&root.uid)
                .map_err(|e| anyhow!("members_of: {e}"))?;

            let is_method = |s: &nestweaver_schema::Symbol| {
                s.kind == SymbolKind::Method || s.kind == SymbolKind::Function
            };

            const MAX_METHODS: usize = 20;
            let method_trees: Vec<Value> = if !members.is_empty() {
                members
                    .iter()
                    .filter(|s| is_method(s))
                    .take(MAX_METHODS)
                    .map(|s| {
                        let mut v = visited.clone();
                        v.insert(s.uid.clone());
                        build_flow_tree(store, &s.uid, &s.name, &s.file_path, 0, &mut v, &opts)
                    })
                    .collect::<Result<Vec<Value>, _>>()?
            } else {
                // Fallback: line-range heuristic excluding methods inside nested classes.
                let file_symbols = store
                    .symbols_in_file(&root.file_path)
                    .map_err(|e| anyhow!("symbols_in_file: {e}"))?;
                let nested_class_ranges: Vec<(u32, u32)> = file_symbols
                    .iter()
                    .filter(|s| {
                        s.kind == SymbolKind::Class
                            && s.uid != root.uid
                            && s.start_line > root.start_line
                            && s.end_line < root.end_line
                    })
                    .map(|s| (s.start_line, s.end_line))
                    .collect();
                file_symbols
                    .iter()
                    .filter(|s| {
                        s.uid != root.uid
                            && is_method(s)
                            && s.start_line > root.start_line
                            && s.start_line <= root.end_line
                            && !nested_class_ranges
                                .iter()
                                .any(|&(start, end)| s.start_line >= start && s.start_line <= end)
                    })
                    .take(MAX_METHODS)
                    .map(|s| {
                        let mut v = visited.clone();
                        v.insert(s.uid.clone());
                        build_flow_tree(store, &s.uid, &s.name, &s.file_path, 0, &mut v, &opts)
                    })
                    .collect::<Result<Vec<Value>, _>>()?
            };

            return Ok(json!({
                "root_uid": root.uid,
                "root_name": root.name,
                "root_kind": "class",
                "max_depth": max_depth,
                "note": "Class expanded to its methods — classes have no direct CALLS edges. Methods filtered to this class only.",
                "methods": method_trees,
            }));
        }
    }

    let tree = build_flow_tree(
        store,
        &root.uid,
        &root.name,
        &root.file_path,
        0,
        &mut visited,
        &opts,
    )?;

    Ok(json!({
        "root_uid": root.uid,
        "root_name": root.name,
        "max_depth": max_depth,
        "tree": tree,
    }))
}

/// Configuration for `build_flow_tree` to keep the argument count under
/// the clippy `too_many_arguments` threshold.
struct FlowTraceOpts<'a> {
    max_depth: usize,
    concise: bool,
    /// Cooperative cancellation flag, checked once per recursion level. See
    /// [`nestweaver_store::StoreError::Cancelled`] for the contract.
    cancel: Option<&'a std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

fn build_flow_tree(
    store: &GraphStore,
    uid: &str,
    name: &str,
    file_path: &str,
    depth: usize,
    visited: &mut HashSet<String>,
    opts: &FlowTraceOpts<'_>,
) -> Result<Value, anyhow::Error> {
    if opts
        .cancel
        .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    {
        // A cancelled trace is incomplete — propagate the store's typed
        // cancellation error so the boundary never serves (or caches) a
        // truncated tree as a real answer.
        return Err(anyhow::Error::new(nestweaver_store::StoreError::Cancelled(
            nestweaver_store::CancelReason::Timeout,
        )));
    }

    let mut children = Vec::new();

    if depth < opts.max_depth
        && let Ok(callees) = store.callees_of(uid)
    {
        for callee in &callees {
            if visited.contains(&callee.uid) {
                continue;
            }
            visited.insert(callee.uid.clone());
            let child = build_flow_tree(
                store,
                &callee.uid,
                &callee.name,
                &callee.file_path,
                depth + 1,
                visited,
                opts,
            )?;
            children.push(child);
        }
    }

    if opts.concise {
        Ok(json!({
            "name": name,
            "children": children,
        }))
    } else {
        // Look up repo_uid and canonical_id for boundary detection.
        let (repo_uid, canonical_id) = store
            .lookup_symbol(uid)
            .ok()
            .map(|s| {
                (
                    s.repo_uid.clone(),
                    s.canonical_id.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        Ok(json!({
            "uid": uid,
            "name": name,
            "file_path": file_path,
            "depth": depth,
            "repo_uid": repo_uid,
            "canonical_id": canonical_id,
            "children": children,
        }))
    }
}

// ── 11. detect_changes ─────────────────────────────────────────────────────

fn tool_schema_detect_changes() -> Value {
    json!({
        "name": "detect_changes",
        "description": "Assess file-level blast radius for a set of changed files. Maps files to symbols, traces transitive dependents, and returns a risk assessment with explicit trust status.\n\nGuidelines:\n- Use BEFORE committing or reviewing changes\n- Pass repo-relative file paths; returns affected symbols, flows, and risk level (low/medium/high)\n- Treat `risk` as usable only when `status == complete`; `degraded-unknown` requires reindexing or manual review\n- For single-symbol impact use brain_impact; for git diff details use brain_diff\n\nLimitations:\n- Static call-graph analysis only — misses runtime/reflection-based dependencies\n- For cross-repo impact use cross_repo_contracts",
        "inputSchema": {
            "type": "object",
            "properties": {
                "changed_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "List of changed file paths (repo-relative). Example: [\"src/auth/login.ts\", \"src/utils/validate.ts\"]."
                },
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Backward-compatible alias for changed_files."
                }
            },
            "anyOf": [
                { "required": ["changed_files"] },
                { "required": ["files"] }
            ]
        }
    })
}

fn tool_detect_changes(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let files: Vec<String> = args
        .get("changed_files")
        .or_else(|| args.get("files"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("'changed_files' must be an array of strings"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if files.is_empty() {
        return Err(anyhow!("'files' must contain at least one path"));
    }

    let impact = detect_changes_impact(store, &files, 10).context("detect_changes_impact")?;

    let affected_symbols: Vec<Value> = impact
        .affected_symbols
        .iter()
        .map(|s| {
            json!({
                "uid": s.uid,
                "name": s.name,
                "file_path": s.file_path,
            })
        })
        .collect();

    let affected_processes: Vec<Value> = impact
        .affected_processes
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "uid": p.uid,
                "affected_symbol_count": p.affected_symbol_count,
                "total_symbol_count": p.total_symbol_count,
            })
        })
        .collect();

    let risk_str = match impact.risk {
        nestweaver_engine::RiskLevel::Low => "low",
        nestweaver_engine::RiskLevel::Medium => "medium",
        nestweaver_engine::RiskLevel::High => "high",
    };

    Ok(json!({
        "files": files,
        "risk": risk_str,
        "status": serde_json::to_value(impact.status)?,
        "gate_state": serde_json::to_value(impact.gate_state)?,
        "notifications": serde_json::to_value(&impact.notifications)?,
        "blast_radius": impact.blast_radius,
        "affected_symbols": affected_symbols,
        "affected_symbol_count": impact.affected_symbols.len(),
        "affected_processes": affected_processes,
        "affected_process_count": impact.affected_processes.len(),
    }))
}

// ── 31. affected_tests ──────────────────────────────────────────────────────

fn tool_schema_affected_tests() -> Value {
    json!({
        "name": "affected_tests",
        "description": "Prioritize which test files a PR should run by mapping changed files through the call/import graph to test files. Results bucketed into priority tiers.\n\nRequires either 'changed_files' or 'base_ref' (at least one must be provided).\n\nGuidelines:\n- Provide changed_files (repo-relative) or base_ref (git ref like 'main') to diff against\n- tier_1 = directly references changed symbol, tier_2 = direct caller, tier_3 = transitive\n- For symbol-level blast radius use brain_impact; for risk scoring use detect_changes\n- `recommendation` is a machine-readable CI directive: 'run-full-suite' on any non-complete run (fail-safe widening), 'selection-usable' otherwise\n\nLimitations:\n- Static call-graph regression test selection — misses reflection, DI, codegen, and integration/e2e tests\n- 'No tests found' does NOT mean safe to skip testing. IMPORTANT: keep periodic full test runs in CI\n\nWhen queried through the hybrid client (a local daemon connected to an upstream server), returns two-tier results (local_impact + org_wide_impact) with _meta.sources indicating provenance; a raw MCP connection to a single daemon returns single-tier local results.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "changed_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Changed file paths (repo-relative). Example: [\"src/auth/login.ts\"]."
                },
                "base_ref": {
                    "type": "string",
                    "description": "Git ref to diff against (e.g. \"main\"). Used when changed_files is omitted; diffs the locally-indexed repo via git."
                }
            }
        }
    })
}

fn tool_affected_tests(
    store: &GraphStore,
    args: Value,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    let owners = restricted_symbol_owners(store, visible)?;

    // Resolve the set of changed files: explicit list takes precedence over base_ref.
    let mut changed_files: Vec<String> = bound_identifiers(
        args.get("changed_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    );

    let base_ref = args.get("base_ref").and_then(|v| v.as_str());
    if changed_files.is_empty()
        && let Some(base_ref) = base_ref
    {
        let repo_path = scoped_local_repo_path(store, visible)?.unwrap_or_else(|| ".".to_string());
        let files =
            nestweaver_engine::changed_files_from_git(Path::new(&repo_path), Some(base_ref))
                .context("git diff for base_ref")?;
        changed_files = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
    }

    // Only a genuine "no input" (neither changed_files nor base_ref) is an error.
    // A `base_ref` that resolves to ZERO changed files (e.g. HEAD vs HEAD) is a
    // valid empty diff — run the analysis so the caller still gets the full trust
    // contract (empty tiers + status/recommendation/disclaimer), matching the
    // changed_files-with-no-symbols path, instead of an error that drops it
    // (nw-088).
    if changed_files.is_empty() && base_ref.is_none() {
        return Err(anyhow!(
            "provide either 'changed_files' (non-empty) or 'base_ref'"
        ));
    }

    // nw-037: route through the recorded wrapper so every selection feeds the
    // measured-recall loop and carries the in-band `measured` disclosure.
    let mut result = if let Some(owners) = &owners {
        let allowed: HashSet<String> = owners
            .iter()
            .filter(|(_, repo_uid)| repo_is_visible(repo_uid, visible))
            .map(|(uid, _)| uid.clone())
            .collect();
        nestweaver_engine::affected_tests::affected_tests_scoped(store, &changed_files, &allowed)
            .context("affected_tests")?
    } else {
        let db_path = current_db_path(store).ok();
        nestweaver_engine::rts_eval::run_recorded(store, &changed_files, db_path.as_deref())
            .context("affected_tests")?
    };

    if let Some(owners) = owners {
        let mut ownership_unproven = false;
        result.changed_symbols.retain(|symbol| {
            let repo_uid = if symbol.repo_uid.is_empty() {
                owners.get(&symbol.uid).map(String::as_str)
            } else {
                Some(symbol.repo_uid.as_str())
            };
            match repo_uid {
                Some(repo_uid) => repo_is_visible(repo_uid, visible),
                None => {
                    ownership_unproven = true;
                    false
                }
            }
        });
        for tier in [&mut result.tier_1, &mut result.tier_2, &mut result.tier_3] {
            tier.retain(|file| match owners.get(&file.symbol_uid) {
                Some(repo_uid) => repo_is_visible(repo_uid, visible),
                None => {
                    ownership_unproven = true;
                    false
                }
            });
        }

        // This aggregate is learned across recorded selections and is not
        // caller-scope keyed, so it cannot be disclosed under repo scoping.
        result.measured = None;
        if ownership_unproven {
            result.status = result
                .status
                .max(nestweaver_engine::blast_radius::AnalysisStatus::Degraded);
            result
                .notifications
                .push(nestweaver_engine::blast_radius::Notification {
                    level: nestweaver_engine::blast_radius::NotificationLevel::Error,
                    message: "affected-test ownership could not be proven".to_string(),
                    descriptor: "authorization.symbol-ownership-unproven".to_string(),
                });
        }

        for notification in &mut result.notifications {
            tracing::debug!(
                descriptor = %notification.descriptor,
                detail = %notification.message,
                "redacting affected-test notification detail for a restricted response"
            );
            notification.message =
                "affected-test analysis details withheld by repository visibility policy"
                    .to_string();
        }
        let count_tests = |tier: &[nestweaver_engine::affected_tests::AffectedTestFile]| {
            tier.iter().map(|file| file.tests.len()).sum::<usize>()
        };
        result.summary = format!(
            "{} tier-1, {} tier-2, {} tier-3 tests affected",
            count_tests(&result.tier_1),
            count_tests(&result.tier_2),
            count_tests(&result.tier_3),
        );
        result.recommendation = if matches!(
            result.status,
            nestweaver_engine::blast_radius::AnalysisStatus::Complete
        ) {
            "selection-usable"
        } else {
            "run-full-suite"
        }
        .to_string();
    }
    Ok(serde_json::to_value(&result)?)
}

/// Resolve the local working tree used for a `base_ref` diff.
///
/// Restricted callers must have exactly one visible local repository; choosing
/// an arbitrary first repository would let graph ordering cross caller scope.
fn scoped_local_repo_path(
    store: &GraphStore,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Option<String>, anyhow::Error> {
    let repos = store.list_repos(None).map_err(|error| {
        anyhow!("listing repositories for affected-tests base_ref failed: {error}")
    })?;
    let roots: Vec<String> = repos
        .iter()
        .filter(|repo| repo_is_visible(&repo.uid, visible))
        .filter_map(|repo| repo.local_root().map(String::from))
        .collect();
    if matches!(
        visible,
        Some(nestweaver_engine::authz::VisibleRepos::Only(_))
    ) {
        match roots.as_slice() {
            [root] => Ok(Some(root.clone())),
            [] => Err(anyhow!(
                "affected_tests base_ref requires one visible local repository"
            )),
            _ => Err(anyhow!(
                "affected_tests base_ref is ambiguous across visible repositories; \
                 provide changed_files explicitly"
            )),
        }
    } else {
        Ok(roots.into_iter().next())
    }
}

// ── 12. clusters ───────────────────────────────────────────────────────────

fn tool_schema_clusters() -> Value {
    json!({
        "name": "clusters",
        "description": "View the codebase's high-level architecture via Leiden community detection. Groups tightly-connected symbols into named functional clusters.\n\nGuidelines:\n- Adjust resolution: higher = more smaller clusters, lower = fewer larger clusters (default 0.5)\n- Returns cluster name, cohesion score, key files, and a 20-member preview per cluster (full `size` reported)\n- Pass cluster_id to get ONE cluster's full member list (paging deep clusters); `members_truncated` flags when even that is capped\n- For specific symbol lookup use brain_search; for dependency analysis use brain_impact\n\nLimitations:\n- Clustering is computed on demand, not cached\n- Quality depends on the density and accuracy of indexed call/import edges",
        "inputSchema": {
            "type": "object",
            "properties": {
                "resolution": {
                    "type": "number",
                    "description": "Leiden resolution parameter. Higher = more, smaller clusters; lower = fewer, larger clusters. Default 0.5 (0.3 for large graphs >10K symbols). Try 2.0 for fine-grained modules.",
                    "default": 0.5
                },
                "cluster_id": {
                    "type": "integer",
                    "description": "Return only this cluster (by its numeric `id`), with its FULL member list instead of the 20-member preview. Use the same resolution as the call that produced the id."
                }
            }
        }
    })
}

fn tool_clusters(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let user_resolution = args.get("resolution").and_then(|v| v.as_f64());
    let resolution = user_resolution.unwrap_or_else(|| {
        let sym_count = store.count_symbols().unwrap_or(0);
        if sym_count > 10_000 { 0.3 } else { 0.5 }
    });

    let output = compute_clusters(store, resolution).context("compute_clusters")?;

    // Persist so hub_nodes/bridge_nodes can read the sidecar afterwards.
    // Deliberate exception to "reads don't write": `clusters` is not in
    // MUTATING_TOOLS, so this runs under the daemon's read guard — but it
    // only writes derived cache data to a fixed sidecar path, never graph
    // state, and degrades to warn-only (e.g. on a read-only filesystem).
    if let Ok(db_path) = current_db_path(store)
        && let Err(e) = nestweaver_engine::save_clusters(&db_path, &output)
    {
        tracing::warn!("failed to persist clusters sidecar: {e}");
    }

    // nw-090: `cluster_id` pages the FULL membership of a single cluster. Without
    // it, every cluster returns a 20-member preview (`size` still reports the true
    // count), which made large clusters' membership unretrievable from the tool.
    let requested_id = args.get("cluster_id").and_then(|v| v.as_i64());
    // Full membership when a specific cluster is requested (bounded so a giant
    // cluster can't blow the context window), a small preview otherwise.
    const FULL_MEMBER_CAP: usize = 2000;
    const PREVIEW_MEMBER_CAP: usize = 20;
    let clusters_json: Vec<Value> = output
        .communities
        .iter()
        .filter(|c| requested_id.is_none_or(|id| c.id as i64 == id))
        .map(|c| {
            let member_cap = if requested_id.is_some() {
                FULL_MEMBER_CAP
            } else {
                PREVIEW_MEMBER_CAP
            };
            let members: Vec<Value> = c
                .members
                .iter()
                .take(member_cap)
                .map(|m| {
                    json!({
                        "uid": m.uid,
                        "name": m.name,
                        "file_path": m.file_path,
                    })
                })
                .collect();
            json!({
                "id": c.id,
                "name": c.name,
                "size": c.member_count,
                "cohesion": c.cohesion,
                "key_files": c.key_files,
                "members": members,
                "members_truncated": c.members.len() > member_cap,
            })
        })
        .collect();

    let symbol_count: usize = output.communities.iter().map(|c| c.member_count).sum();

    Ok(json!({
        "resolution": resolution,
        "cluster_count": output.communities.len(),
        "symbol_count": symbol_count,
        "modularity": output.modularity,
        "clusters": clusters_json,
    }))
}

// ── 13. stale_check ────────────────────────────────────────────────────────

fn tool_schema_stale_check() -> Value {
    json!({
        "name": "stale_check",
        "description": "Check whether the graph index is current by comparing each repo's indexed git SHA against HEAD. No parameters required.\n\nGuidelines:\n- Call at session start or after code changes to verify index freshness\n- Returns per-repo staleness with indexed SHA, HEAD SHA, and commits-behind count\n- If stale, re-index with brain_add_source or CLI nestweaver index\n\nLimitations:\n- Only checks git repos, not vault/note freshness\n- For viewing what actually changed, use brain_diff",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

fn tool_stale_check(store: &GraphStore) -> Result<Value, anyhow::Error> {
    let repos = store
        .list_repos(None)
        .map_err(|e| anyhow!("list_repos: {e}"))?;

    let mut results = Vec::new();
    let mut any_stale = false;

    for repo in &repos {
        // Local working tree → read HEAD from disk; otherwise ask the remote.
        let current_head = if let Some(path) = repo.local_root() {
            get_git_head(path)
        } else {
            get_remote_head(&repo.url)
        };

        // Compute commits behind for local repos when HEAD differs from indexed SHA.
        let is_valid_sha =
            repo.indexed_sha.len() == 40 && repo.indexed_sha.chars().all(|c| c.is_ascii_hexdigit());
        let commits_behind = match (&current_head, repo.local_root()) {
            (Some(head), Some(path)) if is_valid_sha && *head != repo.indexed_sha => {
                count_commits_between(path, &repo.indexed_sha, head).unwrap_or(0)
            }
            _ => repo.staleness_commits_behind as u64,
        };

        let is_stale = match &current_head {
            Some(head) => head != &repo.indexed_sha,
            None => commits_behind > 0,
        };

        if is_stale {
            any_stale = true;
        }

        results.push(json!({
            "url": repo.url,
            "indexed_sha": repo.indexed_sha,
            "current_head": current_head,
            "is_stale": is_stale,
            "staleness_commits_behind": commits_behind,
        }));
    }

    Ok(json!({
        "repo_count": repos.len(),
        "any_stale": any_stale,
        "repos": results,
    }))
}

/// Get the current HEAD sha for a git repo at the given path.
fn get_git_head(repo_path: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Count commits between two SHAs in a local repo.
fn count_commits_between(repo_path: &str, from_sha: &str, to_sha: &str) -> Option<u64> {
    let range = format!("{from_sha}..{to_sha}");
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-list", "--count", &range])
        .output()
        .ok()?;
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u64>()
            .ok()
    } else {
        None
    }
}

/// Get the current HEAD sha for a remote git repo via `git ls-remote`.
/// Works for SSH (`git@github.com:...`) and HTTPS (`https://...`) URLs.
///
/// Stderr is suppressed so SSH key errors or other diagnostics don't leak
/// into MCP responses.
fn get_remote_head(url: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["ls-remote", "--exit-code", url, "HEAD"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format: "<sha>\tHEAD\n"
    stdout.split_whitespace().next().map(|s| s.to_string())
}

// ── 14. set_extension ──────────────────────────────────────────────────────

fn tool_schema_set_extension() -> Value {
    json!({
        "name": "set_extension",
        "description": "Attach custom key-value metadata to any node (symbol, note, section, tag) in a JSON sidecar alongside the database.\n\nGuidelines:\n- Use for information not in core schema: team ownership, deprecation status, review flags\n- Value accepts any JSON type (string, number, boolean, array, object); overwrites existing\n- Properties persist across sessions and are queryable via query_extensions\n\nLimitations:\n- Stored in a sidecar file, not the main graph — not included in graph traversals\n- To query existing properties use query_extensions, not this tool",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": {
                    "type": "string",
                    "description": "Node UID to annotate (e.g. \"sym:repo:...:hash:42\" for symbols, \"note:vlt:...:hash\" for notes)."
                },
                "key": {
                    "type": "string",
                    "description": "Property name (e.g. \"team_owner\", \"deprecated\", \"review_needed\", \"priority\")."
                },
                "value": {
                    "description": "Property value — any JSON value (string, number, boolean, object, array). Overwrites any existing value for this key on this node."
                }
            },
            "required": ["uid", "key", "value"]
        }
    })
}

fn tool_set_extension(args: Value) -> Result<Value, anyhow::Error> {
    let db_path = CURRENT_DB_PATH
        .with(|c| c.borrow().clone())
        .ok_or_else(|| anyhow!("database path not set on server"))?;

    // Always route through the daemon (starting it if needed) so the sidecar
    // read-modify-write runs under the daemon write gate — serialized against a
    // backup's sidecar staging, visible to the shutdown drain, and safe from
    // two concurrent callers losing updates (last-writer-wins). Mirrors
    // `tool_brain_add_source`. The daemon's gRPC `set_extension` handler
    // validates and performs the actual mutation directly (never back through
    // this function), so there is no routing recursion.
    #[cfg(feature = "daemon")]
    {
        let db_path_buf = std::path::PathBuf::from(&db_path);
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| anyhow::anyhow!("failed to create tokio runtime: {e}"))?;
        let sock_path = inline_ensure_daemon(&db_path_buf)
            .map_err(|e| anyhow::anyhow!("failed to start daemon: {e}"))?;
        let mut client = rt.block_on(inline_connect_daemon(&sock_path))?;
        dispatch_set_extension_via_daemon(&mut client, &rt, args)
    }

    // Non-daemon fallback (daemon feature not compiled in): single-process, so
    // a direct write is the only writer and needs no cross-process gate.
    #[cfg(not(feature = "daemon"))]
    {
        let uid = args
            .get("uid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("'uid' must be a string"))?;
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("'key' must be a string"))?;
        let value = args
            .get("value")
            .cloned()
            .ok_or_else(|| anyhow!("'value' is required"))?;

        let mut store = load_extensions(&db_path);
        set_property(&mut store, uid, key, value.clone());
        save_extensions(&db_path, &store)?;

        Ok(json!({
            "uid": uid,
            "key": key,
            "value": value,
            "status": "saved",
        }))
    }
}

/// Route a `set_extension` call to the daemon's gated gRPC handler so the
/// `.extensions.json` write holds the daemon write gate. Mirrors
/// `dispatch_add_source_via_daemon`.
#[cfg(feature = "daemon")]
fn dispatch_set_extension_via_daemon(
    client: &mut DaemonGrpcClient,
    rt: &tokio::runtime::Runtime,
    args: Value,
) -> Result<Value, anyhow::Error> {
    use nestweaver_proto::JsonRequest;

    let args_json = serde_json::to_string(&args)?;
    let resp = rt
        .block_on(client.set_extension(JsonRequest { args_json }))
        .map_err(|e| anyhow::anyhow!("set_extension RPC failed: {}", e.message()))?;
    let result_json = resp.into_inner().result_json;
    serde_json::from_str(&result_json).map_err(Into::into)
}

// ── 15. query_extensions ───────────────────────────────────────────────────

fn tool_schema_query_extensions() -> Value {
    json!({
        "name": "query_extensions",
        "description": "Query custom metadata set via set_extension. Two modes: by uid (all properties for a node) or by key+value (find all nodes matching a property).\n\nRequires either 'uid' or both 'key' and 'value' (at least one mode must be specified).\n\nGuidelines:\n- Pass uid alone to inspect a node's custom properties\n- Pass key + value to find all nodes matching that property (exact match only)\n- For core graph queries use brain_search or brain_context, not this tool\n\nLimitations:\n- Exact match only — no partial matching, ranges, or regex on values\n- Only queries the extension sidecar, not core graph properties",
        "inputSchema": {
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Property name to filter by (e.g. \"team_owner\", \"deprecated\"). Required when not using uid mode."
                },
                "value": {
                    "description": "Value to match — any JSON value. Required when key is provided. Exact match only."
                },
                "uid": {
                    "type": "string",
                    "description": "Return all custom properties for this specific node UID. When provided, key and value are ignored."
                }
            }
        }
    })
}

fn tool_query_extensions(args: Value) -> Result<Value, anyhow::Error> {
    let db_path = CURRENT_DB_PATH
        .with(|c| c.borrow().clone())
        .ok_or_else(|| anyhow!("database path not set on server"))?;

    let store = load_extensions(&db_path);

    // Single-UID lookup mode.
    if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        let props = get_all_properties(&store, uid);
        return Ok(json!({
            "uid": uid,
            "properties": props,
        }));
    }

    // Filter-by-key-value mode.
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("provide either 'uid' or both 'key' and 'value'"))?;
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| anyhow!("'value' is required when 'key' is given"))?;

    let matching_uids = query_by_property(&store, key, &value);

    let results: Vec<Value> = matching_uids
        .iter()
        .map(|uid| {
            let props = store.get(*uid).cloned().unwrap_or_default();
            json!({
                "uid": uid,
                "properties": props,
            })
        })
        .collect();

    Ok(json!({
        "key": key,
        "value": value,
        "count": results.len(),
        "results": results,
    }))
}

// ── 16. brain_diff ─────────────────────────────────────────────────────────

fn tool_schema_brain_diff() -> Value {
    json!({
        "name": "brain_diff",
        "description": "Show what changed since the graph was last indexed: files added/modified/deleted plus affected symbols. For locally-indexed repos only.\n\nGuidelines:\n- Use before code review or after pulling changes to understand the delta\n- Pass repo name or URL substring; optional since_sha overrides the base\n- For impact analysis of hypothetical changes use detect_changes; for staleness check use stale_check\n\nLimitations:\n- Only works with local repos (file:// URLs), not remote\n- Shows file-level diff, not line-level — use git diff for detailed patches",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repo name or substring of its URL (e.g. \"nestweaver\" or \"github.com/org/repo\"). Matched against indexed repos."
                },
                "since_sha": {
                    "type": "string",
                    "description": "Git SHA to compare against. Defaults to the repo's indexed_sha. Use a specific SHA to diff against an older baseline."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max affected symbols to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            },
            "required": ["repo"]
        }
    })
}

fn tool_brain_diff(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    use nestweaver_engine::git_diff;

    let repo_name = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'repo' must be a string"))?;
    let since_sha_arg = args.get("since_sha").and_then(|v| v.as_str());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);

    // Find the repo in the graph.
    let repos = store.list_repos(None)?;
    let repo = repos
        .iter()
        .find(|r| {
            r.url.contains(repo_name) || {
                let name_part = r.url.split('/').next_back().unwrap_or("");
                name_part == repo_name
            }
        })
        .ok_or_else(|| anyhow!("repo '{}' not found in graph", repo_name))?;

    let Some(repo_path) = repo.local_root() else {
        anyhow::bail!(
            "brain_diff only works with locally-indexed repositories \
             (repos with a local working tree); '{}' is not a local repo",
            repo.url
        );
    };

    let base_sha = since_sha_arg.unwrap_or(&repo.indexed_sha);

    // Get the current HEAD SHA.
    let head_sha = git_diff::current_head_sha(std::path::Path::new(repo_path))
        .unwrap_or_else(|_| "unknown".to_string());

    // If base == head there is nothing to show.
    if base_sha == head_sha {
        return Ok(json!({
            "repo": repo_name,
            "base_sha": base_sha,
            "head_sha": head_sha,
            "files_added": 0,
            "files_modified": 0,
            "files_deleted": 0,
            "changed_files": [],
            "affected_symbols": [],
            "message": "graph is up to date with HEAD",
        }));
    }

    let changes = git_diff::detect_changes(std::path::Path::new(repo_path), base_sha, &head_sha)
        .context("git diff")?;

    let mut added: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();

    for change in &changes {
        match change {
            git_diff::FileChange::Added(p) => added.push(p.to_string_lossy().into_owned()),
            git_diff::FileChange::Modified(p) => modified.push(p.to_string_lossy().into_owned()),
            git_diff::FileChange::Deleted(p) => deleted.push(p.to_string_lossy().into_owned()),
            git_diff::FileChange::Renamed { to, .. } => {
                modified.push(to.to_string_lossy().into_owned())
            }
        }
    }

    // Collect symbols from the changed/added files.
    let changed_paths: Vec<&str> = added
        .iter()
        .chain(modified.iter())
        .map(String::as_str)
        .collect();

    let mut affected_symbols: Vec<Value> = Vec::new();
    for file_path in &changed_paths {
        if let Ok(syms) = store.symbols_in_file(file_path) {
            for sym in syms {
                affected_symbols.push(json!({
                    "uid": sym.uid,
                    "name": sym.name,
                    "kind": sym.kind,
                    "file_path": sym.file_path,
                    "start_line": sym.start_line,
                }));
            }
        }
    }

    let all_changed: Vec<&str> = added
        .iter()
        .chain(modified.iter())
        .map(String::as_str)
        .collect();

    let total_affected = affected_symbols.len();
    affected_symbols.truncate(limit);

    Ok(json!({
        "repo": repo_name,
        "base_sha": base_sha,
        "head_sha": head_sha,
        "files_added": added.len(),
        "files_modified": modified.len(),
        "files_deleted": deleted.len(),
        "added_files": added,
        "modified_files": modified,
        "deleted_files": deleted,
        "changed_files": all_changed,
        "affected_symbols": affected_symbols,
        "affected_symbol_count": total_affected,
        "affected_symbols_returned": affected_symbols.len(),
    }))
}

// ── 17. project_context ────────────────────────────────────────────────────

fn tool_schema_project_context() -> Value {
    json!({
        "name": "project_context",
        "description": "Retrieve context for a named project: notes, symbols, and sections ranked by PPR within the project's subgraph, bounded by token budget.\n\nGuidelines:\n- Use when you know the project name — for ad-hoc topics use brain_context with seeds instead\n- Returns a CONCISE orientation by default (~1000 tokens: kind/title/location per node); pass response_format:'detailed' for full metadata (uid + relevance, ~3000 tokens)\n- Narrow with repos, path_prefix, tags/exclude_tags, kinds, since, recency_weight — carry the same filter names over to brain_context when drilling in\n- For composite projects, include_components pulls in sub-project content\n\nLimitations:\n- Requires projects to be defined in the graph (via vault taxonomy or instance config)\n- If you don't know the project name, use brain_search to find it first",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project name (e.g. \"AuthService\"), alias, or UID. Resolved via name match, then alias match, then UID substring match."
                },
                "token_budget": {
                    "type": "integer",
                    "default": 3000,
                    "description": "Approximate token cap for the result (chars / 4). Increase for comprehensive context, decrease for quick overview."
                },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter result kinds: \"Symbol\" for code, \"Note\" for documents, \"Section\" for note sections. Case-insensitive prefix match."
                },
                "include_components": {
                    "type": "boolean",
                    "default": true,
                    "description": "For composite projects, also include notes/symbols from component sub-projects. Set to false to see only direct project content."
                },
                "since": {
                    "type": "string",
                    "description": "ISO 8601 timestamp. Only return Note/Section nodes modified after this time. Symbol nodes always kept."
                },
                "recency_weight": {
                    "type": "number",
                    "default": 0.0,
                    "description": "Multiplier for age-decay boost. 0 = disabled. 1.0 = same-day node ranks ~2x a year-old node."
                },
                "recency_half_life_days": {
                    "type": "number",
                    "default": 30.0,
                    "description": "Half-life for age-decay in days."
                },
                "include_seeds": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, include the full seeds array in the response. Default false — only seeds_expanded (count) is returned to keep responses small."
                },
                "intent": {
                    "type": "string",
                    "enum": ["find-definition", "understand-architecture", "analyze-impact", "general-context"],
                    "description": "Optional query intent hint that adjusts ranking strategy. 'find-definition' boosts exact name matches; 'understand-architecture' broadens to structural neighbors (default for project_context); 'analyze-impact' follows dependency edges; 'general-context' uses balanced defaults."
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "description": "'concise' (default) returns kind/title/location per node at a ~1000-token budget — right for orienting at session start, then narrow with brain_context. 'detailed' adds uid + relevance and uses a ~3000-token budget."
                },
                "repos": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Scope results to these repos (display name, uid, or path substring). Use on a returning session to skip the broad load."
                },
                "path_prefix": {
                    "type": "string",
                    "description": "Keep only nodes whose location starts with this path prefix (e.g. \"crates/nestweaver-daemon/\")."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keep only note/section nodes tagged with any of these tags. Symbol nodes are always kept."
                },
                "exclude_tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Drop note/section nodes tagged with any of these tags."
                }
            },
            "required": ["project"]
        }
    })
}

fn tool_project_context(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    args: Value,
    embed_model: Option<&dyn EmbedQueryFn>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Value, anyhow::Error> {
    // Reject an empty/whitespace project — otherwise the UID-substring fallback below
    // (`uid.contains(project_str)`) matches EVERY project on "" and silently resolves to the
    // first one. Reachable via the daemon path, where the proto `project` field defaults to "".
    let project_str = args
        .get("project")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("'project' must be a non-empty string"))?;
    // response_format: "concise" (default) trims per-node fields and uses a smaller default
    // token budget; "detailed" returns full metadata at the larger budget. A session-opener
    // wants orientation, not payload — the agent then narrows. See ADR
    // server-mode-remainder-decisions (evidence: Anthropic response_format, Aider 1k map,
    // Lost-in-the-Middle / Context Rot). Anything but "detailed" (incl. empty) → concise.
    let concise = args
        .get("response_format")
        .and_then(|v| v.as_str())
        .map(|s| !s.eq_ignore_ascii_case("detailed"))
        .unwrap_or(true);
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(if concise { 1000 } else { 3000 });
    let include_components = args
        .get("include_components")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let filter_repos: Option<Vec<String>> =
        args.get("repos").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
    let path_prefix: Option<String> = args
        .get("path_prefix")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let filter_kinds: Option<Vec<String>> =
        args.get("kinds").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect()
        });

    // 1. Resolve the project: name/alias/UID.
    let project = if project_str.starts_with("proj:") {
        // Direct UID — list all projects and find by uid or substring.
        let all = store
            .list_projects()
            .map_err(|e| anyhow!("list_projects: {e}"))?;
        all.into_iter()
            .find(|p| p.uid == project_str || p.uid.contains(project_str))
            .ok_or_else(|| anyhow!("project UID '{}' not found", project_str))?
    } else {
        // Try name match first.
        match store
            .lookup_project_by_name(project_str)
            .map_err(|e| anyhow!("lookup_project_by_name: {e}"))?
        {
            Some(p) => p,
            None => {
                let all = store
                    .list_projects()
                    .map_err(|e| anyhow!("list_projects: {e}"))?;

                // Try alias match via extension sidecar.
                let db_path = current_db_path(store).unwrap_or_default();
                let ext_store = load_extensions(&db_path);
                let needle = project_str.to_lowercase();
                let alias_match = all.iter().find(|p| {
                    if let Some(serde_json::Value::Array(aliases)) =
                        ext_store.get(&p.uid).and_then(|m| m.get("aliases"))
                    {
                        aliases
                            .iter()
                            .any(|a| a.as_str().is_some_and(|s| s.to_lowercase() == needle))
                    } else {
                        false
                    }
                });
                if let Some(p) = alias_match {
                    p.clone()
                } else {
                    // Fall back to UID substring match.
                    all.into_iter()
                        .find(|p| p.uid.contains(project_str))
                        .ok_or_else(|| anyhow!("project '{}' not found", project_str))?
                }
            }
        }
    };

    // 2. Collect member UIDs (notes + symbols) for the post-PPR boost.
    //    Member note UIDs are tracked separately: they get seeded into PPR
    //    and surfaced into `connected` (Bug #12).
    let mut member_uids: Vec<String> = Vec::new();
    let mut member_note_uids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let note_uids = store
        .list_project_note_uids(&project.uid)
        .map_err(|e| anyhow!("list_project_note_uids: {e}"))?;
    member_note_uids.extend(note_uids.iter().cloned());
    member_uids.extend(note_uids);
    let sym_uids = store
        .list_project_symbol_uids(&project.uid)
        .map_err(|e| anyhow!("list_project_symbol_uids: {e}"))?;
    member_uids.extend(sym_uids);

    // 3. If include_components, also collect note/symbol UIDs from each component project.
    let component_uids = if include_components {
        store
            .list_project_component_uids(&project.uid)
            .map_err(|e| anyhow!("list_project_component_uids: {e}"))?
    } else {
        vec![]
    };
    for comp_uid in &component_uids {
        let comp_notes = store
            .list_project_note_uids(comp_uid)
            .map_err(|e| anyhow!("list_project_note_uids: {e}"))?;
        member_note_uids.extend(comp_notes.iter().cloned());
        member_uids.extend(comp_notes);
        let comp_syms = store
            .list_project_symbol_uids(comp_uid)
            .map_err(|e| anyhow!("list_project_symbol_uids: {e}"))?;
        member_uids.extend(comp_syms);
    }

    // Deduplicate members.
    let mut seen = std::collections::HashSet::new();
    member_uids.retain(|u| seen.insert(u.clone()));

    if member_uids.is_empty() {
        return Ok(json!({
            "project": project.name,
            "project_uid": project.uid,
            "seeds_expanded": 0,
            "connected": [],
            "tokens_used": 0,
            "token_budget": token_budget,
            "note": "No notes or symbols are associated with this project yet.",
        }));
    }

    // 4. Seed PPR from the project node, its components, and — critically —
    //    the project's member notes (Bug #12). Seeding the notes guarantees
    //    they survive the `min_score` filter in PPR: when a project declares
    //    repos, the project node's mass is split across tens of thousands of
    //    PROJECT_INCLUDES_SYMBOL edges, leaving each PROJECT_INCLUDES_NOTE
    //    target below threshold so it never reaches `connected`. Seeding also
    //    lets the walk explore each note's neighbourhood (sections, links).
    //
    //    Member symbols suffer the identical fan-out, so also seed the top-K
    //    by PageRank (Bug #18 / wave-5 regression). Without this, a project
    //    that declares any repo returns notes-only context even after
    //    `materialize-projects` writes hundreds of thousands of
    //    PROJECT_INCLUDES_SYMBOL edges.
    const PROJECT_SYMBOL_SEED_LIMIT: usize = 100;
    let mut member_symbol_uids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let top_symbols = store
        .list_project_symbol_uids_by_pagerank(&project.uid, PROJECT_SYMBOL_SEED_LIMIT)
        .map_err(|e| anyhow!("list_project_symbol_uids_by_pagerank: {e}"))?;
    member_symbol_uids.extend(top_symbols);
    for comp_uid in &component_uids {
        let comp_top = store
            .list_project_symbol_uids_by_pagerank(comp_uid, PROJECT_SYMBOL_SEED_LIMIT)
            .unwrap_or_default();
        member_symbol_uids.extend(comp_top);
    }

    let mut ppr_seeds: Vec<String> = vec![project.uid.clone()];
    ppr_seeds.extend(component_uids);
    ppr_seeds.extend(member_note_uids.iter().cloned());
    ppr_seeds.extend(member_symbol_uids.iter().cloned());

    let intent: nestweaver_store::QueryIntent = args
        .get("intent")
        .and_then(|v| v.as_str())
        .map(|s| s.parse::<nestweaver_store::QueryIntent>())
        .transpose()
        .map_err(|e| anyhow!("invalid intent: {e}"))?
        .unwrap_or(nestweaver_store::QueryIntent::ProjectContext);

    let db_path = current_db_path(store).unwrap_or_default();
    let aliases = load_alias_sidecar(&db_path);
    let config = match current_instance_config() {
        Some(cfg) => HybridSearchConfig {
            weight_ppr: cfg.embedding.weight_ppr,
            weight_bm25: cfg.embedding.weight_bm25,
            weight_semantic: cfg.embedding.weight_semantic,
            semantic_limit: cfg.embedding.semantic_search_limit,
            always_blend_semantic: cfg.embedding.always_blend_semantic,
            semantic_seed_limit: cfg.embedding.semantic_seed_limit,
            ..HybridSearchConfig::default()
        },
        None => HybridSearchConfig::default(),
    };
    let mut result = build_brain_context_hybrid_with_aliases(
        store,
        &ppr_seeds,
        tantivy,
        &config,
        &aliases,
        Some(&db_path),
        Some(intent),
        embed_model,
        cancel,
    )?;

    // 4b. Surface the project's curated member notes into `connected`. They
    //     were seeded above, so they live in `result.seeds` — which is
    //     disjoint from `connected` and not rendered. For project orientation
    //     the curated notes are the answer, so promote them (Bug #12).
    nestweaver_engine::promote_member_notes_into_connected(&mut result, &member_note_uids);
    // 4b'. Mirror the notes promotion for the seeded top-K member symbols
    //      (Bug #18 / wave-5 regression). Without this, the symbols stay in
    //      `seeds` and never appear in the rendered `connected` list.
    nestweaver_engine::promote_member_symbols_into_connected(&mut result, &member_symbol_uids);
    // 4b''. Drop Heading nodes that duplicate a Section with the same
    //       `(file, title)`. The Section carries the body, so the bare
    //       Heading is redundant; notes-heavy projects spend ~25% of a
    //       2000-token budget on these duplicates without this trim.
    nestweaver_engine::dedup_heading_section_pairs(&mut result);

    // 4c. Post-PPR scope boost: multiply relevance for nodes that belong
    //     to the project (member UIDs are the authoritative membership signal).
    let member_set: std::collections::HashSet<&str> =
        member_uids.iter().map(|s| s.as_str()).collect();
    for node in &mut result.connected {
        if member_set.contains(node.uid.as_str()) {
            node.relevance *= 5.0;
        }
    }
    result.connected.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 5. Apply optional kinds filter.
    if let Some(ref kinds) = filter_kinds {
        let apply_kinds = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
            nodes.retain(|n| {
                let kind_lower = n.kind.to_lowercase();
                kinds.iter().any(|k| kind_lower.starts_with(k.as_str()))
            });
        };
        apply_kinds(&mut result.seeds);
        apply_kinds(&mut result.connected);
    }

    // 5b. repos + path_prefix scope filters (mirror brain_context so the two tools scope
    //     identically — the "load project_context, then narrow" handoff keeps the same params).
    let repo_names = if filter_repos.is_some() {
        build_repo_name_map(store)
    } else {
        std::collections::HashMap::new()
    };
    let apply_scope = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
        if let Some(ref repos) = filter_repos {
            let filter_lower: Vec<String> = repos.iter().map(|r| r.to_lowercase()).collect();
            nodes.retain(|n| {
                let node_repo_uid = if n.uid.starts_with("sym:") {
                    let parts: Vec<&str> = n.uid[4..].splitn(4, ':').collect();
                    if parts.len() >= 3 {
                        Some(format!("{}:{}:{}", parts[0], parts[1], parts[2]))
                    } else {
                        None
                    }
                } else {
                    None
                };
                filter_lower.iter().any(|r| {
                    if let Some(ref repo_uid) = node_repo_uid
                        && let Some(name) = repo_names.get(repo_uid)
                        && name.to_lowercase().contains(r)
                    {
                        return true;
                    }
                    n.uid.to_lowercase().contains(r) || n.location.to_lowercase().contains(r)
                })
            });
        }
        if let Some(ref prefix) = path_prefix {
            nodes.retain(|n| n.location.starts_with(prefix.as_str()));
        }
    };
    apply_scope(&mut result.seeds);
    apply_scope(&mut result.connected);

    // 5c. tags filter: keep only note/section nodes tagged with any of these (symbols kept).
    if let Some(tags) = args.get("tags").and_then(|v| v.as_array()) {
        let tag_names: Vec<String> = tags
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !tag_names.is_empty() {
            let tagged_notes = store
                .list_note_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_note_uids_with_tags: {e}"))?;
            let tagged_sections = store
                .list_section_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_section_uids_with_tags: {e}"))?;
            let filter_tagged = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                nodes.retain(|item| {
                    if item.kind.to_lowercase().contains("symbol") {
                        return true;
                    }
                    tagged_notes.contains(&item.uid) || tagged_sections.contains(&item.uid)
                });
            };
            filter_tagged(&mut result.seeds);
            filter_tagged(&mut result.connected);
        }
    }

    // 5d. exclude_tags filter: drop note/section nodes tagged with any of these.
    if let Some(exclude_tags) = args.get("exclude_tags").and_then(|v| v.as_array()) {
        let tag_names: Vec<String> = exclude_tags
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !tag_names.is_empty() {
            let excluded_notes = store
                .list_note_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_note_uids_with_tags: {e}"))?;
            let excluded_sections = store
                .list_section_uids_with_tags(&tag_names)
                .map_err(|e| anyhow!("list_section_uids_with_tags: {e}"))?;
            let filter_excluded = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
                nodes.retain(|item| {
                    !excluded_notes.contains(&item.uid) && !excluded_sections.contains(&item.uid)
                });
            };
            filter_excluded(&mut result.seeds);
            filter_excluded(&mut result.connected);
        }
    }

    // 6a. since filter: hard filter Note/Section nodes by modified_at.
    if let Some(since) = args.get("since").and_then(|v| v.as_str()) {
        let recent_notes = store
            .list_note_uids_modified_since(since)
            .map_err(|e| anyhow!("list_note_uids_modified_since: {e}"))?;
        let recent_sections = store
            .list_section_uids_modified_since(since)
            .map_err(|e| anyhow!("list_section_uids_modified_since: {e}"))?;
        let filter_since = |nodes: &mut Vec<nestweaver_engine::BrainNode>| {
            nodes.retain(|item| {
                if item.kind.to_lowercase().contains("symbol") {
                    return true;
                }
                recent_notes.contains(&item.uid) || recent_sections.contains(&item.uid)
            });
        };
        filter_since(&mut result.seeds);
        filter_since(&mut result.connected);
    }

    // 6b. recency bias: soft boost based on note modified_at age.
    let recency_weight = args
        .get("recency_weight")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let recency_half_life_days = args
        .get("recency_half_life_days")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0);
    if recency_weight > 0.0 {
        apply_recency_bias(
            store,
            &mut result.connected,
            recency_weight,
            recency_half_life_days,
        );
        apply_recency_bias(
            store,
            &mut result.seeds,
            recency_weight,
            recency_half_life_days,
        );
    }

    // 6. Apply token budget: account for seed cost, allocate remainder to
    //    connected. Don't double-count items that the promotion helpers above
    //    copied from `seeds` into `connected` — those tokens belong to the
    //    connected budget, not the seed overhead.
    let connected_uids: std::collections::HashSet<&str> =
        result.connected.iter().map(|n| n.uid.as_str()).collect();
    let seed_tokens: usize = result
        .seeds
        .iter()
        .filter(|n| !connected_uids.contains(n.uid.as_str()))
        .map(|n| render_cost(n, concise))
        .sum();
    let remaining_budget = token_budget.saturating_sub(seed_tokens);
    let (cut, connected_tokens) = budgeted_cut(&result.connected, remaining_budget, concise);
    let used_tokens = seed_tokens + connected_tokens;

    // 7. Load external_refs from extension sidecar.
    let ext_store = load_extensions(&db_path);
    let external_refs = get_all_properties(&ext_store, &project.uid)
        .get("external_refs")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // concise drops the machine id (uid) and the relevance score in favor of the semantic
    // fields an agent orients with (kind/title/location); detailed keeps the full record.
    let render_node = |n: &nestweaver_engine::BrainNode| -> Value {
        if concise {
            json!({
                "kind": n.kind,
                "title": n.title,
                "location": n.location,
            })
        } else {
            json!({
                "uid": n.uid,
                "kind": n.kind,
                "title": n.title,
                "location": n.location,
                "relevance": n.relevance,
            })
        }
    };

    let mut connected_json: Vec<Value> =
        result.connected.iter().take(cut).map(render_node).collect();

    let include_seeds = args
        .get("include_seeds")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let seeds_json: Option<Vec<Value>> = if include_seeds {
        Some(result.seeds.iter().map(render_node).collect())
    } else {
        None
    };

    // Final budget enforcement: measure actual serialized size including
    // response wrapper metadata (project, project_uid, seeds_expanded,
    // token_budget, tokens_used, external_refs) which the per-node
    // render_cost does not account for.
    {
        let mut probe = json!({
            "project": project.name,
            "project_uid": project.uid,
            "seeds_expanded": result.seeds.len(),
            "connected": connected_json,
            "tokens_used": used_tokens,
            "token_budget": token_budget,
        });
        if let Some(ref sj) = seeds_json {
            probe["seeds"] = json!(sj);
        }
        if !result.unresolved_seeds.is_empty() {
            probe["unresolved_seeds"] = json!(result.unresolved_seeds);
        }
        if !external_refs.is_null() {
            probe["external_refs"] = external_refs.clone();
        }
        let serialized = serde_json::to_string(&probe)?;
        let actual_tokens = serialized.len().div_ceil(4);
        if actual_tokens > token_budget {
            while connected_json.len() > 1 {
                connected_json.pop();
                probe["connected"] = json!(connected_json);
                let check = serde_json::to_string(&probe)?;
                if check.len().div_ceil(4) <= token_budget {
                    break;
                }
            }
        }
    }

    let mut resp = json!({
        "project": project.name,
        "project_uid": project.uid,
        "seeds_expanded": result.seeds.len(),
        "connected": connected_json,
        "tokens_used": used_tokens,
        "token_budget": token_budget,
    });

    if let Some(sj) = seeds_json {
        resp["seeds"] = json!(sj);
    }

    if !result.unresolved_seeds.is_empty() {
        resp["unresolved_seeds"] = json!(result.unresolved_seeds);
    }

    if !external_refs.is_null() {
        resp["external_refs"] = external_refs;
    }

    Ok(resp)
}

// ── 18. dead_code ─────────────────────────────────────────────────────────

fn tool_schema_dead_code() -> Value {
    json!({
        "name": "dead_code",
        "description": "Find potentially unreachable symbols by walking forward from all entry points (main, HTTP handlers, event listeners, test runners).\n\nGuidelines:\n- Confidence scoring: High (private/internal), Medium (inferred visibility), Low (public/library API)\n- Use min_confidence to filter; 'low' shows all, 'high' shows only strong candidates\n- For understanding what depends on a specific symbol use brain_impact instead\n\nLimitations:\n- Static reachability analysis — misses runtime reflection, DI, and dynamic dispatch\n- Public symbols flagged as Low confidence may be consumed by external code",
        "inputSchema": {
            "type": "object",
            "properties": {
                "min_confidence": {
                    "type": "string",
                    "enum": ["low", "medium", "high"],
                    "default": "low",
                    "description": "Minimum confidence to include in results. Default 'low' (show all)."
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns name + confidence only; \"detailed\" (default) adds UIDs, file paths, kinds, and visibility."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max unreachable symbols to return (defaults to the configured result limit). The response reports the true total in 'unreachable_count' and sets 'truncated' when the cap applied."
                }
            }
        }
    })
}

fn tool_dead_code(
    store: &GraphStore,
    args: Value,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Value, anyhow::Error> {
    let min_conf_str = args
        .get("min_confidence")
        .and_then(|v| v.as_str())
        .unwrap_or("low");
    let min_conf =
        DeadCodeConfidence::from_str_loose(min_conf_str).unwrap_or(DeadCodeConfidence::Low);
    let concise = is_concise(&args);
    // Cap the returned symbols so a large codebase can't return a multi-MB
    // payload that blows an agent's context window (the HTTP boundary caps via
    // add_limit_metadata, but the stdio path had no bound). `unreachable_count`
    // still reports the true total; `returned`/`truncated` disclose the cap.
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(configured_result_limit);

    let result = detect_dead_code_cancellable(store, cancel).context("detect_dead_code")?;

    let all_matching: Vec<_> = result
        .unreachable_symbols
        .iter()
        .filter(|s| s.confidence >= min_conf)
        .collect();
    let total_unreachable = all_matching.len();
    let filtered: Vec<Value> = all_matching
        .into_iter()
        .take(limit)
        .map(|s| {
            if concise {
                json!({
                    "name": s.name,
                    "confidence": s.confidence.to_string(),
                })
            } else {
                json!({
                    "uid": s.uid,
                    "name": s.name,
                    "kind": s.kind,
                    "file_path": s.file_path,
                    "visibility": s.visibility,
                    "confidence": s.confidence.to_string(),
                })
            }
        })
        .collect();

    Ok(json!({
        "total_symbols": result.total_symbols,
        "reachable_symbols": result.reachable_symbols,
        "unreachable_count": total_unreachable,
        "returned": filtered.len(),
        "truncated": total_unreachable > filtered.len(),
        "excluded_count": result.excluded_count,
        "dead_percentage": result.dead_percentage,
        "min_confidence": min_conf_str,
        "unreachable_symbols": filtered,
    }))
}

// ── 19. hub_nodes ─────────────────────────────────────────────────────────

fn tool_schema_hub_nodes() -> Value {
    json!({
        "name": "hub_nodes",
        "description": "Identify the most connected symbols in the codebase ranked by total degree (incoming + outgoing edges). These are the architectural core.\n\nGuidelines:\n- Use for quick orientation on which abstractions are most central\n- Includes optional cluster membership when clustering sidecar exists\n- For chokepoints between communities use bridge_nodes instead\n\nLimitations:\n- Degree centrality only — does not account for path importance (use bridge_nodes for betweenness)\n- For specific symbol dependencies use brain_impact or flow_trace",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Number of top hubs to return. Default 10.",
                    "default": 10
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns name + total degree only; \"detailed\" (default) adds UIDs, file paths, PageRank scores, and cluster IDs."
                }
            }
        }
    })
}

fn tool_hub_nodes(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let top_n = args
        .get("limit")
        .or_else(|| args.get("top_n"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);
    let concise = is_concise(&args);

    let mut hubs = find_hub_nodes(store, top_n).context("find_hub_nodes")?;

    // Attach cluster IDs if clustering sidecar exists.
    let db_path = current_db_path(store).unwrap_or_default();
    let clustering_available = match load_clusters(&db_path) {
        Ok(Some(clustering)) => {
            attach_cluster_ids(&mut hubs, &clustering);
            true
        }
        _ => false,
    };

    let nodes_json: Vec<Value> = hubs
        .iter()
        .map(|h| {
            if concise {
                json!({
                    "name": h.name,
                    "total_degree": h.total_degree,
                })
            } else {
                json!({
                    "uid": h.uid,
                    "name": h.name,
                    "file_path": h.file_path,
                    "in_degree": h.in_degree,
                    "out_degree": h.out_degree,
                    "total_degree": h.total_degree,
                    "pagerank_score": h.pagerank_score,
                    "cluster_id": h.cluster_id,
                })
            }
        })
        .collect();

    let mut resp = json!({
        "top_n": top_n,
        "count": nodes_json.len(),
        "hubs": nodes_json,
        "clustering_available": clustering_available,
    });
    if !clustering_available {
        resp["note"] = json!(
            "cluster_id is null because clustering has not been computed. Run 'nestweaver cluster' to populate."
        );
    }
    Ok(resp)
}

// ── 20. bridge_nodes ──────────────────────────────────────────────────────

fn tool_schema_bridge_nodes() -> Value {
    json!({
        "name": "bridge_nodes",
        "description": "Find architectural chokepoints — symbols with high betweenness centrality that sit on many shortest paths between other nodes.\n\nGuidelines:\n- Use to identify symbols with outsized blast radius if changed\n- Returns betweenness score plus which community clusters each bridge connects\n- For most-connected nodes (degree centrality) use hub_nodes instead\n\nLimitations:\n- Betweenness computed via Brandes' algorithm with sampling — approximate for large graphs\n- For single-symbol impact analysis use brain_impact",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Number of top bridges to return. Default 10.",
                    "default": 10
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns name + betweenness score only; \"detailed\" (default) adds UIDs, file paths, and connected community IDs."
                }
            }
        }
    })
}

fn tool_bridge_nodes(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let top_n = args
        .get("limit")
        .or_else(|| args.get("top_n"))
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);
    let concise = is_concise(&args);

    let mut bridges = find_bridge_nodes(store, top_n).context("find_bridge_nodes")?;

    // Attach community connection info if clustering sidecar exists.
    let db_path = current_db_path(store).unwrap_or_default();
    if let Ok(Some(clustering)) = load_clusters(&db_path) {
        attach_communities(&mut bridges, &clustering, store);
    }

    let nodes_json: Vec<Value> = bridges
        .iter()
        .map(|b| {
            if concise {
                json!({
                    "name": b.name,
                    "betweenness_score": b.betweenness_score,
                })
            } else {
                json!({
                    "uid": b.uid,
                    "name": b.name,
                    "file_path": b.file_path,
                    "betweenness_score": b.betweenness_score,
                    "communities_connected": b.communities_connected,
                })
            }
        })
        .collect();

    Ok(json!({
        "top_n": top_n,
        "count": nodes_json.len(),
        "bridges": nodes_json,
    }))
}

// ── 21. blast_radius ─────────────────────────────────────────────────────

fn tool_schema_blast_radius() -> Value {
    json!({
        "name": "blast_radius",
        "description": "Assess full blast radius of file changes: maps to symbols, traces reverse dependencies, groups by cluster, and returns risk level (Low/Medium/High) with impact scores.\n\nGuidelines:\n- Use BEFORE merging a PR; pass repo-relative changed file paths\n- Each affected symbol has impact_score (0.0-1.0) decaying through the call graph\n- For single-symbol impact use brain_impact; for cross-repo use cross_repo_contracts\n\n`cochanged_files` lists historically co-changing files (git history, Jaccard confidence) with no static edge — an advisory recall supplement; absence of co-change data is disclosed via a `cochange-unavailable` note.\n\nTrust contract (read before trusting a green result):\n- status (complete/partial/degraded/failed) + gate_state (ok/degraded-unknown/risk-flagged): a run that did NOT complete is degraded-unknown, NEVER risk-flagged — treat it as 'unknown, review manually', not 'safe'\n- coverage (repos in scope / not indexed / stale / truncated) distinguishes 'no impact' from 'incomplete coverage'\n- blind_spots: inherent static gaps (dynamic-dispatch, reflection, config-wiring, codegen) plus run-specific ones (pruned-below-threshold, depth-truncated, not-indexed)\n\nLimitations:\n- Static analysis only — misses dynamic dispatch and reflection (declared in blind_spots, not silently)\n- Response size scales with number of changed files and graph density\n\nWhen queried through the hybrid client (a local daemon connected to an upstream server), returns two-tier results (local_impact + org_wide_impact) with _meta.sources indicating provenance; a raw MCP connection to a single daemon returns single-tier local results. On an authenticated server with an [authz] policy, results are redacted to the caller's visible repos.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "changed_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of changed file paths (repo-relative). Example: [\"src/auth/login.ts\", \"src/utils/validate.ts\"]."
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum transitive traversal depth. Default 3. Higher values find more distant dependents.",
                    "default": 3
                },
                "repo": {
                    "type": "string",
                    "description": "Optional repo_uid to scope changed-file resolution to (recommended in multi-repo graphs)."
                },
                "include_data_edges": {
                    "type": "boolean",
                    "description": "Also follow data-dependence edges (type refs & field access). Higher recall, noisier; default false.",
                    "default": false
                },
                "limit": {
                    "type": "integer",
                    "description": "Cap on returned affected_symbols (most-impactful first). Omit for the full set; a truncation note reports the true total.",
                    "minimum": 1
                },
                "format": {
                    "type": "string",
                    "enum": ["json", "sarif"],
                    "description": "Output format. 'json' (default) is the native result; 'sarif' emits SARIF v2.1.0 for GitHub code scanning / Azure DevOps / the VS Code SARIF viewer.",
                    "default": "json"
                }
            },
            "required": ["changed_files"]
        }
    })
}

fn tool_blast_radius(
    store: &GraphStore,
    args: Value,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    let files: Vec<std::path::PathBuf> = args
        .get("changed_files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("'changed_files' must be an array of strings"))?
        .iter()
        .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
        .collect();

    if files.is_empty() {
        return Err(anyhow!("'changed_files' must contain at least one path"));
    }

    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(3);

    let target_repo = args.get("repo").and_then(|v| v.as_str());

    // Optional: also follow data-dependence edges (type refs & field access).
    // Default false — higher recall but noisier.
    let include_data_edges = args
        .get("include_data_edges")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Optional cap on returned affected_symbols (most-impactful first).
    let requested_limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let visibility_restricted = matches!(
        visible,
        Some(nestweaver_engine::authz::VisibleRepos::Only(_))
    );

    let options = BlastRadiusOptions {
        target_repo: target_repo.map(str::to_string),
        max_depth,
        include_data_edges,
        // Restricted callers must be redacted against the complete result so
        // their total is exact and contains no pre-authz cardinality.
        limit: if visibility_restricted {
            None
        } else {
            requested_limit
        },
    };

    let db_path = current_db_path(store).ok();
    let mut result = analyze_blast_radius(store, &files, &options, cancel, db_path.as_deref())
        .context("analyze_blast_radius")?;

    // R9b: redact the typed result down to the caller's visible repos BEFORE
    // building any output (both the JSON and SARIF paths), so every derived
    // count/field reflects the redacted vecs. A `None`/`All` visibility (the
    // unconfigured single-trust-domain default) is a no-op — zero behavior
    // change unless an `[authz]` policy scopes this caller. nw-043: a store
    // error at this re-list means the boundary's earlier listing succeeded and
    // this one failed — exactly the transient signature — so fail the request
    // rather than serve a mis-redacted result.
    if let Some(v @ nestweaver_engine::authz::VisibleRepos::Only(_)) = visible {
        let repos = store.list_repos(None).map_err(|e| {
            // Log the detailed chain server-side; return a generic message so
            // the client never sees store internals.
            tracing::error!("authz: repo listing failed at redaction point: {e:#}");
            anyhow!("authz repo listing unavailable")
        })?;
        nestweaver_engine::authz::redact_blast_radius_for_visibility(&mut result, v, &repos);
        nestweaver_engine::blast_radius::apply_affected_symbol_limit(&mut result, requested_limit);
    }

    // SARIF output: emit a standard SARIF v2.1.0 run (with namespaced
    // nestweaver/* extensions) instead of the native json result. The default
    // path is unchanged.
    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("json");
    if format == "sarif" {
        return Ok(nestweaver_engine::blast_radius_to_sarif(
            &result,
            env!("CARGO_PKG_VERSION"),
        ));
    }

    let risk_str = match result.risk_level {
        nestweaver_engine::RiskLevel::Low => "low",
        nestweaver_engine::RiskLevel::Medium => "medium",
        nestweaver_engine::RiskLevel::High => "high",
    };

    let changed_json: Vec<Value> = result
        .changed_symbols
        .iter()
        .map(|s| {
            json!({
                "uid": s.uid,
                "name": s.name,
                "file_path": s.file_path,
                "kind": s.kind,
                "pagerank_score": s.pagerank_score,
            })
        })
        .collect();

    let affected_json: Vec<Value> = result
        .affected_symbols
        .iter()
        .map(|s| {
            json!({
                "uid": s.uid,
                "name": s.name,
                "file_path": s.file_path,
                "depth": s.depth,
                "edge_type": s.edge_type,
                "confidence": s.confidence,
                "impact_score": s.impact_score,
            })
        })
        .collect();

    let clusters_json: Vec<Value> = result
        .affected_clusters
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "name": c.name,
                "affected_count": c.affected_count,
                "total_count": c.total_count,
                "cohesion": c.cohesion,
            })
        })
        .collect();

    // Trust signals: whether the analysis ran to completion, the gate verdict
    // (never RiskFlagged from a degraded run), and machine-readable reasons.
    let notifications_json: Vec<Value> = result
        .notifications
        .iter()
        .map(|n| serde_json::to_value(n).unwrap_or(Value::Null))
        .collect();
    let status_json = serde_json::to_value(result.status).unwrap_or(Value::Null);
    let gate_state_json = serde_json::to_value(result.gate_state).unwrap_or(Value::Null);

    // Coverage & blind spots: which repos were in scope/stale/not-indexed,
    // whether the traversal was truncated, and the static-analysis gaps — so a
    // consumer can tell "no impact" from "incomplete coverage".
    let coverage_json = serde_json::to_value(&result.coverage).unwrap_or(Value::Null);
    let blind_spots_json = serde_json::to_value(&result.blind_spots).unwrap_or(Value::Null);

    Ok(json!({
        "changed_files": files.iter().map(|f| f.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "max_depth": max_depth,
        "risk": risk_str,
        "summary": result.summary,
        "status": status_json,
        "gate_state": gate_state_json,
        "notifications": notifications_json,
        "changed_symbols": changed_json,
        "changed_symbol_count": changed_json.len(),
        "affected_symbols": affected_json,
        "affected_symbol_count": result.affected_symbol_count,
        "returned_affected_symbol_count": affected_json.len(),
        "affected_symbols_truncated": affected_json.len() < result.affected_symbol_count,
        "affected_clusters": clusters_json,
        "affected_cluster_count": clusters_json.len(),
        // Always present so consumers can distinguish "no cross-repo impact"
        // (null) from the field being dropped; populated when a change reaches
        // another repo.
        "org_wide": serde_json::to_value(&result.org_wide).unwrap_or(Value::Null),
        "coverage": coverage_json,
        "blind_spots": blind_spots_json,
        "cochanged_files": serde_json::to_value(&result.cochanged_files)
            .unwrap_or(serde_json::Value::Array(Vec::new())),
        "analysis_direction": result.analysis_direction,
    }))
}

// ── 22. get_summary ──────────────────────────────────────────────────────

fn tool_schema_get_summary() -> Value {
    json!({
        "name": "get_summary",
        "description": "Generate deterministic architectural summaries at three granularity levels: symbol, file, or cluster. Derived from graph data, no LLM needed.\n\nGuidelines:\n- level 'symbol' = per-function/class with callers/callees, 'file' = per-file exports, 'cluster' = community architecture\n- Use target to filter to a specific file, symbol, or cluster name\n- Use token_budget to cap output size for context windows\n\nLimitations:\n- Summaries reflect indexed graph state — may be stale if index is behind HEAD\n- For specific symbol source code use read_symbols; for call chains use flow_trace",
        "inputSchema": {
            "type": "object",
            "properties": {
                "level": {
                    "type": "string",
                    "enum": ["symbol", "file", "cluster", "hub"],
                    "description": "Summary granularity. 'symbol' = per-function/class, 'file' = per-file exports, 'cluster' = per-community architecture, 'hub' = top hub nodes with call-graph shape + role (architectural orientation).",
                    "default": "file"
                },
                "target": {
                    "type": "string",
                    "description": "Optional filter: file path, symbol name, or cluster name substring. Only matching summaries are returned."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Approximate token cap for the result. Default unlimited.",
                }
            }
        }
    })
}

fn tool_get_summary(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    use nestweaver_engine::{load_summaries, merge_and_save_summaries};

    let level_str = args.get("level").and_then(|v| v.as_str()).unwrap_or("file");
    let level: SummaryLevel = level_str.parse().map_err(|e: String| anyhow!("{e}"))?;
    // Accept `name` as an alias for `target` — agents naturally pass a symbol as
    // `name`, and silently dropping it used to force a full-store regeneration.
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .or_else(|| args.get("name").and_then(|v| v.as_str()));
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    // ── Symbol level: bounded, target-pushed-down path (nw-079) ───────────────
    // Symbol summaries are O(symbols × per-symbol caller/callee queries). An
    // untargeted call over a large graph used to hang for tens of seconds and
    // never cache (so every call re-hung). Route symbol level through the bounded
    // engine API instead: a `target` filters BEFORE the expensive queries (fast),
    // and an untargeted call is hard-capped and reports the truncation honestly.
    // We deliberately do NOT touch the sidecar cache here — a targeted or capped
    // result is a partial set and must never be persisted as "the" summaries.
    if level == SummaryLevel::Symbol {
        let out = nestweaver_engine::generate_symbol_summaries_bounded(
            store,
            target,
            nestweaver_engine::DEFAULT_SYMBOL_SUMMARY_CAP,
        )?;
        let matched_total = out.matched_total;
        let capped = out.capped;
        let display: Vec<nestweaver_engine::Summary> = if let Some(budget) = token_budget {
            truncate_to_budget(&out.summaries, budget)
                .into_iter()
                .cloned()
                .collect()
        } else {
            out.summaries
        };
        let total_tokens: usize = display.iter().map(|s| s.token_estimate).sum();
        let truncated_by_budget = display.len() < matched_total;
        let note = if capped {
            Some(format!(
                "symbol-level summary is capped at {} symbols of {matched_total}; pass `target` \
                 (a symbol name or file substring) to summarize a specific area",
                nestweaver_engine::DEFAULT_SYMBOL_SUMMARY_CAP
            ))
        } else if target.is_some() && display.is_empty() {
            Some(format!(
                "no symbol matched target {:?}",
                target.unwrap_or_default()
            ))
        } else {
            None
        };
        return Ok(json!({
            "level": level_str,
            "target": target,
            "count": display.len(),
            "total_available": matched_total,
            "tokens_used": total_tokens,
            "token_budget": token_budget,
            "truncated": truncated_by_budget || capped,
            "partial": capped,
            "cached": false,
            "note": note,
            "summaries": render_text(&display),
        }));
    }

    // Try loading cached summaries from the sidecar first; only use the
    // cache when it contains entries at the requested level.
    let db_path = match current_db_path(store) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("get_summary: db_path unavailable ({e}), sidecar cache disabled");
            None
        }
    };
    // `no_cache` / `cache: "bypass"` must skip the summary sidecar too, not just
    // the F16 response cache — otherwise a caller asking for fresh data got
    // `cached: true` served from the sidecar, which reads as contradictory.
    let bypass = cache_bypassed(&args);
    let (summaries, from_cache) = if let Some(ref db) = db_path
        && !bypass
        && let Ok(Some(cached)) = load_summaries(db, store.graph_generation())
    {
        let level_filtered: Vec<nestweaver_engine::Summary> =
            cached.into_iter().filter(|s| s.level == level).collect();
        if level_filtered.is_empty() {
            tracing::debug!(
                level = level_str,
                "sidecar has summaries but none at requested level; regenerating"
            );
            let fresh = generate_summaries(store, level)?;
            (fresh, false)
        } else {
            (level_filtered, true)
        }
    } else {
        let fresh = generate_summaries(store, level)?;
        (fresh, false)
    };

    // Persist freshly generated summaries so subsequent calls hit the cache,
    // preserving cached entries at other levels (shared invariant).
    if !from_cache && let Some(ref db) = db_path {
        merge_and_save_summaries(db, store.graph_generation(), level, &summaries);
    }

    let total_available = summaries.len();

    // Build the display list: filter by target, then truncate by budget.
    let after_filter: Vec<nestweaver_engine::Summary> = if let Some(t) = target {
        filter_by_target(&summaries, t)
            .into_iter()
            .cloned()
            .collect()
    } else {
        summaries
    };

    let after_filter_len = after_filter.len();
    let display: Vec<nestweaver_engine::Summary> = if let Some(budget) = token_budget {
        truncate_to_budget(&after_filter, budget)
            .into_iter()
            .cloned()
            .collect()
    } else {
        after_filter
    };

    let total_tokens: usize = display.iter().map(|s| s.token_estimate).sum();
    let text = render_text(&display);

    Ok(json!({
        "level": level_str,
        "target": target,
        "count": display.len(),
        "total_available": total_available,
        "tokens_used": total_tokens,
        "token_budget": token_budget,
        "truncated": display.len() < after_filter_len,
        "cached": from_cache,
        "summaries": text,
    }))
}

/// Expand a leading `~/` to the user's home directory. Returns the input
/// unchanged when no expansion is possible.
#[cfg(not(feature = "daemon"))]
fn expand_tilde(input: &str) -> String {
    if let Some(stripped) = input.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{stripped}");
    }
    input.to_string()
}

/// Shallow check: does the directory contain any `.md` file in its tree?
/// Bounded depth to avoid blowing time on huge monorepos.
#[cfg(not(feature = "daemon"))]
fn walk_has_markdown(root: &Path) -> bool {
    fn recurse(p: &Path, depth: u32) -> bool {
        if depth > 4 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(p) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && (ext == "md" || ext == "markdown")
                {
                    return true;
                }
            } else if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                if recurse(&path, depth + 1) {
                    return true;
                }
            }
        }
        false
    }
    recurse(root, 0)
}

// The store doesn't expose its file path. We carry it on the server side
// in a thread-local so add_source can re-open. Set by `lib.rs` before
// dispatching tool calls.
thread_local! {
    static CURRENT_DB_PATH: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static CURRENT_INSTANCE_CONFIG: std::cell::RefCell<Option<std::sync::Arc<nestweaver_engine::InstanceConfig>>> =
        const { std::cell::RefCell::new(None) };
    static ALLOW_ADD_SOURCES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static LITE_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ALLOWED_TOOLS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
    static TRACK_INTERACTIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SERVER_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // F16 response cache: size cap (MiB) and per-session hit/miss counters.
    static CACHE_MAX_SIZE_MB: std::cell::Cell<u64> =
        const { std::cell::Cell::new(nestweaver_store::cache::DEFAULT_MAX_SIZE_MB) };
    static CACHE_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CACHE_MISSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    // In-process cache map: db_path → ResponseCache (avoids per-call disk I/O).
    static RESPONSE_CACHE: std::cell::RefCell<
        std::collections::HashMap<std::path::PathBuf, nestweaver_store::cache::ResponseCache>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    // Counts cache misses since last flush; flush to disk every N misses.
    static FLUSH_COUNTER: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub fn set_current_db_path(path: std::path::PathBuf) {
    CURRENT_DB_PATH.with(|c| *c.borrow_mut() = Some(path));
}

/// Install the parsed `InstanceConfig` for the current dispatch context.
/// Callers (daemon, MCP server) set this once per dispatch so tools like
/// `brain_search` can apply Feature F6 `[ranking]` priors without re-parsing
/// the file. Pass `None` to clear.
pub fn set_current_instance_config(cfg: Option<std::sync::Arc<nestweaver_engine::InstanceConfig>>) {
    CURRENT_INSTANCE_CONFIG.with(|c| *c.borrow_mut() = cfg);
}

/// Retrieve the parsed `InstanceConfig` installed for this dispatch, if any.
pub(crate) fn current_instance_config() -> Option<std::sync::Arc<nestweaver_engine::InstanceConfig>>
{
    CURRENT_INSTANCE_CONFIG.with(|c| c.borrow().clone())
}

/// Read the configured default result limit from the instance config's
/// `[limits]` section, falling back to the compile-time constant if no
/// instance config is installed for this dispatch context.
fn configured_result_limit() -> usize {
    current_instance_config()
        .map(|cfg| cfg.limits.default_result_limit)
        .unwrap_or(DEFAULT_RESULT_LIMIT)
}

/// Read the configured `[response]` settings, falling back to defaults if no
/// instance config is installed for this dispatch context.
fn configured_response() -> nestweaver_engine::ResponseConfig {
    current_instance_config()
        .map(|cfg| cfg.response.clone())
        .unwrap_or_default()
}

/// Set the F16 response-cache size cap in MiB (from `[cache] max_size_mb`).
pub fn set_cache_max_size_mb(mb: u64) {
    CACHE_MAX_SIZE_MB.with(|c| c.set(mb));
}

pub fn set_allow_add_sources(allowed: bool) {
    ALLOW_ADD_SOURCES.with(|c| c.set(allowed));
}

pub fn set_lite_mode(lite: bool) {
    LITE_MODE.with(|c| c.set(lite));
}

pub fn set_allowed_tools(names: Vec<String>) {
    ALLOWED_TOOLS.with(|c| *c.borrow_mut() = Some(names));
}

pub fn is_lite_mode() -> bool {
    LITE_MODE.with(|c| c.get())
}

pub fn set_track_interactions(track: bool) {
    TRACK_INTERACTIONS.with(|c| c.set(track));
}

pub fn is_track_interactions() -> bool {
    TRACK_INTERACTIONS.with(|c| c.get())
}

/// Mark this dispatch context as running in server mode (no local source files).
/// When set, tools like `read_symbols` add a note explaining that source spans
/// may be empty because the server indexes bare clones without checkout trees.
pub fn set_server_mode(server: bool) {
    SERVER_MODE.with(|c| c.set(server));
}

pub fn is_server_mode() -> bool {
    SERVER_MODE.with(|c| c.get())
}

// ── Daemon proxy dispatch ─────────────────────────────────────────────────

/// The tonic-generated gRPC client type for the `NestWeaverDaemon` service.
/// Re-exported so callers (e.g. `main.rs`) can construct it and pass it in
/// without depending on `nestweaver-proto` directly.
#[cfg(feature = "daemon")]
pub type DaemonGrpcClient =
    nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient<tonic::transport::Channel>;

/// Render post-commit reconciliation failures as human-readable warning strings
/// for an MCP `reconciliation_warnings` field (nw-091 / Bug 2).
#[cfg(feature = "daemon")]
fn reconciliation_warnings(failures: &[nestweaver_proto::ReconciliationFailure]) -> Vec<String> {
    failures
        .iter()
        .map(|f| {
            if f.repo_uid.is_empty() {
                format!("{}: {}", f.stage, f.message)
            } else {
                format!("{} ({}): {}", f.stage, f.repo_uid, f.message)
            }
        })
        .collect()
}

#[cfg(feature = "daemon")]
fn daemon_brain_search_response_to_json(
    response: &nestweaver_proto::BrainSearchResponse,
    concise: bool,
) -> Value {
    let results: Vec<Value> = response
        .results
        .iter()
        .map(|result| {
            let mut item = if concise {
                let mut item = json!({
                    "uid": result.uid,
                    "kind": result.kind,
                    "title": result.title,
                    "matched_headings": result.matched_headings,
                });
                if let Some(location) = &result.location {
                    item["location"] = json!(location);
                }
                item
            } else {
                let mut item = json!({
                    "uid": result.uid,
                    "kind": result.kind,
                    "title": result.title,
                    "score": result.score,
                    "matched_headings": result.matched_headings,
                });
                if let Some(location) = &result.location {
                    item["location"] = json!(location);
                }
                if let Some(body) = &result.inline_body {
                    item["inline_body"] = json!(body);
                }
                item
            };
            if let Some(canonical_id) = &result.canonical_id {
                item["canonical_id"] = json!(canonical_id);
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
    let mut value = json!({
        "query": response.query,
        "engine": response.engine,
        "total_matches": response.total_matches,
        "total_matches_relation": relation,
        "returned_matches": returned_matches,
        "truncated": truncated,
        "results": results,
    });
    if !response.expansion_terms.is_empty() {
        value["expansion_terms"] = json!(response.expansion_terms);
    }
    value
}

/// Dispatch an MCP tool call through the daemon gRPC service instead of
/// opening the DB directly. Maps each MCP tool name to the corresponding
/// gRPC RPC on the `NestWeaverDaemon` service.
///
/// The caller is responsible for connecting the gRPC client (typically via
/// `nestweaver_client::DaemonClient`) and passing the inner tonic client.
#[cfg(feature = "daemon")]
pub fn dispatch_via_daemon(
    client: &mut DaemonGrpcClient,
    rt: &tokio::runtime::Runtime,
    name: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    use nestweaver_proto::JsonRequest;

    validate_tool_arguments(name, &args)?;

    let args_json = serde_json::to_string(&args)?;

    // brain_add_source is special: it maps to IndexRepo or IndexVault
    // (streaming RPCs) depending on the path content.
    if name == "brain_add_source" {
        return dispatch_add_source_via_daemon(client, rt, args);
    }

    // brain_remove_source uses typed RemoveRepo/RemoveVault RPCs.
    if name == "brain_remove_source" {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("'target' is required"))?
            .to_string();

        // nw-089: RESOLVE the target (path / name / url / uid) to the actual repo
        // (or vault) UID before deleting. The RPC's repo_uid is an exact uid, so
        // the previous code that sent the raw `target` never matched a path/name
        // and silently deleted nothing (a no-op reported as success). Fetch the
        // repo list from the daemon and match with the same logic the CLI uses.
        let repos: Vec<nestweaver_schema::Repo> = {
            let req = tonic::Request::new(nestweaver_proto::JsonRequest {
                args_json: "{}".to_string(),
            });
            let resp = rt
                .block_on(client.list_repos_json(req))
                .map_err(|s| anyhow::anyhow!("list_repos RPC failed: {}", s.message()))?;
            serde_json::from_str(&resp.into_inner().result_json).unwrap_or_default()
        };
        let matched_repo = match_repo_target(&repos, &target);
        if matched_repo.len() > 1 {
            return Err(anyhow::anyhow!(
                "'{target}' matches multiple repos — pass a full UID to disambiguate."
            ));
        }
        if let Some(repo) = matched_repo.first() {
            let resp = rt
                .block_on(client.remove_repo(nestweaver_proto::RemoveRepoRequest {
                    repo_uid: repo.uid.clone(),
                }))
                .map_err(|s| anyhow::anyhow!("remove_repo RPC failed: {}", s.message()))?;
            let inner = resp.into_inner();
            return Ok(json!({
                "kind": "repo",
                "name": repo.name.clone().unwrap_or_else(|| repo.url.clone()),
                "uid": repo.uid,
                "files_deleted": inner.files_deleted,
                "symbols_deleted": inner.symbols_deleted,
                // nw-091 / Bug 2: committed=true means the delete HAPPENED even if
                // some post-commit reconciliation step failed — surfaced as warnings,
                // never as an RPC error that reads as "nothing happened".
                "committed": inner.committed,
                "reconciliation_warnings": reconciliation_warnings(&inner.reconciliation_failures),
            }));
        }

        // Not a repo — resolve as a vault (by uid, name, or root path).
        let vaults: Vec<nestweaver_schema::Vault> = {
            let req = tonic::Request::new(nestweaver_proto::JsonRequest {
                args_json: "{}".to_string(),
            });
            match rt.block_on(client.list_vaults_json(req)) {
                Ok(resp) => {
                    serde_json::from_str(&resp.into_inner().result_json).unwrap_or_default()
                }
                Err(_) => Vec::new(),
            }
        };
        let canonical_target = std::fs::canonicalize(&target)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let target_trimmed = target.trim_end_matches('/');
        let matched_vault: Vec<&nestweaver_schema::Vault> = vaults
            .iter()
            .filter(|v| {
                v.uid == target
                    || v.name == target_trimmed
                    || v.root_path.trim_end_matches('/') == target_trimmed
                    || (!canonical_target.is_empty()
                        && v.root_path.trim_end_matches('/')
                            == canonical_target.trim_end_matches('/'))
            })
            .collect();
        if matched_vault.len() > 1 {
            return Err(anyhow::anyhow!(
                "'{target}' matches multiple vaults — pass a full UID to disambiguate."
            ));
        }
        if let Some(vault) = matched_vault.first() {
            let resp = rt
                .block_on(client.remove_vault(nestweaver_proto::RemoveVaultRequest {
                    vault_uid: vault.uid.clone(),
                }))
                .map_err(|s| anyhow::anyhow!("remove_vault RPC failed: {}", s.message()))?;
            let inner = resp.into_inner();
            return Ok(json!({
                "kind": "vault",
                "name": vault.name.clone(),
                "uid": vault.uid,
                "notes_deleted": inner.notes_deleted,
                "committed": inner.committed,
                "reconciliation_warnings": reconciliation_warnings(&inner.reconciliation_failures),
            }));
        }

        return Err(anyhow::anyhow!(
            "no repo or vault matching '{target}' found"
        ));
    }

    // prune_stale uses a typed PruneStaleRequest RPC.
    if name == "prune_stale" {
        let resp = rt
            .block_on(client.prune_stale(nestweaver_proto::PruneStaleRequest {}))
            .map_err(|e| anyhow::anyhow!("prune_stale RPC failed: {}", e.message()))?;
        let inner = resp.into_inner();
        return Ok(json!({
            "removed_repos": inner.removed_repos,
            "removed_vaults": inner.removed_vaults
        }));
    }

    // Helper to parse string arrays from JSON args.
    let str_array = |key: &str| -> Vec<String> {
        args.get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let str_field = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let i32_field =
        |key: &str| -> i32 { args.get(key).and_then(|v| v.as_i64()).unwrap_or(0) as i32 };
    let opt_str_field = |key: &str| -> Option<String> {
        let v = str_field(key);
        if v.is_empty() { None } else { Some(v) }
    };
    let bool_field =
        |key: &str| -> bool { args.get(key).and_then(|v| v.as_bool()).unwrap_or(false) };
    let f64_field = |key: &str| -> f64 { args.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0) };

    let result_json: String = rt.block_on(async {
        match name {
            // ── Typed hot-path RPCs ──────────────────────────────────
            "brain_search" => {
                use nestweaver_proto::BrainSearchRequest;
                let req = tonic::Request::new(BrainSearchRequest {
                    query: str_field("query"),
                    limit: i32_field("limit"),
                    response_format: opt_str_field("response_format"),
                    include_bodies: bool_field("include_bodies"),
                    prf: bool_field("prf"),
                    rerank: bool_field("rerank"),
                    root: opt_str_field("root"),
                });
                let resp = client
                    .search(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
                let inner = resp.into_inner();
                let is_concise = str_field("response_format").eq_ignore_ascii_case("concise");
                let value = daemon_brain_search_response_to_json(&inner, is_concise);
                Ok(serde_json::to_string(&value)?)
            }
            "brain_context" => {
                use nestweaver_proto::BrainContextRequest;
                let req = tonic::Request::new(BrainContextRequest {
                    seeds: str_array("seeds"),
                    token_budget: i32_field("token_budget"),
                    response_format: str_field("response_format"),
                    repos: str_array("repos"),
                    vaults: str_array("vaults"),
                    kinds: str_array("kinds"),
                    path_prefix: str_field("path_prefix"),
                    tags: str_array("tags"),
                    exclude_tags: str_array("exclude_tags"),
                    weight_ppr: f64_field("weight_ppr"),
                    weight_bm25: f64_field("weight_bm25"),
                    intent: str_field("intent"),
                    include_seeds: bool_field("include_seeds"),
                    include_bodies: bool_field("include_bodies"),
                    root: str_field("root"),
                    prf: bool_field("prf"),
                    rerank: bool_field("rerank"),
                    weight_semantic: f64_field("weight_semantic"),
                    since: str_field("since"),
                    recency_weight: f64_field("recency_weight"),
                    recency_half_life_days: f64_field("recency_half_life_days"),
                });
                let resp = client
                    .get_context(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
                Ok(resp.into_inner().result_json)
            }
            "project_context" => {
                use nestweaver_proto::ProjectContextRequest;
                let req = tonic::Request::new(ProjectContextRequest {
                    project: str_field("project"),
                    token_budget: i32_field("token_budget"),
                    kinds: str_array("kinds"),
                    include_components: bool_field("include_components"),
                    intent: str_field("intent"),
                    include_seeds: bool_field("include_seeds"),
                    since: str_field("since"),
                    recency_weight: f64_field("recency_weight"),
                    recency_half_life_days: f64_field("recency_half_life_days"),
                    response_format: str_field("response_format"),
                    repos: str_array("repos"),
                    path_prefix: str_field("path_prefix"),
                    tags: str_array("tags"),
                    exclude_tags: str_array("exclude_tags"),
                });
                let resp = client
                    .get_project_context(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
                Ok(resp.into_inner().result_json)
            }
            "note_get" => {
                use nestweaver_proto::NoteGetRequest;
                let req = tonic::Request::new(NoteGetRequest {
                    uid: opt_str_field("uid"),
                    title: opt_str_field("title"),
                    include_body: bool_field("include_body"),
                    sections: str_array("sections"),
                });
                let resp = client
                    .get_note(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
                let inner = resp.into_inner();
                let mut value = serde_json::json!({
                    "uid": inner.uid,
                    "title": inner.title,
                    "path": inner.path,
                    "note_kind": inner.note_kind,
                    "word_count": inner.word_count,
                    "section_count": inner.section_count,
                });
                if let Some(ref body) = inner.body {
                    value["body"] = serde_json::json!(body);
                }
                Ok(serde_json::to_string(&value)?)
            }
            "brain_status" => {
                // Use the JSON pass-through RPC so per-vault rows (uid +
                // instance_id), warnings[], and any other engine-side fields
                // round-trip intact. The typed BrainStatusResponse only
                // carries the scalar totals.
                let req = tonic::Request::new(JsonRequest {
                    args_json: args_json.clone(),
                });
                let resp = client
                    .brain_status_json(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
                Ok(resp.into_inner().result_json)
            }
            "hub_nodes" => {
                use nestweaver_proto::HubNodesRequest;
                let req = tonic::Request::new(HubNodesRequest {
                    // The schema advertises 'limit'; 'top_n' kept as a
                    // backward-compat alias (and it is the proto field name).
                    top_n: args
                        .get("limit")
                        .or_else(|| args.get("top_n"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as i32,
                    response_format: str_field("response_format"),
                });
                let resp = client
                    .hub_nodes(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
                Ok(resp.into_inner().result_json)
            }
            // ── JSON pass-through RPCs ───────────────────────────────
            other => {
                let req = tonic::Request::new(JsonRequest {
                    args_json: args_json.clone(),
                });
                let resp = match other {
                    "backlinks" => client.get_backlinks(req).await,
                    "brain_impact" => client.impact(req).await,
                    "brain_guide" => client.brain_guide(req).await,
                    "flow_trace" => client.flow_trace(req).await,
                    "blast_radius" => client.blast_radius(req).await,
                    "detect_changes" => client.detect_changes(req).await,
                    "brain_diff" => client.brain_diff(req).await,
                    "read_symbols" => client.read_symbols(req).await,
                    "regex_search" => client.regex_search(req).await,
                    "count_patterns" => client.count_patterns(req).await,
                    "cross_repo_contracts" => client.cross_repo_contracts(req).await,
                    "contract_drift" => client.contract_drift(req).await,
                    "dead_code" => client.dead_code(req).await,
                    "brain_broken_links" => client.brain_broken_links(req).await,
                    "brain_orphan_documents" => client.brain_orphan_documents(req).await,
                    "brain_topic_clusters" => client.brain_topic_clusters(req).await,
                    "brain_tag_graph" => client.brain_tag_graph(req).await,
                    "brain_doc_stats" => client.brain_doc_stats(req).await,
                    "brain_memory_lint" => client.brain_memory_lint(req).await,
                    "brain_memory_consolidate" => client.brain_memory_consolidate(req).await,
                    "brain_memory_related" => client.brain_memory_related(req).await,
                    "affected_tests" => client.affected_tests(req).await,
                    "clusters" => client.clusters(req).await,
                    "stale_check" => client.stale_check(req).await,
                    "bridge_nodes" => client.bridge_nodes(req).await,
                    "get_summary" => client.get_summary(req).await,
                    "investigate" => client.investigate(req).await,
                    "investigate_expand" => client.investigate_expand(req).await,
                    "investigate_hydrate" => client.investigate_hydrate(req).await,
                    "set_extension" => client.set_extension(req).await,
                    "query_extensions" => client.query_extensions(req).await,
                    unknown => {
                        return Err(anyhow::anyhow!(
                            "unknown tool for daemon dispatch: {unknown}"
                        ));
                    }
                };
                let resp = resp.map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
                Ok(resp.into_inner().result_json)
            }
        }
    })?;

    serde_json::from_str(&result_json).map_err(Into::into)
}

/// Handle `brain_add_source` by routing to `IndexRepo` or `IndexVault`
/// streaming RPCs. Detection order matches the non-daemon path:
/// 1. `.git/` present → code repo (IndexRepo)
/// 2. `.obsidian/` present OR contains `.md` files → vault/markdown (IndexVault)
#[cfg(feature = "daemon")]
#[derive(Clone, Copy)]
enum DaemonIndexSource {
    Repo,
    Vault,
}

#[cfg(feature = "daemon")]
impl DaemonIndexSource {
    fn label(self) -> &'static str {
        match self {
            Self::Repo => "repository",
            Self::Vault => "vault",
        }
    }
}

#[cfg(feature = "daemon")]
async fn consume_daemon_index_progress<S>(
    source: DaemonIndexSource,
    mut stream: S,
) -> Result<String, anyhow::Error>
where
    S: tokio_stream::Stream<Item = Result<nestweaver_proto::IndexProgress, tonic::Status>> + Unpin,
{
    nestweaver_proto::consume_index_progress(&mut stream, |_| {})
        .await
        .map_err(|error| anyhow::anyhow!("{} index failed: {error}", source.label()))
}

/// Heuristic vault detector for `brain_add_source`: true when markdown files are
/// the majority of the "content" files in a bounded shallow scan of `dir`. Used
/// to distinguish a notes vault from a code directory so a code dir without a
/// `.git` isn't misclassified as a vault (nw-089). Bounded (depth ≤ 3, ≤ 4000
/// files, skips VCS/dependency dirs) so it stays cheap on large trees.
#[cfg(feature = "daemon")]
fn dir_is_markdown_dominant(dir: &std::path::Path) -> bool {
    const CODE_EXTS: &[&str] = &[
        "rs", "js", "ts", "jsx", "tsx", "py", "go", "java", "c", "h", "cpp", "hpp", "cc", "cs",
        "rb", "php", "swift", "kt", "scala", "lua", "sh", "sql", "hcl", "dart", "ex", "exs", "zig",
        "m", "mm",
    ];
    let mut md = 0usize;
    let mut code = 0usize;
    let mut seen = 0usize;
    let mut stack = vec![(dir.to_path_buf(), 0u32)];
    while let Some((d, depth)) = stack.pop() {
        if depth > 3 || seen > 4000 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            seen += 1;
            if seen > 4000 {
                break;
            }
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.starts_with('.')
                    || matches!(
                        name,
                        "node_modules" | "target" | "vendor" | "dist" | "build"
                    )
                {
                    continue;
                }
                stack.push((p, depth + 1));
            } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                if ext == "md" || ext == "markdown" {
                    md += 1;
                } else if CODE_EXTS.contains(&ext) {
                    code += 1;
                }
            }
        }
    }
    md > 0 && md >= code
}

#[cfg(feature = "daemon")]
fn dispatch_add_source_via_daemon(
    client: &mut DaemonGrpcClient,
    rt: &tokio::runtime::Runtime,
    args: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("'path' is required for brain_add_source"))?
        .to_string();

    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let resolved = std::path::Path::new(&path);
    // nw-089: validate the path exists — the old code returned a phantom
    // `status:"indexed"` vault for a nonexistent path (no validation).
    if !resolved.exists() {
        return Err(anyhow::anyhow!("path does not exist: {path}"));
    }
    if !resolved.is_dir() {
        return Err(anyhow::anyhow!(
            "path is not a directory (index a source directory, not a file): {path}"
        ));
    }
    // nw-089: classify by a POSITIVE vault signal, then default to CODE. The old
    // `if is_vault || !is_repo` indexed any non-git directory as a vault, so a
    // plain code dir without a `.git` was indexed as a vault and its source was
    // never picked up (vault indexing only ingests markdown). A directory is a
    // vault only when it is an Obsidian vault or is markdown-dominant; everything
    // else — including a code dir with no `.git` — indexes as code.
    let is_vault = resolved.join(".obsidian").exists() || dir_is_markdown_dominant(resolved);

    let instance_id = current_instance_config()
        .map(|c| c.instance_id.clone())
        .unwrap_or_default();

    rt.block_on(async {
        if is_vault {
            let req = tonic::Request::new(nestweaver_proto::IndexVaultRequest {
                vault_path: path.clone(),
                vault_name: name,
                extra_ignore_patterns: vec![],
                instance_id: instance_id.clone(),
            });
            let stream = client
                .index_vault(req)
                .await
                .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?
                .into_inner();
            let last_msg = consume_daemon_index_progress(DaemonIndexSource::Vault, stream).await?;
            Ok(serde_json::json!({
                "status": "indexed",
                "path": path,
                "type": "vault",
                "message": last_msg,
            }))
        } else {
            let req = tonic::Request::new(nestweaver_proto::IndexRepoRequest {
                repo_path: path.clone(),
                name,
                force: false,
                with_trigrams: false,
                with_git_activity: false,
                // nw-019: no explicit instance here — let the daemon decide
                // (config's logical name, else runtime hash).
                instance_id: String::new(),
            });
            let stream = client
                .index_repo(req)
                .await
                .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?
                .into_inner();
            let last_msg = consume_daemon_index_progress(DaemonIndexSource::Repo, stream).await?;
            Ok(serde_json::json!({
                "status": "indexed",
                "path": path,
                "type": "repo",
                "message": last_msg,
            }))
        }
    })
}

// ── F10: investigate bundle primitive ─────────────────────────────────────

#[cfg(all(test, feature = "daemon"))]
mod daemon_index_progress_tests {
    use super::*;
    use nestweaver_proto::{IndexProgress, Phase};

    fn progress(phase: Phase, message: &str) -> Result<IndexProgress, tonic::Status> {
        Ok(IndexProgress {
            phase: phase as i32,
            message: message.to_string(),
            ..Default::default()
        })
    }

    async fn outcome_for(
        source: DaemonIndexSource,
        events: Vec<Result<IndexProgress, tonic::Status>>,
    ) -> Result<String, anyhow::Error> {
        consume_daemon_index_progress(source, tokio_stream::iter(events)).await
    }

    #[tokio::test]
    async fn repo_and_vault_accept_only_a_done_terminated_stream() {
        for source in [DaemonIndexSource::Repo, DaemonIndexSource::Vault] {
            let message = outcome_for(
                source,
                vec![
                    progress(Phase::Discovering, "scanning"),
                    progress(Phase::Writing, "writing"),
                    progress(Phase::Done, "indexed successfully"),
                ],
            )
            .await
            .expect("Done must be accepted");

            assert_eq!(message, "indexed successfully");
        }
    }

    #[tokio::test]
    async fn repo_and_vault_reject_an_explicit_error_with_its_message() {
        for source in [DaemonIndexSource::Repo, DaemonIndexSource::Vault] {
            let error = outcome_for(
                source,
                vec![
                    progress(Phase::Discovering, "scanning"),
                    progress(Phase::Error, "parser exploded"),
                ],
            )
            .await
            .unwrap_err()
            .to_string();

            assert!(error.contains(source.label()));
            assert!(error.contains("parser exploded"));
        }
    }

    #[tokio::test]
    async fn repo_and_vault_reject_empty_and_truncated_streams() {
        for source in [DaemonIndexSource::Repo, DaemonIndexSource::Vault] {
            let empty = outcome_for(source, vec![]).await.unwrap_err().to_string();
            assert!(empty.contains(source.label()));
            assert!(empty.contains("empty"), "unexpected error: {empty}");

            let truncated =
                outcome_for(source, vec![progress(Phase::Discovering, "still scanning")])
                    .await
                    .unwrap_err()
                    .to_string();
            assert!(truncated.contains(source.label()));
            assert!(
                truncated.contains("before completion"),
                "unexpected error: {truncated}"
            );
        }
    }

    #[tokio::test]
    async fn repo_and_vault_preserve_transport_errors() {
        for source in [DaemonIndexSource::Repo, DaemonIndexSource::Vault] {
            let error = outcome_for(
                source,
                vec![
                    progress(Phase::Discovering, "scanning"),
                    Err(tonic::Status::unavailable("connection reset")),
                ],
            )
            .await
            .unwrap_err()
            .to_string();

            assert!(error.contains(source.label()));
            assert!(error.contains("connection reset"));
        }
    }

    #[tokio::test]
    async fn repo_and_vault_reject_events_after_done() {
        for source in [DaemonIndexSource::Repo, DaemonIndexSource::Vault] {
            let error = outcome_for(
                source,
                vec![
                    progress(Phase::Done, "done"),
                    progress(Phase::Writing, "late write"),
                ],
            )
            .await
            .unwrap_err()
            .to_string();

            assert!(error.contains(source.label()));
            assert!(error.contains("after terminal Done"));
            assert!(error.contains("late write"));
        }
    }

    #[tokio::test]
    async fn repo_and_vault_reject_events_after_error() {
        for source in [DaemonIndexSource::Repo, DaemonIndexSource::Vault] {
            let error = outcome_for(
                source,
                vec![
                    progress(Phase::Error, "first failure"),
                    progress(Phase::Done, "late done"),
                ],
            )
            .await
            .unwrap_err()
            .to_string();

            assert!(error.contains(source.label()));
            assert!(error.contains("after terminal Error"));
            assert!(error.contains("first failure"));
            assert!(error.contains("late done"));
        }
    }
}

/// Resolve the source root for body reads: explicit `root` arg, else cwd.
fn arg_root(args: &Value) -> std::path::PathBuf {
    args.get("root")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn tool_schema_investigate() -> Value {
    json!({
        "name": "investigate",
        "description": "Orient on an unfamiliar topic in ONE call: runs hybrid PPR+BM25 retrieval, groups results into architectural domains, inlines high-confidence source bodies, and returns a token-budgeted map with a bundle_id for drill-down.\n\nGuidelines:\n- Use scope 'project:<slug>' or 'repo:<name>' to restrict; omit for unrestricted\n- Drill into entries with investigate_expand (by asset_id) or fill all bodies with investigate_hydrate\n- more_available counts entries dropped by token budget — raise token_budget to see them\n\nLimitations:\n- Token budget hard-capped at 16000\n- Bundles expire 24h after creation",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The topic/feature/subsystem to orient on (e.g. \"device pairing\", \"how indexing works\")." },
                "scope": { "type": "string", "description": "Optional scope: \"project:<slug>\", \"repo:<name>\", or \"vault\"/\"all\" (default = no restriction)." },
                "token_budget": { "type": "integer", "default": 4000, "description": "Approximate token cap for the map (chars/4). Hard-capped at 16000." },
                "root": { "type": "string", "description": "Filesystem root for reading inline source bodies. Defaults to the server's working directory." }
            },
            "required": ["query"]
        }
    })
}

fn tool_investigate(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    args: Value,
    embed_model: Option<&dyn EmbedQueryFn>,
) -> Result<Value, anyhow::Error> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'query' must be a string"))?;
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("vault");
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let root = arg_root(&args);
    let db_path = current_db_path(store).ok();
    let result = investigate(
        store,
        tantivy,
        db_path.as_deref(),
        &root,
        query,
        scope,
        token_budget,
        embed_model,
    )?;
    Ok(serde_json::to_value(result)?)
}

fn tool_schema_investigate_expand() -> Value {
    json!({
        "name": "investigate_expand",
        "description": "Drill into specific investigate map entries: fetch full source bodies and immediate neighbors (callers/callees for symbols, wikilink sources for notes).\n\nGuidelines:\n- Pass bundle_id from a prior investigate call and target asset_ids or raw node uids\n- Expanded entries always have body_complete: true (full untruncated body)\n- Unresolved targets are returned in the unresolved array\n\nLimitations:\n- Requires a valid bundle_id from a prior investigate call\n- Bundles expire 24h after creation",
        "inputSchema": {
            "type": "object",
            "properties": {
                "bundle_id": { "type": "string", "description": "The bundle_id returned by a prior `investigate` call." },
                "targets": { "type": "array", "items": { "type": "string" }, "description": "asset_ids (from the investigate map) or raw node uids to expand." },
                "root": { "type": "string", "description": "Filesystem root for reading source bodies. Defaults to the server's working directory." }
            },
            "required": ["bundle_id", "targets"]
        }
    })
}

fn tool_investigate_expand(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let bundle_id = args
        .get("bundle_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'bundle_id' must be a string"))?;
    let targets: Vec<String> = args
        .get("targets")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if targets.is_empty() {
        return Err(anyhow!("'targets' must be a non-empty array"));
    }
    let root = arg_root(&args);
    let db_path = current_db_path(store)?;
    let result = investigate_expand(store, &db_path, &root, bundle_id, &targets)?;
    Ok(serde_json::to_value(result)?)
}

fn tool_schema_investigate_hydrate() -> Value {
    json!({
        "name": "investigate_hydrate",
        "description": "Fill in source bodies for all un-hydrated entries in an investigate bundle — the bulk version of investigate_expand, budget-bounded.\n\nGuidelines:\n- Pass bundle_id from a prior investigate call; bodies are read up to token_budget\n- body_complete: true means full source inlined; false means truncated (use read_symbols for the rest)\n- Token budget hard-capped at 16000\n\nLimitations:\n- Requires a valid bundle_id from a prior investigate call\n- Bundles expire 24h after creation",
        "inputSchema": {
            "type": "object",
            "properties": {
                "bundle_id": { "type": "string", "description": "The bundle_id returned by a prior `investigate` call." },
                "token_budget": { "type": "integer", "default": 4000, "description": "Approximate token cap for the hydrated bodies (chars/4). Hard-capped at 16000." },
                "root": { "type": "string", "description": "Filesystem root for reading source bodies. Defaults to the server's working directory." }
            },
            "required": ["bundle_id"]
        }
    })
}

fn tool_investigate_hydrate(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // hydrate is the BULK operation — it hydrates every un-hydrated entry in the
    // bundle and takes no per-entry selector. A caller passing `targets`/`uid`/
    // `uids` has confused it with investigate_expand; those keys were silently
    // ignored (a no-op that reads as "nothing to hydrate"), so reject them with a
    // pointer instead, matching investigate_expand's own strictness.
    for key in ["targets", "uid", "uids"] {
        if args.get(key).is_some() {
            return Err(anyhow!(
                "investigate_hydrate takes no '{key}' — it hydrates the whole bundle. \
                 Use investigate_expand with 'targets' to hydrate specific entries."
            ));
        }
    }
    let bundle_id = args
        .get("bundle_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'bundle_id' must be a string"))?;
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let root = arg_root(&args);
    let db_path = current_db_path(store)?;
    let result = investigate_hydrate(store, &db_path, &root, bundle_id, token_budget)?;
    Ok(serde_json::to_value(result)?)
}

fn current_db_path(_store: &GraphStore) -> Result<std::path::PathBuf, anyhow::Error> {
    CURRENT_DB_PATH.with(|c| {
        c.borrow()
            .clone()
            .ok_or_else(|| anyhow!("database path not set on server"))
    })
}

#[cfg(test)]
mod server_mode_tests {
    use super::*;

    // read_symbols in server mode must read source at the repo's recorded
    // indexed_sha, NOT the bare clone's HEAD — the daemon may have fetched past
    // the indexed commit, so HEAD would return the wrong commit's source for
    // symbol spans taken from the indexed graph. Build a bare repo whose HEAD
    // (v2) differs from indexed_sha (v1) and assert the reader returns v1.
    #[test]
    fn bare_reader_reads_indexed_sha_not_head() {
        use nestweaver_engine::content_reader::ContentReader;
        use std::process::Command;

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&src)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(src.join("a.txt"), "v1").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "c1"]);
        let sha1 = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&src)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(src.join("a.txt"), "v2").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "c2"]);

        // Bare clone at the URL-hashed path bare_reader_for_repo resolves to.
        let url = "https://github.com/example/twocommit";
        let workspace_root = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_root).unwrap();
        let clone_dir = nestweaver_engine::pull::clone_dir_name_from_url(url);
        let bare = workspace_root.join(format!("{clone_dir}.git"));
        Command::new("git")
            .args([
                "clone",
                "--bare",
                "-q",
                &src.display().to_string(),
                &bare.display().to_string(),
            ])
            .output()
            .unwrap();

        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo-tc".to_string(),
                url: url.to_string(),
                indexed_sha: sha1.clone(),
                staleness_commits_behind: 1,
                instance_id: "inst".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();

        let reader = bare_reader_for_repo(&store, &workspace_root, "repo-tc")
            .expect("reader for indexed repo");
        let content = reader.read_file(std::path::Path::new("a.txt")).unwrap();
        assert_eq!(
            content, "v1",
            "read_symbols must read source at indexed_sha (v1), not bare HEAD (v2)"
        );
    }

    // brain_status must report the ACTUAL server mode (the thread-local set by
    // the transport handler), not a hardcoded value. Regression guard for the
    // MCP-over-HTTP path reporting server_mode: false even when running
    // --server, which also masked the read_symbols empty-body bug.
    #[test]
    fn brain_status_reflects_server_mode_flag() {
        let store = GraphStore::in_memory().unwrap();

        set_server_mode(true);
        let status = tool_brain_status(&store, None).unwrap();
        assert_eq!(
            status["server_mode"],
            serde_json::json!(true),
            "brain_status should report server_mode=true when the flag is set"
        );

        set_server_mode(false);
        let status = tool_brain_status(&store, None).unwrap();
        assert_eq!(
            status["server_mode"],
            serde_json::json!(false),
            "brain_status should report server_mode=false when the flag is unset"
        );
    }
}

#[cfg(test)]
mod project_context_bug12_tests {
    use super::*;
    use nestweaver_schema::{Note, NoteKind, Project, Symbol, SymbolKind, Vault, Visibility};

    fn mk_note(uid: &str, vault_uid: &str, file_path: &str, title: &str) -> Note {
        Note {
            uid: uid.to_string(),
            vault_uid: vault_uid.to_string(),
            file_path: file_path.to_string(),
            title: title.to_string(),
            note_kind: NoteKind::General,
            word_count: 100,
            content_hash: format!("hash-{uid}"),
            frontmatter: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        }
    }

    fn mk_symbol(uid: &str, repo_uid: &str, file_path: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: file_path.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("hash-{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    // Bug #12: a project that declares repos (so it has a large
    // PROJECT_INCLUDES_SYMBOL fan-out) must still surface its curated member
    // notes through `project_context`. Notes are seeded into PPR — so they
    // land in `seeds`, disjoint from the rendered `connected` list — and must
    // be promoted into `connected`. This guards the wiring at the call site:
    // removing the promote step leaves notes in `seeds` only and fails here.
    #[test]
    fn project_context_surfaces_member_notes_when_project_has_symbol_mass() {
        let store = GraphStore::in_memory().unwrap();

        store
            .insert_vault(&Vault {
                uid: "vlt:t".into(),
                name: "t".into(),
                root_path: "/v".into(),
                instance_id: "default".into(),
            })
            .unwrap();

        let proj = Project {
            uid: "proj:pp".into(),
            name: "Parallel Paths".into(),
            summary: None,
            instance_id: "default".into(),
        };
        store.insert_project(&proj).unwrap();

        let note_uids = ["note:prd", "note:status", "note:arch"];
        for (i, n) in note_uids.iter().enumerate() {
            store
                .insert_note(&mk_note(
                    n,
                    "vlt:t",
                    &format!("Projects/parallel-paths/doc{i}.md"),
                    n,
                ))
                .unwrap();
        }

        // 50 symbols simulate an attached repo's code mass.
        let mut sym_uids: Vec<String> = Vec::new();
        for i in 0..50 {
            let uid = format!("sym:s{i}");
            store
                .insert_symbol(&mk_symbol(
                    &uid,
                    "repo:r",
                    &format!("src/f{i}.rs"),
                    &format!("fn{i}"),
                ))
                .unwrap();
            sym_uids.push(uid);
        }

        let note_edges: Vec<(&str, &str)> = note_uids.iter().map(|n| ("proj:pp", *n)).collect();
        store.batch_insert_project_note_edges(&note_edges).unwrap();
        store
            .batch_insert_project_symbol_edges("proj:pp", &sym_uids, 1.0)
            .unwrap();

        let resp = tool_project_context(
            &store,
            None,
            // response_format "detailed" so `connected` nodes carry `uid` (concise, the new
            // default, returns only kind/title/location); this test identifies notes by uid.
            json!({ "project": "Parallel Paths", "token_budget": 5000, "response_format": "detailed" }),
            None,
            None,
        )
        .unwrap();

        let connected = resp["connected"].as_array().expect("connected array");
        let uids: Vec<&str> = connected.iter().filter_map(|n| n["uid"].as_str()).collect();
        let hits = note_uids.iter().filter(|n| uids.contains(n)).count();
        assert_eq!(
            hits,
            note_uids.len(),
            "all {} member notes must surface in connected; got {uids:?}",
            note_uids.len()
        );
    }

    #[test]
    fn brain_context_surfaces_resolved_seed_when_no_neighbors() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&mk_symbol(
                "sym:repo:t:abc:leaf",
                "repo:t:abc",
                "src/leaf.rs",
                "LeafOnlySymbol",
            ))
            .unwrap();

        let resp = tool_brain_context(
            &store,
            None,
            json!({ "seeds": ["LeafOnlySymbol"], "token_budget": 5000 }),
            None,
            None,
        )
        .unwrap();

        let connected = resp["connected"].as_array().expect("connected array");
        assert!(
            connected
                .iter()
                .any(|n| n["title"].as_str() == Some("LeafOnlySymbol")),
            "resolved seed should be visible when it has no connected neighbors: {connected:?}"
        );
        assert_eq!(resp["seeds_expanded"].as_u64(), Some(1));
    }

    // Feature F8: brain_context with include_bodies embeds the source span of
    // high-relevance connected symbols inline; off by default it does not.
    #[test]
    fn brain_context_inline_bodies_opt_in() {
        use nestweaver_engine::index_directory_in_memory;
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(name) {\n  return hello(name);\n}\nfunction hello(n) {\n  return n;\n}\n",
        )
        .unwrap();
        let (_repo, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let root = src.to_string_lossy().to_string();

        // Off by default: no inline_body anywhere.
        let off = tool_brain_context(
            &store,
            None,
            json!({ "seeds": ["greet"], "token_budget": 5000, "include_seeds": true }),
            None,
            None,
        )
        .unwrap();
        let any_body_off = off["connected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.get("inline_body").is_some());
        assert!(!any_body_off, "default path must not embed inline bodies");

        // Opted in: at least the top connected symbol carries an inline_body
        // that contains its source span.
        let on = tool_brain_context(
            &store,
            None,
            json!({
                "seeds": ["greet"],
                "token_budget": 5000,
                "include_bodies": true,
                "root": root,
            }),
            None,
            None,
        )
        .unwrap();
        let connected = on["connected"].as_array().unwrap();
        assert!(!connected.is_empty(), "expected connected results");
        let bodied: Vec<&str> = connected
            .iter()
            .filter_map(|n| n.get("inline_body").and_then(|b| b.as_str()))
            .collect();
        assert!(
            bodied.iter().any(|b| b.contains("function")),
            "opted-in path should embed at least one symbol body; got connected={connected:?}"
        );
    }
}

// ── F16: response-cache dispatch tests ───────────────────────────────────────
#[cfg(test)]
mod cache_dispatch_tests {
    use super::*;
    use std::fs;

    /// Index a small JS repo to an on-disk db and return its path + the repo
    /// source dir (kept alive by the returned tempdir).
    fn index_on_disk() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            "function greet(n){return hello(n);}\nfunction hello(n){return n;}\n",
        )
        .unwrap();
        let db_path = dir.path().join("test.lbug");
        let repo_url = format!("file://{}", src.display());
        nestweaver_engine::index_directory(&src, &db_path, "test", &repo_url, "local").unwrap();
        // Compute + persist PageRank so hub_nodes has scores (mirrors the CLI).
        let store = GraphStore::open(&db_path).unwrap();
        store
            .compute_pagerank(0.85, 20, &nestweaver_store::GraphScope::code_only())
            .unwrap();
        store
            .save_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        (dir, db_path)
    }

    /// Reset the per-thread cache state that other tests in this thread may
    /// have touched (thread-locals persist across tests on the same thread).
    fn reset_session() {
        CACHE_HITS.with(|c| c.set(0));
        CACHE_MISSES.with(|c| c.set(0));
        RESPONSE_CACHE.with(|m| m.borrow_mut().clear());
        FLUSH_COUNTER.with(|c| c.set(0));
    }

    #[test]
    fn same_query_twice_is_a_cache_hit_byte_identical() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();

        let args = json!({ "limit": 5 });
        let first = dispatch(&store, None, "hub_nodes", args.clone(), None).unwrap();
        let second = dispatch(&store, None, "hub_nodes", args.clone(), None).unwrap();

        assert_eq!(
            first, second,
            "2nd call must return byte-identical response"
        );
        // Flush the in-process cache to disk so we can verify disk state.
        flush_response_cache();
        // The cache file exists and holds an entry.
        let cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
        );
        assert_eq!(cache.len(), 1);
        // Exactly one miss (1st) then one hit (2nd).
        assert_eq!(CACHE_MISSES.with(|c| c.get()), 1);
        assert_eq!(CACHE_HITS.with(|c| c.get()), 1);
    }

    #[test]
    fn reindex_bump_invalidates_cache() {
        reset_session();
        let (dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());

        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        let gen_before = store.graph_generation();
        let args = json!({ "limit": 5 });
        let _ = dispatch(&store, None, "hub_nodes", args.clone(), None).unwrap();
        drop(store);

        // Re-index with a new file → generation bumps + persists.
        let src = dir.path().join("repo");
        fs::write(src.join("extra.js"), "function added(){return 1;}\n").unwrap();
        let repo_url = format!("file://{}", src.display());
        nestweaver_engine::index_directory_with_options(
            &src, &db_path, "test", &repo_url, "local", true, None,
        )
        .unwrap();

        // Fresh process: reopen → generation loaded from sidecar is higher.
        let store2 = GraphStore::open(&db_path).unwrap();
        store2
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        assert!(
            store2.graph_generation() > gen_before,
            "reindex must bump the persisted generation"
        );

        reset_session();
        let _ = dispatch(&store2, None, "hub_nodes", args, None).unwrap();
        // The old entry's generation no longer matches → MISS (recomputed).
        assert_eq!(CACHE_MISSES.with(|c| c.get()), 1, "stale entry must miss");
        assert_eq!(CACHE_HITS.with(|c| c.get()), 0);
    }

    #[test]
    fn dirty_generation_cache_entry_misses_after_clean_publication() {
        reset_session();
        let (dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let marker_path = nestweaver_engine::sidecar_path(&db_path, ".index-dirty");
        fs::write(&marker_path, b"dirty").unwrap();

        let dirty_store = GraphStore::open(&db_path).unwrap();
        let dirty_generation = dirty_store.graph_generation();
        let args = json!({ "limit": 5 });
        let key = nestweaver_store::cache::ResponseCache::key("hub_nodes", &args);
        let scope_digest = whole_db_scope_digest(&db_path);
        let dirty_response = br#"{"dirty":true}"#;
        let mut cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
        );
        cache.insert(
            key,
            "hub_nodes",
            dirty_response,
            dirty_generation,
            scope_digest,
        );
        cache.save();
        drop(dirty_store);

        let src = dir.path().join("repo");
        let repo_url = format!("file://{}", src.display());
        nestweaver_engine::index_directory_with_options(
            &src, &db_path, "test", &repo_url, "local", true, None,
        )
        .unwrap();

        let clean_store = GraphStore::open(&db_path).unwrap();
        assert!(
            clean_store.graph_generation() > dirty_generation,
            "clean publication must advance beyond the dirty reservation"
        );
        reset_session();
        let result = dispatch(&clean_store, None, "hub_nodes", args, None).unwrap();
        assert_ne!(
            result,
            serde_json::from_slice::<Value>(dirty_response).unwrap()
        );
        assert_eq!(CACHE_MISSES.with(|c| c.get()), 1);
        assert_eq!(CACHE_HITS.with(|c| c.get()), 0);
    }

    #[test]
    fn dirty_publication_bypasses_response_cache() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        fs::write(
            nestweaver_engine::sidecar_path(&db_path, ".index-dirty"),
            b"dirty",
        )
        .unwrap();
        let store = GraphStore::open(&db_path).unwrap();
        let args = json!({ "limit": 5 });

        let _ = dispatch(&store, None, "hub_nodes", args.clone(), None).unwrap();
        let _ = dispatch(&store, None, "hub_nodes", args, None).unwrap();
        flush_response_cache();

        assert_eq!(CACHE_HITS.with(|c| c.get()), 0);
        assert_eq!(CACHE_MISSES.with(|c| c.get()), 0);
        let cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
        );
        assert!(cache.is_empty(), "dirty responses must not be retained");
    }

    #[test]
    fn bypass_always_misses() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();

        // Prime the cache.
        let _ = dispatch(&store, None, "hub_nodes", json!({ "limit": 5 }), None).unwrap();
        reset_session();
        // cache:"bypass" skips the cache entirely (no hit recorded).
        let _ = dispatch(
            &store,
            None,
            "hub_nodes",
            json!({ "limit": 5, "cache": "bypass" }),
            None,
        )
        .unwrap();
        // no_cache:true likewise.
        let _ = dispatch(
            &store,
            None,
            "hub_nodes",
            json!({ "limit": 5, "no_cache": true }),
            None,
        )
        .unwrap();
        assert_eq!(CACHE_HITS.with(|c| c.get()), 0, "bypass must never hit");
        assert_eq!(
            CACHE_MISSES.with(|c| c.get()),
            0,
            "bypass should not even consult the cache (no miss counted)"
        );
    }

    #[test]
    fn write_tools_are_never_cached() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        // Write/mutation and stateful tools are excluded from the cacheable set.
        assert!(!is_cacheable_tool("brain_add_source"));
        assert!(!is_cacheable_tool("set_extension"));
        assert!(!is_cacheable_tool("investigate"));
        assert!(!is_cacheable_tool("investigate_expand"));
        assert!(!is_cacheable_tool("investigate_hydrate"));
        assert!(!is_cacheable_tool("brain_status"));
        assert!(!is_cacheable_tool("stale_check"));
        // nw-077: read_symbols reads bodies from the filesystem (cwd/root-
        // dependent), so it must NOT be cached — a call from a wrong cwd would
        // otherwise poison the cache with an empty body served forever.
        assert!(!is_cacheable_tool("read_symbols"));
        // nw-089: query_extensions reads a sidecar that set_extension mutates
        // without bumping the generation, so a cached result would go stale.
        assert!(!is_cacheable_tool("query_extensions"));
        // And a representative read tool IS cacheable.
        assert!(is_cacheable_tool("hub_nodes"));

        let store = GraphStore::open(&db_path).unwrap();
        // set_extension is a write tool; dispatching it must not create a cache.
        let _ = dispatch(
            &store,
            None,
            "set_extension",
            json!({ "uid": "sym:x", "key": "k", "value": "v" }),
            None,
        );
        let cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
        );
        assert!(
            cache.is_empty(),
            "write tools must never populate the cache"
        );
    }

    /// A deterministic embed model so `brain_context` runs its semantic (vector)
    /// leg — the leg that observes the cancellation flag.
    struct FixedEmbed(Vec<f32>);
    impl EmbedQueryFn for FixedEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn cancelled_query_is_not_cached() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();

        let embed = FixedEmbed(vec![1.0, 0.0, 0.0]);
        let args = json!({ "seeds": ["greet"], "token_budget": 2000 });

        // Pre-cancelled: the vector leg trips the flag mid-flight, so the whole
        // query must error rather than return a truncated "complete" Ok.
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let cancelled = dispatch_cancellable(
            &store,
            None,
            "brain_context",
            args.clone(),
            Some(&embed),
            Some(&cancel),
            None,
        );
        assert!(
            cancelled.is_err(),
            "a cancelled brain_context must return Err, not a truncated Ok"
        );

        // …and it must NOT have populated the response cache.
        flush_response_cache();
        let cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
        );
        assert!(
            cache.is_empty(),
            "a cancelled query must never populate the response cache"
        );

        // A subsequent uncancelled call must RECOMPUTE (a MISS), never serve a
        // cached truncated/empty result.
        reset_session();
        let ok = dispatch_cancellable(
            &store,
            None,
            "brain_context",
            args,
            Some(&embed),
            None,
            None,
        );
        assert!(ok.is_ok(), "uncancelled recompute must succeed");
        assert_eq!(
            CACHE_MISSES.with(|c| c.get()),
            1,
            "recompute must be a MISS, not a cached-empty HIT"
        );
    }

    /// A pre-tripped cancel flag must make the flow_trace recursion return
    /// `Cancelled` at its first level — never a truncated Ok tree.
    #[test]
    fn flow_trace_cancellable_bails_when_flag_is_set() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let args = json!({ "symbol": "greet" });
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let cancelled = dispatch_cancellable(
            &store,
            None,
            "flow_trace",
            args.clone(),
            None,
            Some(&cancel),
            None,
        );
        let err =
            cancelled.expect_err("a cancelled flow_trace must return Err, not a truncated tree");
        assert!(
            err.downcast_ref::<nestweaver_store::StoreError>()
                .is_some_and(nestweaver_store::StoreError::is_cancelled),
            "the error must be StoreError::Cancelled, got: {err:#}"
        );

        // Untripped flag: the trace completes as before.
        let untripped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ok = dispatch_cancellable(
            &store,
            None,
            "flow_trace",
            args,
            None,
            Some(&untripped),
            None,
        );
        assert!(ok.is_ok(), "uncancelled flow_trace must succeed");
    }

    #[test]
    fn whole_db_scope_digest_covers_all_repos_and_distinguishes_same_rel_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.lbug");
        let path = nestweaver_engine::sidecar_path(&db, ".filemeta.json");

        let mut s = nestweaver_engine::FileMetaSidecar::default();
        s.repos.entry("repo:t:aaaa".into()).or_default().insert(
            "main.js".into(),
            nestweaver_engine::CachedFileMeta {
                mtime_secs: 1,
                size_bytes: 1,
                content_hash: "h".into(),
            },
        );
        nestweaver_engine::save_filemeta_sidecar(&s, &path).unwrap();
        let one_repo = whole_db_scope_digest(&db);

        // Same rel path + same hash in a SECOND repo must CHANGE the digest — if
        // pairs weren't repo-qualified, identical (path, hash) pairs would XOR-cancel.
        s.repos.entry("repo:t:bbbb".into()).or_default().insert(
            "main.js".into(),
            nestweaver_engine::CachedFileMeta {
                mtime_secs: 1,
                size_bytes: 1,
                content_hash: "h".into(),
            },
        );
        nestweaver_engine::save_filemeta_sidecar(&s, &path).unwrap();
        let two_repos = whole_db_scope_digest(&db);

        assert_ne!(
            one_repo, two_repos,
            "digest must be repo-qualified: identical rel paths across repos must not collapse"
        );
        assert_ne!(two_repos, 0);
    }

    /// Minimal `BrainNode` for budgeting tests: sets the semantic fields the
    /// renderers use and defaults the rest.
    fn test_brain_node(
        uid: &str,
        title: &str,
        kind: &str,
        location: &str,
    ) -> nestweaver_engine::BrainNode {
        nestweaver_engine::BrainNode {
            uid: uid.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            location: location.to_string(),
            relevance: 1.0,
            inline_body: None,
            body_complete: true,
        }
    }

    #[test]
    fn concise_budget_fits_more_nodes_than_detailed() {
        // nw-019 part 3: render_cost charged uid+relevance for nodes the concise
        // renderer never emits, so concise under-filled its budget.
        let nodes: Vec<nestweaver_engine::BrainNode> = (0..200)
            .map(|i| {
                test_brain_node(
                    &format!("sym:repo:c37ccf01:abcd1234:deadbeef{i:04}"), // realistic long uid
                    &format!("symbol_{i}"),
                    "Symbol/Function",
                    &format!("crates/foo/src/bar_{i}.rs:42"),
                )
            })
            .collect();
        let budget = 500usize;
        let (detailed, _) = budgeted_cut(&nodes, budget, false);
        let (concise, _) = budgeted_cut(&nodes, budget, true);
        assert!(
            concise > detailed,
            "concise must fit more nodes in the same budget: concise={concise} detailed={detailed}"
        );
    }
}

#[cfg(test)]
mod configured_limit_tests {
    use super::*;

    fn test_config(limit: usize) -> nestweaver_engine::InstanceConfig {
        serde_json::from_value(serde_json::json!({
            "instance_id": "test",
            "repos": [],
            "snapshot_storage": { "backend": "local", "path": "/tmp" },
            "workspace": { "backend": "local", "path": "/tmp" },
            "inference": { "endpoint": "", "embedding_model": "", "summary_model": "" },
            "git": { "credential_method": "ssh" },
            "limits": { "default_result_limit": limit }
        }))
        .expect("valid test config")
    }

    #[test]
    fn configured_result_limit_uses_default_without_config() {
        set_current_instance_config(None);
        assert_eq!(configured_result_limit(), DEFAULT_RESULT_LIMIT);
    }

    #[test]
    fn configured_result_limit_reads_from_instance_config() {
        let cfg = test_config(7);
        assert_eq!(cfg.limits.default_result_limit, 7);
        set_current_instance_config(Some(std::sync::Arc::new(cfg)));
        assert_eq!(configured_result_limit(), 7);
        set_current_instance_config(None);
    }

    #[test]
    fn configured_response_uses_default_without_config() {
        set_current_instance_config(None);
        let resp = configured_response();
        assert!((resp.inline_body_threshold - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn configured_response_reads_from_instance_config() {
        let mut cfg = test_config(50);
        cfg.response.inline_body_threshold = 0.5;
        cfg.response.inline_max_body_tokens = 200;
        set_current_instance_config(Some(std::sync::Arc::new(cfg)));
        let resp = configured_response();
        assert!((resp.inline_body_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(resp.inline_max_body_tokens, 200);
        set_current_instance_config(None);
    }
}

#[cfg(test)]
mod tool_doc_tests {
    use super::*;

    #[test]
    fn all_tools_have_doc_categories() {
        let entries = tool_doc_entries();
        let tool_count = tool_list(false)["tools"].as_array().unwrap().len();
        assert_eq!(
            entries.len(),
            tool_count,
            "doc entries must cover all tools"
        );
        for (name, cat, _, _) in &entries {
            assert_ne!(cat, "Other", "tool {name} is missing a category assignment");
        }
    }
}

// The v2.2.1 schema renames (top_n → limit, files → changed_files) keep the
// old names as runtime aliases. These tests pin both spellings on the
// store-direct handlers; the daemon dispatch mappings (dispatch_via_daemon,
// nestweaver-federation) forward 'limit' into the proto's top_n field.
#[cfg(test)]
mod arg_alias_tests {
    use super::*;

    #[test]
    fn hub_nodes_accepts_limit_and_top_n_alias() {
        let store = GraphStore::in_memory().unwrap();
        let via_limit = tool_hub_nodes(&store, json!({ "limit": 5 })).unwrap();
        assert_eq!(via_limit["top_n"], json!(5));
        let via_alias = tool_hub_nodes(&store, json!({ "top_n": 7 })).unwrap();
        assert_eq!(via_alias["top_n"], json!(7));
        let default = tool_hub_nodes(&store, json!({})).unwrap();
        assert_eq!(default["top_n"], json!(10));
    }

    #[test]
    fn bridge_nodes_accepts_limit_and_top_n_alias() {
        let store = GraphStore::in_memory().unwrap();
        let via_limit = tool_bridge_nodes(&store, json!({ "limit": 5 })).unwrap();
        assert_eq!(via_limit["top_n"], json!(5));
        let via_alias = tool_bridge_nodes(&store, json!({ "top_n": 7 })).unwrap();
        assert_eq!(via_alias["top_n"], json!(7));
    }

    #[test]
    fn detect_changes_accepts_changed_files_and_files_alias() {
        let store = GraphStore::in_memory().unwrap();
        let via_new =
            tool_detect_changes(&store, json!({ "changed_files": ["src/a.rs"] })).unwrap();
        assert_eq!(via_new["files"], json!(["src/a.rs"]));
        let via_alias = tool_detect_changes(&store, json!({ "files": ["src/b.rs"] })).unwrap();
        assert_eq!(via_alias["files"], json!(["src/b.rs"]));
        assert!(tool_detect_changes(&store, json!({})).is_err());
    }

    #[test]
    fn detect_changes_surfaces_unknown_source_trust_fields() {
        let store = GraphStore::in_memory().unwrap();
        let result = dispatch(
            &store,
            None,
            "detect_changes",
            json!({ "changed_files": ["src/new.rs"] }),
            None,
        )
        .unwrap();

        assert_eq!(result["status"], json!("partial"));
        assert_eq!(result["gate_state"], json!("degraded-unknown"));
        assert!(
            result["notifications"]
                .as_array()
                .expect("serialized notifications")
                .iter()
                .any(|n| n["descriptor"] == json!("changed-file-no-symbols"))
        );
    }

    #[test]
    fn get_summary_symbol_honors_name_alias_and_target() {
        // nw-079: `name` is accepted as an alias for `target`, and a targeted
        // symbol-level summary returns only the match (not a full-store scan).
        let store = GraphStore::in_memory().unwrap();
        for name in ["greet", "hello"] {
            let sym = nestweaver_schema::Symbol {
                uid: format!("sym:test:abc:{name}"),
                name: name.to_string(),
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid: "repo:test".to_string(),
                file_path: "src/main.js".to_string(),
                start_line: 1,
                end_line: 1,
                signature: format!("function {name}()"),
                summary: None,
                content_hash: name.to_string(),
                embedding: None,
                pagerank_score: Some(0.5),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: nestweaver_schema::Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            };
            store.insert_symbol(&sym).unwrap();
        }
        // `name` alias is honored (not silently dropped) and filters to one match.
        let via_name =
            tool_get_summary(&store, json!({ "level": "symbol", "name": "greet" })).unwrap();
        assert_eq!(via_name["count"], json!(1));
        assert_eq!(via_name["total_available"], json!(1));
        assert_eq!(via_name["partial"], json!(false));
        assert!(
            via_name["summaries"].as_str().unwrap().contains("greet"),
            "summary should describe the targeted symbol"
        );
        // `target` works identically.
        let via_target =
            tool_get_summary(&store, json!({ "level": "symbol", "target": "hello" })).unwrap();
        assert_eq!(via_target["count"], json!(1));
    }

    #[test]
    fn investigate_hydrate_rejects_targeting_keys() {
        // nw-084: hydrate is bulk (no per-entry selector). Passing targets/uid/
        // uids was silently ignored (looked like "nothing hydrated"); now it's a
        // clear error pointing to investigate_expand, matching expand's strictness.
        let store = GraphStore::in_memory().unwrap();
        for key in ["targets", "uid", "uids"] {
            let err = tool_investigate_hydrate(
                &store,
                json!({ "bundle_id": "bndl_x", key: ["sym:whatever"] }),
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("investigate_hydrate takes no") && err.contains("investigate_expand"),
                "expected a pointer to investigate_expand for key {key}, got: {err}"
            );
        }
        // A well-formed call (no targeting keys) gets PAST the new validation —
        // it fails later on db-path/bundle resolution, not on a targeting-key
        // rejection.
        let err = tool_investigate_hydrate(&store, json!({ "bundle_id": "bndl_missing" }))
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("investigate_hydrate takes no"),
            "a well-formed call must pass targeting-key validation, got: {err}"
        );
    }
}

#[cfg(test)]
mod blast_radius_visibility_tests {
    use super::*;
    use nestweaver_engine::authz::VisibleRepos;
    use nestweaver_schema::{
        CrossRepoLinkType, EdgeType, Repo, ResolvedEdge, Symbol, SymbolKind, Visibility,
    };

    /// Build a two-repo (repo:api → repo:client) cross-repo store, mirroring the
    /// engine's `org_wide_populated_for_cross_repo_impact` fixture. Changing the
    /// api symbol surfaces repo:client as an org-wide impact. Repo records use
    /// `url == uid` only to keep the expected presentation labels concise;
    /// authorization uses the org item's stable source and destination UIDs.
    fn cross_repo_store() -> GraphStore {
        let store = GraphStore::in_memory().expect("in_memory store");

        let mk = |uid: &str, name: &str, repo: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo.to_string(),
            file_path: file.to_string(),
            start_line: 3,
            end_line: 9,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: Some(0.2),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        let mk_repo = |uid: &str| Repo {
            uid: uid.to_string(),
            url: uid.to_string(),
            indexed_sha: String::new(),
            staleness_commits_behind: 0,
            instance_id: "inst".to_string(),
            name: None,
            root_path: None,
        };

        store.insert_repo(&mk_repo("repo:api")).unwrap();
        store.insert_repo(&mk_repo("repo:client")).unwrap();
        store
            .insert_symbol(&mk("api", "Handler", "repo:api", "src/api.rs"))
            .unwrap();
        store
            .insert_symbol(&mk("client", "Caller", "repo:client", "src/client.rs"))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "client".to_string(),
                target_uid: "api".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: vec![],
            })
            .unwrap();
        store
    }

    fn mixed_visibility_store() -> GraphStore {
        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, repo: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo.to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };
        let mk_repo = |uid: &str| Repo {
            uid: uid.to_string(),
            url: uid.to_string(),
            indexed_sha: String::new(),
            staleness_commits_behind: 0,
            instance_id: "inst".to_string(),
            name: None,
            root_path: None,
        };

        for repo in ["repo:api", "repo:a", "repo:b"] {
            store.insert_repo(&mk_repo(repo)).unwrap();
        }
        store
            .insert_symbol(&mk("target", "Target", "repo:api", "src/target.rs"))
            .unwrap();
        for (uid, name, repo, confidence) in [
            ("hidden", "HiddenCaller", "repo:b", 0.95_f32),
            ("visible", "VisibleCaller", "repo:a", 0.9_f32),
            ("local", "LocalCaller", "", 0.8_f32),
        ] {
            store
                .insert_symbol(&mk(uid, name, repo, &format!("src/{uid}.rs")))
                .unwrap();
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: uid.to_string(),
                    target_uid: "target".to_string(),
                    edge_type: EdgeType::Calls,
                    confidence,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }
        store
    }

    fn duplicate_url_store() -> GraphStore {
        let store = GraphStore::in_memory().expect("in_memory store");
        let repo = |uid: &str, url: &str| Repo {
            uid: uid.to_string(),
            url: url.to_string(),
            indexed_sha: String::new(),
            staleness_commits_behind: 0,
            instance_id: "inst".to_string(),
            name: None,
            root_path: None,
        };
        let symbol = |uid: &str, name: &str, repo_uid: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        store
            .insert_repo(&repo("repo:hidden", "https://example.test/shared"))
            .unwrap();
        store
            .insert_repo(&repo("repo:alias", "https://example.test/shared"))
            .unwrap();
        store
            .insert_repo(&repo("repo:source", "https://example.test/source"))
            .unwrap();
        store
            .insert_symbol(&symbol(
                "source",
                "VisibleSource",
                "repo:source",
                "src/source.rs",
            ))
            .unwrap();
        store
            .insert_symbol(&symbol(
                "hidden-duplicate",
                "HiddenDuplicateCaller",
                "repo:hidden",
                "hidden/duplicate.rs",
            ))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "hidden-duplicate".to_string(),
                target_uid: "source".to_string(),
                edge_type: EdgeType::CrossRepoLink,
                confidence: 0.9,
                link_type: Some(CrossRepoLinkType::SharedImport),
                evidence: vec![],
            })
            .unwrap();
        store
    }

    fn unresolved_affected_owner_store() -> GraphStore {
        let store = GraphStore::in_memory().expect("in_memory store");
        let repo = |uid: &str| Repo {
            uid: uid.to_string(),
            url: format!("https://example.test/{uid}"),
            indexed_sha: String::new(),
            staleness_commits_behind: 0,
            instance_id: "inst".to_string(),
            name: None,
            root_path: None,
        };
        let symbol = |uid: &str, name: &str, repo_uid: &str, file: &str, signature: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: repo_uid.to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 2,
            signature: signature.to_string(),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        store.insert_repo(&repo("repo:visible")).unwrap();
        store.insert_repo(&repo("repo:hidden")).unwrap();
        store
            .insert_symbol(&symbol(
                "visible-target",
                "VisibleTarget",
                "repo:visible",
                "src/target.rs",
                "fn VisibleTarget()",
            ))
            .unwrap();
        // The edge scan reads uid/name/path, but the authoritative symbol-row
        // lookup rejects this corruption canary while decoding `signature`.
        store
            .insert_symbol(&symbol(
                "hidden-lookup-miss",
                "HiddenLookupMiss",
                "repo:hidden",
                "hidden/lookup-miss.rs",
                "fn HiddenLookupMiss()\0",
            ))
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "hidden-lookup-miss".to_string(),
                target_uid: "visible-target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .unwrap();
        store
    }

    /// Redaction must strip the cross-repo (repo:client) org item and affected
    /// symbol when the caller may only see repo:api, while `None` (the
    /// backward-compatible default) leaves the full result intact.
    #[test]
    fn tool_blast_radius_redacts_to_visible_repos() {
        let store = cross_repo_store();
        let args = json!({ "changed_files": ["src/api.rs"] });

        // None ⇒ no scoping ⇒ the cross-repo consumer is present.
        let full = tool_blast_radius(&store, args.clone(), None, None).unwrap();
        let full_str = serde_json::to_string(&full).unwrap();
        assert!(
            full_str.contains("repo:client"),
            "unredacted result must name the downstream repo; got: {full_str}"
        );
        assert!(
            full_str.contains("Caller"),
            "unredacted result must include the cross-repo affected symbol"
        );
        assert!(
            !full["org_wide"].is_null(),
            "unredacted result must carry org_wide impact"
        );

        // Only(repo:api) ⇒ everything naming repo:client is redacted out.
        let visible = VisibleRepos::Only(["repo:api".to_string()].into_iter().collect());
        let scoped = tool_blast_radius(&store, args, None, Some(&visible)).unwrap();
        let scoped_str = serde_json::to_string(&scoped).unwrap();
        assert!(
            !scoped_str.contains("repo:client"),
            "redacted result must not leak the hidden repo; got: {scoped_str}"
        );
        assert!(
            !scoped_str.contains("Caller"),
            "redacted result must drop the hidden repo's affected symbol"
        );
        assert!(
            scoped["org_wide"].is_null(),
            "org_wide collapses to null once its only item (repo:client) is hidden"
        );
    }

    #[test]
    fn two_tier_local_tools_redact_hidden_repo_symbols_and_counts() {
        let store = mixed_visibility_store();
        let visible = VisibleRepos::Only(
            ["repo:api".to_string(), "repo:a".to_string()]
                .into_iter()
                .collect(),
        );
        let mut hidden_target = store
            .lookup_symbols_by_name("HiddenCaller")
            .expect("hidden template")
            .remove(0);
        hidden_target.uid = "hidden-target".to_string();
        hidden_target.name = "Target".to_string();
        hidden_target.file_path = "hidden/target.rs".to_string();
        store
            .insert_symbol(&hidden_target)
            .expect("hidden same-name target");
        let mut visible_via_hidden = hidden_target.clone();
        visible_via_hidden.uid = "visible-via-hidden".to_string();
        visible_via_hidden.name = "VisibleViaHidden".to_string();
        visible_via_hidden.repo_uid = "repo:a".to_string();
        visible_via_hidden.file_path = "src/visible_via_hidden.rs".to_string();
        store
            .insert_symbol(&visible_via_hidden)
            .expect("visible transitive caller");
        store
            .insert_edge(&ResolvedEdge {
                source_uid: visible_via_hidden.uid.clone(),
                target_uid: "hidden".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
                evidence: vec![],
            })
            .expect("hidden-intermediate edge");

        let impact = tool_brain_impact(&store, json!({ "symbol": "Target" }), None, Some(&visible))
            .expect("scoped impact");
        let impact_text = impact.to_string();
        assert_eq!(
            impact["status"], "ok",
            "a hidden same-name symbol must not make the visible target ambiguous"
        );
        assert!(impact_text.contains("VisibleCaller"), "{impact}");
        assert!(!impact_text.contains("HiddenCaller"), "{impact}");
        assert!(!impact_text.contains("LocalCaller"), "{impact}");
        assert!(!impact_text.contains("VisibleViaHidden"), "{impact}");
        assert!(!impact_text.contains("hidden/target.rs"), "{impact}");
        assert_eq!(impact["total"], 1);
        assert_eq!(impact["returned"], 1);

        let mut visible_test = store
            .lookup_symbols_by_name("VisibleCaller")
            .expect("visible template")
            .remove(0);
        visible_test.uid = "visible-test".to_string();
        visible_test.name = "visible_target_test".to_string();
        visible_test.file_path = "tests/visible_target_test.rs".to_string();
        store.insert_symbol(&visible_test).expect("visible test");
        let mut hidden_test = visible_test.clone();
        hidden_test.uid = "hidden-test".to_string();
        hidden_test.name = "hidden_target_test".to_string();
        hidden_test.repo_uid = "repo:b".to_string();
        hidden_test.file_path = "hidden/tests/hidden_target_test.rs".to_string();
        store.insert_symbol(&hidden_test).expect("hidden test");
        let mut hidden_same_path_test = hidden_test.clone();
        hidden_same_path_test.uid = "hidden-same-path-test".to_string();
        hidden_same_path_test.name = "hidden_same_path_test".to_string();
        hidden_same_path_test.file_path = visible_test.file_path.clone();
        store
            .insert_symbol(&hidden_same_path_test)
            .expect("hidden same-path test");
        for source_uid in ["visible-test", "hidden-test"] {
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: source_uid.to_string(),
                    target_uid: "target".to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .expect("test edge");
        }
        store
            .insert_edge(&ResolvedEdge {
                source_uid: hidden_same_path_test.uid,
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.7,
                link_type: None,
                evidence: vec![],
            })
            .expect("hidden same-path test edge");

        let tests = tool_affected_tests(
            &store,
            json!({ "changed_files": ["src/target.rs"] }),
            Some(&visible),
        )
        .expect("scoped affected tests");
        let tests_text = tests.to_string();
        assert!(tests_text.contains("visible_target_test"), "{tests}");
        assert!(!tests_text.contains("hidden_target_test"), "{tests}");
        assert!(!tests_text.contains("hidden_same_path_test"), "{tests}");
        assert!(!tests_text.contains("hidden/tests"), "{tests}");
        assert_eq!(
            tests["summary"],
            "1 tier-1, 0 tier-2, 0 tier-3 tests affected"
        );
    }

    #[test]
    fn blast_radius_count_is_independent_of_limit() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let mk = |uid: &str, name: &str, file: &str| Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:api".to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("h_{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        };

        store
            .insert_symbol(&mk("target", "fn_target", "src/target.rs"))
            .unwrap();
        for i in 0..60 {
            let uid = format!("caller:{i}");
            store
                .insert_symbol(&mk(
                    &uid,
                    &format!("fn_caller_{i}"),
                    &format!("src/caller_{i}.rs"),
                ))
                .unwrap();
            store
                .insert_edge(&ResolvedEdge {
                    source_uid: uid,
                    target_uid: "target".to_string(),
                    edge_type: EdgeType::Calls,
                    confidence: 0.9,
                    link_type: None,
                    evidence: vec![],
                })
                .unwrap();
        }

        let result = tool_blast_radius(
            &store,
            json!({ "changed_files": ["src/target.rs"], "limit": 5 }),
            None,
            None,
        )
        .unwrap();

        assert_eq!(result["affected_symbol_count"], json!(60));
        assert_eq!(result["returned_affected_symbol_count"], json!(5));
        assert_eq!(result["affected_symbols_truncated"], json!(true));
        assert_eq!(result["affected_symbols"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn blast_radius_authz_precedes_limit_and_sarif_count_exposure() {
        let store = mixed_visibility_store();
        let visible = VisibleRepos::Only(["repo:a".to_string()].into_iter().collect());
        let args = json!({ "changed_files": ["src/target.rs"], "limit": 1 });

        let result = tool_blast_radius(&store, args.clone(), None, Some(&visible)).unwrap();
        assert_eq!(result["affected_symbol_count"], json!(2));
        assert_eq!(result["returned_affected_symbol_count"], json!(1));
        assert_eq!(result["affected_symbols_truncated"], json!(true));
        assert_eq!(result["affected_symbols"].as_array().unwrap().len(), 1);
        assert!(
            result["summary"]
                .as_str()
                .unwrap()
                .contains("2 transitively affected")
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("repo:b"));
        assert!(!serialized.contains("HiddenCaller"));
        assert!(!serialized.contains("src/hidden.rs"));
        assert!(!serialized.contains("repo:api"));
        assert!(!serialized.contains("Target"));
        assert!(result["org_wide"].is_null());

        let sarif = tool_blast_radius(
            &store,
            json!({
                "changed_files": ["src/target.rs"],
                "limit": 1,
                "format": "sarif"
            }),
            None,
            Some(&visible),
        )
        .unwrap();
        let props = &sarif["runs"][0]["properties"];
        assert_eq!(props["nestweaver/affectedSymbolCount"], json!(2));
        assert_eq!(props["nestweaver/returnedAffectedSymbolCount"], json!(1));
        assert_eq!(props["nestweaver/affectedSymbolsTruncated"], json!(true));
        let serialized = serde_json::to_string(&sarif).unwrap();
        assert!(!serialized.contains("repo:b"));
        assert!(!serialized.contains("HiddenCaller"));
        assert!(!serialized.contains("src/hidden.rs"));
        assert!(!serialized.contains("repo:api"));
        assert!(!serialized.contains("Target"));
        assert!(
            sarif["runs"][0]["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|result| result["ruleId"] != "nw/org-impact")
        );
    }

    #[test]
    fn blast_radius_authz_ignores_duplicate_repo_display_urls() {
        let store = duplicate_url_store();
        let visible = VisibleRepos::Only(
            ["repo:source".to_string(), "repo:alias".to_string()]
                .into_iter()
                .collect(),
        );
        let args = json!({ "changed_files": ["src/source.rs"] });

        let result = tool_blast_radius(&store, args.clone(), None, Some(&visible)).unwrap();
        assert!(result["org_wide"].is_null());
        assert_eq!(result["affected_symbol_count"], 0);
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("repo:hidden"));
        assert!(!serialized.contains("HiddenDuplicateCaller"));
        assert!(!serialized.contains("hidden/duplicate.rs"));

        let sarif = tool_blast_radius(
            &store,
            json!({ "changed_files": ["src/source.rs"], "format": "sarif" }),
            None,
            Some(&visible),
        )
        .unwrap();
        assert!(sarif["runs"][0]["results"].as_array().unwrap().is_empty());
        let serialized = serde_json::to_string(&sarif).unwrap();
        assert!(!serialized.contains("repo:hidden"));
        assert!(!serialized.contains("HiddenDuplicateCaller"));
        assert!(!serialized.contains("hidden/duplicate.rs"));
    }

    #[test]
    fn blast_radius_authz_hides_unqualified_cochange_data_from_json_and_sarif() {
        use nestweaver_engine::cochange::{CoChangeEdge, save_cochange_sidecar};

        let store = cross_repo_store();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cochange-authz.lbug");
        std::fs::write(&db_path, b"").unwrap();
        save_cochange_sidecar(
            &[CoChangeEdge {
                file_a: "src/api.rs".to_string(),
                file_b: "hidden/private.sql".to_string(),
                cochange_count: 8675309,
                total_commits_a: 8675310,
                total_commits_b: 8675311,
                confidence: 0.99,
            }],
            &nestweaver_engine::sidecar_path(&db_path, ".cochange.json"),
        )
        .unwrap();
        set_current_db_path(db_path);
        let visible = VisibleRepos::Only(["repo:api".to_string()].into_iter().collect());
        let args = json!({ "changed_files": ["src/api.rs"] });

        let result = tool_blast_radius(&store, args.clone(), None, Some(&visible)).unwrap();
        assert!(result["cochanged_files"].as_array().unwrap().is_empty());
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("hidden/private.sql"));
        assert!(!serialized.contains("8675309"));

        let sarif = tool_blast_radius(
            &store,
            json!({ "changed_files": ["src/api.rs"], "format": "sarif" }),
            None,
            Some(&visible),
        )
        .unwrap();
        let serialized = serde_json::to_string(&sarif).unwrap();
        assert!(!serialized.contains("hidden/private.sql"));
        assert!(!serialized.contains("8675309"));
    }

    #[test]
    fn blast_radius_authz_sanitizes_degraded_notifications_in_json_and_sarif() {
        let store = GraphStore::in_memory().unwrap();
        let visible = VisibleRepos::Only(HashSet::new());
        let args = json!({ "changed_files": ["hidden/private.rs"] });

        let result = tool_blast_radius(&store, args.clone(), None, Some(&visible)).unwrap();
        assert_eq!(result["status"], "degraded");
        assert_eq!(result["gate_state"], "degraded-unknown");
        let notifications = result["notifications"].as_array().unwrap();
        let changed_file_note = notifications
            .iter()
            .find(|note| note["descriptor"] == "changed-file-no-symbols")
            .expect("changed-file-no-symbols notification");
        assert_eq!(changed_file_note["level"], "warning");
        assert_eq!(
            changed_file_note["message"],
            "blast-radius analysis details withheld by repository visibility policy"
        );
        let notifications_json = serde_json::to_string(notifications).unwrap();
        assert!(!notifications_json.contains("hidden/private.rs"));

        let sarif = tool_blast_radius(
            &store,
            json!({ "changed_files": ["hidden/private.rs"], "format": "sarif" }),
            None,
            Some(&visible),
        )
        .unwrap();
        assert_eq!(
            sarif["runs"][0]["properties"]["nestweaver/status"],
            "degraded"
        );
        assert_eq!(
            sarif["runs"][0]["properties"]["nestweaver/gateState"],
            "degraded-unknown"
        );
        let serialized = serde_json::to_string(&sarif).unwrap();
        assert!(!serialized.contains("hidden/private.rs"));
    }

    #[test]
    fn blast_radius_authz_drops_unknown_affected_ownership_from_json_and_sarif() {
        let store = unresolved_affected_owner_store();
        let visible = VisibleRepos::Only(["repo:visible".to_string()].into_iter().collect());
        let args = json!({ "changed_files": ["src/target.rs"] });

        let result = tool_blast_radius(&store, args.clone(), None, Some(&visible)).unwrap();
        assert_eq!(result["affected_symbol_count"], 0);
        assert_eq!(result["returned_affected_symbol_count"], 0);
        assert!(result["affected_symbols"].as_array().unwrap().is_empty());
        assert!(
            result["summary"]
                .as_str()
                .unwrap()
                .contains("0 transitively affected")
        );
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("hidden-lookup-miss"));
        assert!(!serialized.contains("HiddenLookupMiss"));
        assert!(!serialized.contains("hidden/lookup-miss.rs"));

        let sarif = tool_blast_radius(
            &store,
            json!({ "changed_files": ["src/target.rs"], "format": "sarif" }),
            None,
            Some(&visible),
        )
        .unwrap();
        let props = &sarif["runs"][0]["properties"];
        assert_eq!(props["nestweaver/affectedSymbolCount"], 0);
        assert_eq!(props["nestweaver/returnedAffectedSymbolCount"], 0);
        assert!(sarif["runs"][0]["results"].as_array().unwrap().is_empty());
        let serialized = serde_json::to_string(&sarif).unwrap();
        assert!(!serialized.contains("hidden-lookup-miss"));
        assert!(!serialized.contains("HiddenLookupMiss"));
        assert!(!serialized.contains("hidden/lookup-miss.rs"));
    }

    #[test]
    fn blast_radius_authz_keeps_resolved_local_affected_symbol_in_json_and_sarif() {
        let store = mixed_visibility_store();
        let visible = VisibleRepos::Only(["repo:a".to_string()].into_iter().collect());

        let result = tool_blast_radius(
            &store,
            json!({ "changed_files": ["src/target.rs"] }),
            None,
            Some(&visible),
        )
        .unwrap();
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(serialized.contains("LocalCaller"));
        assert!(serialized.contains("src/local.rs"));

        let sarif = tool_blast_radius(
            &store,
            json!({ "changed_files": ["src/target.rs"], "format": "sarif" }),
            None,
            Some(&visible),
        )
        .unwrap();
        let serialized = serde_json::to_string(&sarif).unwrap();
        assert!(serialized.contains("LocalCaller"));
        assert!(serialized.contains("src/local.rs"));
    }
}

#[cfg(test)]
mod source_management_tests {
    use super::*;

    fn repo(
        uid: &str,
        url: &str,
        name: Option<&str>,
        root: Option<&str>,
    ) -> nestweaver_schema::Repo {
        nestweaver_schema::Repo {
            uid: uid.to_string(),
            url: url.to_string(),
            indexed_sha: "sha".to_string(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: name.map(String::from),
            root_path: root.map(String::from),
        }
    }

    #[test]
    fn match_repo_target_resolves_by_uid_name_and_path() {
        // nw-089: brain_remove_source must resolve a path/name/url/uid to the
        // repo, not send the raw string as a uid (which matched nothing).
        let repos = vec![
            repo(
                "repo:aa:bb",
                "file:///tmp/nw_match_test_xyz",
                Some("my-repo"),
                Some("/tmp/nw_match_test_xyz"),
            ),
            repo("repo:cc:dd", "file:///other/place", None, None),
        ];
        // by uid
        assert_eq!(match_repo_target(&repos, "repo:aa:bb").len(), 1);
        // by name
        assert_eq!(match_repo_target(&repos, "my-repo").len(), 1);
        // by file:// URL
        assert_eq!(
            match_repo_target(&repos, "file:///tmp/nw_match_test_xyz").len(),
            1
        );
        // by absolute path (canonicalize fails on a nonexistent path → falls back
        // to file://<path>, which matches the repo url)
        let m = match_repo_target(&repos, "/tmp/nw_match_test_xyz");
        assert_eq!(m.len(), 1, "a path target must resolve to its repo");
        assert_eq!(m[0].uid, "repo:aa:bb");
        // trailing slash tolerated
        assert_eq!(
            match_repo_target(&repos, "/tmp/nw_match_test_xyz/").len(),
            1
        );
        // no match
        assert!(match_repo_target(&repos, "/nope/nope").is_empty());
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn dir_is_markdown_dominant_classifies_code_vs_notes() {
        use std::fs;
        // nw-089: a code dir (no .git, no markdown) must NOT be seen as a vault.
        let code = tempfile::tempdir().unwrap();
        fs::write(code.path().join("lib.rs"), "pub fn a() {}").unwrap();
        fs::write(code.path().join("app.ts"), "export const x = 1;").unwrap();
        assert!(
            !dir_is_markdown_dominant(code.path()),
            "a code directory is not markdown-dominant"
        );

        // A notes dir (all markdown) IS a vault.
        let notes = tempfile::tempdir().unwrap();
        fs::write(notes.path().join("a.md"), "# a").unwrap();
        fs::write(notes.path().join("b.md"), "# b").unwrap();
        assert!(dir_is_markdown_dominant(notes.path()));

        // A code dir with a single README.md is still code (md not the majority).
        let mixed = tempfile::tempdir().unwrap();
        fs::write(mixed.path().join("README.md"), "# readme").unwrap();
        fs::write(mixed.path().join("a.rs"), "fn a() {}").unwrap();
        fs::write(mixed.path().join("b.rs"), "fn b() {}").unwrap();
        assert!(
            !dir_is_markdown_dominant(mixed.path()),
            "a code dir with a README is not a vault"
        );
    }
}

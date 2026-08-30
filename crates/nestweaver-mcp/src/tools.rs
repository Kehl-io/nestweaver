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
use nestweaver_engine::{index_markdown_directory, save_extensions, set_property};

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
    let mut schemas = all_tool_schemas_undecorated();
    // Every cacheable tool honours `cache: "bypass"` / `no_cache: true` at
    // dispatch (`cache_bypassed`), so every cacheable tool must DECLARE them.
    // Derived from `CACHEABLE_TOOLS` rather than written into each schema by
    // hand: with `additionalProperties: false` an undeclared pair is a hard
    // rejection, and a hand-maintained list drifts the moment a tool joins
    // `CACHEABLE_TOOLS`. That drift already happened — 18 of 25 cacheable
    // tools did not declare them, which made `no_cache` a validation error
    // and left `get_summary`'s own `cache_bypassed` branch unreachable.
    for tool in &mut schemas {
        let Some(name) = tool["name"].as_str() else {
            continue;
        };
        if !CACHEABLE_TOOLS.contains(&name) {
            continue;
        }
        let Some(properties) = tool
            .get_mut("inputSchema")
            .and_then(|schema| schema.get_mut("properties"))
            .and_then(|properties| properties.as_object_mut())
        else {
            continue;
        };
        properties.entry("cache").or_insert_with(|| {
            serde_json::json!({
                "type": "string",
                "description": "Set to \"bypass\" to skip the response cache for this call."
            })
        });
        properties.entry("no_cache").or_insert_with(|| {
            serde_json::json!({
                "type": "boolean",
                "description": "When true, skip the response cache for this call."
            })
        });
    }
    // nw-293. Every tool must declare MCP `annotations`, DERIVED from
    // `MUTATING_TOOLS` for exactly the reason the cache decoration above is
    // derived from `CACHEABLE_TOOLS`: the classification already exists and is
    // already authoritative (it is the gate both the HTTP surface and the
    // daemon's gRPC surface enforce), so hand-annotating 42 schemas would
    // create a second list that drifts the moment a seventh mutator lands.
    // Zero of the 42 declared any annotation, which made `prune_stale`
    // indistinguishable from `brain_status` on the wire.
    //
    // `annotations` is a SIBLING of `inputSchema` on the MCP `Tool` object, so
    // it is inert for `tool_validators()` (which builds from
    // `tool["inputSchema"]` only) and cannot affect `additionalProperties`.
    for tool in &mut schemas {
        let Some(name) = tool["name"].as_str() else {
            continue;
        };
        // Not in the canonical mutating list => read-only, and a read-only
        // tool is trivially non-destructive and idempotent.
        let (read_only, destructive, idempotent) = match crate::http::mutating_tool_hints(name) {
            Some((destructive, idempotent)) => (false, destructive, idempotent),
            None => (true, false, true),
        };
        let Some(object) = tool.as_object_mut() else {
            continue;
        };
        object.insert(
            "annotations".to_string(),
            serde_json::json!({
                "readOnlyHint": read_only,
                "destructiveHint": destructive,
                "idempotentHint": idempotent,
                // Every tool here reads the LOCAL graph and local filesystem.
                // `brain_add_source` is the only candidate for an open world
                // and its own description rules it out: "Cannot index remote
                // URLs directly — only local filesystem paths".
                "openWorldHint": false,
            }),
        );
    }
    schemas
}

fn all_tool_schemas_undecorated() -> Vec<Value> {
    vec![
        tool_schema_brain_context(),
        tool_schema_code_context(),
        tool_schema_brain_search(),
        tool_schema_note_get(),
        tool_schema_backlinks(),
        tool_schema_brain_status(),
        tool_schema_brain_add_source(),
        tool_schema_brain_remove_source(),
        tool_schema_prune_stale(),
        tool_schema_compact_embeddings(),
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

/// Reject alias pairs that are BOTH present — `regex_search` called with both
/// `pattern` and `query` is ambiguous about which one drives the search and
/// previously picked one silently.
fn conflicting_alias_error(name: &str, args: &Value) -> Option<&'static str> {
    if !args.is_object() {
        return None;
    }
    match name {
        "regex_search" if args.get("pattern").is_some() && args.get("query").is_some() => {
            Some("conflicting arguments: pass only one of 'pattern' or 'query'")
        }
        _ => None,
    }
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
    } else if let Some(message) = conflicting_alias_error(name, args) {
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
    if is_direct_read_only() {
        tools.retain(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| !crate::http::MUTATING_TOOLS.contains(&name))
        });
    }
    json!({ "tools": tools })
}

/// Validate an explicit CLI tool selection against the selected transport.
/// This runs before the MCP loop starts so a typo or unavailable direct-mode
/// mutator cannot silently produce a zero-tool server.
pub fn validate_tool_selection(
    names: Option<&[String]>,
    lite: bool,
    direct_read_only: bool,
) -> Result<(), anyhow::Error> {
    let registered = all_tool_schemas()
        .into_iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>();
    let Some(names) = names else {
        return Ok(());
    };
    if names.is_empty() {
        anyhow::bail!("--tools must name at least one MCP tool");
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        if name.trim().is_empty() {
            anyhow::bail!("--tools contains an empty tool name");
        }
        if !seen.insert(name.as_str()) {
            anyhow::bail!("--tools contains duplicate tool name '{name}'");
        }
        if !registered.contains(name) {
            anyhow::bail!(
                "unknown MCP tool '{name}'; use `nestweaver mcp --help` and `tools/list` for registered names"
            );
        }
        if direct_read_only && crate::http::MUTATING_TOOLS.contains(&name.as_str()) {
            anyhow::bail!(
                "MCP tool '{name}' is unavailable in direct read-only mode; remove --no-daemon to route mutations through the daemon"
            );
        }
    }
    if lite && !names.iter().any(|name| LITE_TOOLS.contains(&name.as_str())) {
        anyhow::bail!(
            "--lite and --tools have no tools in common; lite tools: {}",
            LITE_TOOLS.join(", ")
        );
    }
    Ok(())
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
    fn explicit_tool_selection_is_transport_aware_and_never_silently_empty() {
        validate_tool_selection(
            Some(&["brain_context".to_string(), "brain_search".to_string()]),
            false,
            true,
        )
        .expect("read tools are available directly");

        for (names, lite, direct, needle) in [
            (
                vec!["context".to_string()],
                false,
                false,
                "unknown MCP tool",
            ),
            (vec!["".to_string()], false, false, "empty tool name"),
            (
                vec!["brain_search".to_string(), "brain_search".to_string()],
                false,
                false,
                "duplicate",
            ),
            (
                vec!["brain_add_source".to_string()],
                false,
                true,
                "unavailable in direct read-only mode",
            ),
            (
                vec!["read_symbols".to_string()],
                true,
                false,
                "no tools in common",
            ),
        ] {
            let error = validate_tool_selection(Some(&names), lite, direct)
                .expect_err("invalid selection must fail before startup")
                .to_string();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn readme_tool_allowlist_examples_only_use_registered_names() {
        let readme = include_str!("../../../README.md");
        let registered = all_tool_schemas()
            .into_iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect::<std::collections::BTreeSet<_>>();
        let mut examples = 0;
        for line in readme.lines() {
            let Some(rest) = line.split("nestweaver mcp --tools ").nth(1) else {
                continue;
            };
            let names = rest.split_whitespace().next().unwrap_or_default();
            for name in names.split(',') {
                assert!(
                    registered.contains(name),
                    "README --tools example names unregistered tool '{name}'"
                );
            }
            examples += 1;
        }
        assert!(examples > 0, "README must contain a tested --tools example");
    }

    #[test]
    fn registry_contains_exactly_the_42_advertised_unique_names() {
        let expected: BTreeSet<&str> = [
            "affected_tests",
            "backlinks",
            "blast_radius",
            "brain_add_source",
            "brain_broken_links",
            "brain_context",
            "code_context",
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
            "compact_embeddings",
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
            42,
            "registry must contain exactly 42 schemas"
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
    fn numeric_ranges_are_enforced_not_silently_coerced() {
        // Negatives/floats/over-cap values used to vanish through
        // `as_u64()` coercion and fall back to defaults. The compiled schemas
        // now reject them on every transport.
        let invalid = [
            ("regex_search", json!({ "pattern": "x", "limit": 0 })),
            ("regex_search", json!({ "pattern": "x", "limit": 10_001 })),
            ("regex_search", json!({ "pattern": "x", "limit": -1 })),
            ("regex_search", json!({ "pattern": "x", "limit": 2.5 })),
            ("regex_search", json!({ "pattern": "x", "max_millis": 0 })),
            (
                "regex_search",
                json!({ "pattern": "x", "max_millis": 600_001 }),
            ),
            (
                "project_context",
                json!({ "project": "p", "token_budget": 0 }),
            ),
            (
                "project_context",
                json!({ "project": "p", "token_budget": 16_001 }),
            ),
            ("investigate", json!({ "query": "q", "token_budget": -3 })),
            (
                "investigate",
                json!({ "query": "q", "token_budget": 16_001 }),
            ),
            (
                "investigate_hydrate",
                json!({ "bundle_id": "b", "token_budget": 0 }),
            ),
            ("brain_impact", json!({ "symbol": "s", "depth": 0 })),
            ("brain_impact", json!({ "symbol": "s", "depth": 16 })),
            ("brain_impact", json!({ "symbol": "s", "limit": -2 })),
            // flow_trace declared NO bounds while the CLI enforced 1..=15, so
            // the MCP accepted depth 0 and depth 1000.
            ("flow_trace", json!({ "symbol": "s", "max_depth": 0 })),
            ("flow_trace", json!({ "symbol": "s", "max_depth": 16 })),
            (
                "blast_radius",
                json!({ "changed_files": ["a.rs"], "max_depth": 0 }),
            ),
            (
                "blast_radius",
                json!({ "changed_files": ["a.rs"], "max_depth": 16 }),
            ),
            ("hub_nodes", json!({ "limit": 0 })),
            ("hub_nodes", json!({ "top_n": 1001 })),
            ("dead_code", json!({ "limit": 1.5 })),
            (
                "read_symbols",
                json!({ "targets": ["sym:x"], "include_neighbors": 256 }),
            ),
            (
                "read_symbols",
                json!({ "targets": ["sym:x"], "include_neighbors": -1 }),
            ),
        ];
        for (name, args) in invalid {
            assert_invalid(name, args);
        }

        // Boundary values stay valid.
        let valid = [
            (
                "regex_search",
                json!({ "pattern": "x", "limit": 10_000, "max_millis": 600_000 }),
            ),
            (
                "project_context",
                json!({ "project": "p", "token_budget": 16_000 }),
            ),
            ("investigate", json!({ "query": "q", "token_budget": 1 })),
            (
                "investigate_hydrate",
                json!({ "bundle_id": "b", "token_budget": 16_000 }),
            ),
            (
                "brain_impact",
                json!({ "symbol": "s", "depth": 15, "limit": 1000 }),
            ),
            (
                "blast_radius",
                json!({ "changed_files": ["a.rs"], "max_depth": 15 }),
            ),
            ("hub_nodes", json!({ "limit": 1000 })),
            ("dead_code", json!({ "limit": 1 })),
            (
                "read_symbols",
                json!({ "targets": ["sym:x"], "include_neighbors": 255 }),
            ),
        ];
        for (name, args) in valid {
            assert_valid(name, args);
        }
    }

    /// Structurally invalid scalars must be rejected at DISPATCH, not silently
    /// defaulted in a handler.
    ///
    /// Written to check a report that "MCP core methods accept structurally
    /// invalid scalar parameters" — they do not. Every handler reads scalars
    /// with `as_u64()`/`as_str()`, which return `None` for a wrong-typed value
    /// and fall back to a default, so the guarantee rests ENTIRELY on schema
    /// validation running first. Nothing pinned that, which is what made the
    /// report plausible. This pins it: if validation is ever bypassed or
    /// loosened, those 34 silent fallbacks become real.
    #[test]
    fn structurally_invalid_scalars_are_rejected_at_dispatch() {
        for (name, args) in [
            // A quoted number where an integer is declared.
            ("brain_search", json!({ "query": "x", "limit": "50" })),
            ("flow_trace", json!({ "symbol": "s", "max_depth": "3" })),
            // A bool where an integer is declared.
            ("brain_impact", json!({ "symbol": "s", "depth": true })),
            // A number where a string is declared.
            ("brain_search", json!({ "query": 42 })),
            // A fractional value for an integer field.
            ("brain_search", json!({ "query": "x", "limit": 2.5 })),
        ] {
            let result = validate_tool_arguments(name, &args);
            assert!(
                result.is_err(),
                "{name} accepted a structurally invalid scalar: {args}"
            );
        }
    }

    /// nw-175. EVERY registered tool must reject unknown argument names.
    ///
    /// Only 11 of 42 did. The consequence is not cosmetic: a mistyped argument
    /// was silently dropped and the handler used its default, so the caller got
    /// a plausible answer to a question they did not ask. That exact failure
    /// was hit twice in one review round — `flow_trace` accepting `max_dpeth`
    /// and silently tracing depth 10, and the config layer accepting
    /// `with_trigams` and silently leaving trigrams off.
    ///
    /// Asserted over the REGISTRY rather than a hand-listed set, so tool 42
    /// cannot be added without it.
    #[test]
    fn every_registered_schema_rejects_unknown_arguments() {
        let mut permissive = Vec::new();
        for schema in all_tool_schemas() {
            let name = schema["name"].as_str().unwrap_or("<unnamed>").to_string();
            let input = &schema["inputSchema"];
            // A schema with no properties at all takes no arguments; it still
            // must not silently swallow one.
            if input["additionalProperties"] != serde_json::json!(false) {
                permissive.push(name);
            }
        }
        assert!(
            permissive.is_empty(),
            "these tools silently accept undeclared arguments, so a typo becomes a \
             wrong answer instead of an error: {permissive:?}"
        );
    }

    /// nw-293. Every registered tool must declare MCP `annotations`, and
    /// `readOnlyHint` must be DERIVED from `MUTATING_TOOLS` rather than
    /// restated by hand.
    ///
    /// Zero of 42 tools declared any annotation, so `prune_stale` and
    /// `brain_status` were indistinguishable on the wire and no client could
    /// build an auto-approve policy without hard-coding our tool names.
    ///
    /// Asserted over the REGISTRY, not a hand-listed set, so tool 43 cannot be
    /// added without classifying it — the same rule
    /// `every_registered_schema_rejects_unknown_arguments` enforces for
    /// `additionalProperties`.
    #[test]
    fn every_registered_tool_declares_annotations_matching_the_mutating_list() {
        const HINTS: &[&str] = &[
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ];

        let payload = tool_list(false);
        let tools = payload["tools"]
            .as_array()
            .expect("tool_list returns a tools array");

        let mut missing = Vec::new();
        let mut mislabelled = Vec::new();

        for tool in tools {
            let name = tool["name"].as_str().expect("registered tool has a name");
            let Some(annotations) = tool.get("annotations").and_then(Value::as_object) else {
                missing.push(name.to_string());
                continue;
            };
            for hint in HINTS {
                assert!(
                    annotations.get(*hint).and_then(Value::as_bool).is_some(),
                    "{name}.annotations.{hint} is absent or not a boolean; a client \
                     cannot build an auto-approve policy from a partial annotation"
                );
            }
            let declared_read_only = annotations["readOnlyHint"]
                .as_bool()
                .expect("checked above");
            let actually_mutates = crate::http::MUTATING_TOOLS.contains(&name);
            if declared_read_only == actually_mutates {
                mislabelled.push(format!(
                    "{name}: readOnlyHint={declared_read_only} but \
                     MUTATING_TOOLS membership={actually_mutates}"
                ));
            }
        }

        assert!(
            missing.is_empty(),
            "these tools declare no `annotations`, so a mutating tool is \
             indistinguishable from a read-only one on the wire: {missing:?}"
        );
        assert!(
            mislabelled.is_empty(),
            "`readOnlyHint` contradicts the authoritative MUTATING_TOOLS gate: {mislabelled:?}"
        );

        // The six mutators must additionally be recoverable as a SET from the
        // annotations alone — that is the property an agent harness consumes.
        let declared_mutators: BTreeSet<&str> = tools
            .iter()
            .filter(|t| t["annotations"]["readOnlyHint"] == json!(false))
            .filter_map(|t| t["name"].as_str())
            .collect();
        let expected: BTreeSet<&str> = crate::http::MUTATING_TOOLS.iter().copied().collect();
        assert_eq!(declared_mutators, expected);
    }

    /// A read-only tool must not claim it may destroy anything, and a mutator's
    /// `destructiveHint`/`idempotentHint` must come from the single classified
    /// table rather than a second hand-list.
    #[test]
    fn mutating_tool_hints_come_from_the_canonical_classification() {
        let payload = tool_list(false);
        let tools = payload["tools"].as_array().unwrap();

        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            let annotations = &tool["annotations"];
            match crate::http::mutating_tool_hints(name) {
                Some((destructive, idempotent)) => {
                    assert_eq!(
                        annotations["destructiveHint"],
                        json!(destructive),
                        "{name}: destructiveHint diverged from MUTATING_TOOL_HINTS"
                    );
                    assert_eq!(
                        annotations["idempotentHint"],
                        json!(idempotent),
                        "{name}: idempotentHint diverged from MUTATING_TOOL_HINTS"
                    );
                }
                None => {
                    assert_eq!(
                        annotations["destructiveHint"],
                        json!(false),
                        "{name} is read-only but advertises that it may destroy data"
                    );
                    assert_eq!(annotations["idempotentHint"], json!(true), "{name}");
                }
            }
            // Every tool in this binary reads the LOCAL graph and local
            // filesystem. `brain_add_source`, the only candidate for an open
            // world, states in its own description that it "cannot index remote
            // URLs directly — only local filesystem paths".
            assert_eq!(
                annotations["openWorldHint"],
                json!(false),
                "{name}: no tool in this server reaches an open world"
            );
        }
    }

    #[test]
    fn bounded_tools_reject_unknown_arguments() {
        // Mistyped arg names must fail loudly instead of being
        // silently ignored (e.g. `neighbors` for `include_neighbors`).
        for (name, args) in [
            (
                "read_symbols",
                json!({ "targets": ["sym:x"], "neighbors": 2 }),
            ),
            ("regex_search", json!({ "pattern": "x", "patterns": ["y"] })),
            ("hub_nodes", json!({ "top": 5 })),
            (
                "brain_impact",
                json!({ "symbol": "s", "min_confidence": "low" }),
            ),
            ("dead_code", json!({ "max_results": 5 })),
            ("project_context", json!({ "project": "p", "budget": 100 })),
            ("investigate", json!({ "query": "q", "seeds": ["s"] })),
            (
                "blast_radius",
                json!({ "changed_files": ["a"], "files": ["b"] }),
            ),
        ] {
            assert_invalid(name, args);
        }

        // Documented aliases remain accepted.
        assert_valid("read_symbols", json!({ "uids_or_fqns": ["sym:x"] }));
        assert_valid("regex_search", json!({ "query": "x" }));
        assert_valid("hub_nodes", json!({ "top_n": 5 }));
    }

    #[test]
    fn regex_search_rejects_conflicting_pattern_and_query() {
        let error = assert_invalid("regex_search", json!({ "pattern": "a", "query": "b" }));
        assert!(
            error.contains("only one of 'pattern' or 'query'"),
            "conflicting aliases must be named: {error}"
        );
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

    #[test]
    fn local_dispatch_enforces_tools_allowlist_and_lite_mode() {
        // The gate must reject with the same error text on the local
        // path (the daemon-proxy test below asserts parity).
        let store = GraphStore::in_memory().unwrap();

        set_allowed_tools(vec!["brain_search".to_string()]);
        let error = dispatch(&store, None, "brain_status", json!({}), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "tool 'brain_status' is not in the allowed tools list; allowed: brain_search"
            ),
            "{error}"
        );
        ALLOWED_TOOLS.with(|c| *c.borrow_mut() = None);

        set_lite_mode(true);
        let error = dispatch(&store, None, "set_extension", json!({}), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("tool 'set_extension' is not available in lite mode"),
            "{error}"
        );
        // A lite tool is not blocked by lite mode.
        dispatch(&store, None, "brain_status", json!({}), None)
            .expect("lite tools must dispatch in lite mode");
        set_lite_mode(false);

        set_direct_read_only(true);
        let listed = tool_list(false);
        let names = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        // 42 registered minus the mutating tools direct read-only hides.
        assert_eq!(names.len(), 36);
        for mutator in crate::http::MUTATING_TOOLS {
            assert!(!names.contains(mutator), "direct mode advertised {mutator}");
            let error = dispatch(&store, None, mutator, json!({}), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("direct read-only mode"), "{error}");
        }
        set_direct_read_only(false);
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn daemon_proxy_enforces_tools_allowlist_and_lite_mode() {
        // The daemon-proxy path used to skip the --tools/--lite gate
        // entirely. The lazy channel never connects, so any call that PASSES
        // the gate fails with a transport error — proving the rejection below
        // happened at the gate, not the wire.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let channel = {
            let _guard = runtime.enter();
            tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy()
        };
        let mut client = DaemonGrpcClient::new(channel);

        set_allowed_tools(vec!["brain_search".to_string()]);
        let error = dispatch_via_daemon(&mut client, &runtime, "brain_status", json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "tool 'brain_status' is not in the allowed tools list; allowed: brain_search"
            ),
            "daemon path must reject with the same text as the local path: {error}"
        );
        ALLOWED_TOOLS.with(|c| *c.borrow_mut() = None);

        set_lite_mode(true);
        let error = dispatch_via_daemon(&mut client, &runtime, "set_extension", json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("tool 'set_extension' is not available in lite mode"),
            "{error}"
        );
        set_lite_mode(false);
    }

    #[test]
    fn brain_guide_rejects_config_arg_with_explicit_error() {
        // The daemon/MCP handler cannot honor an instance config;
        // silently ignoring it would return a guide for the wrong instance.
        let store = GraphStore::in_memory().unwrap();
        let error = tool_brain_guide(&store, json!({ "config": "/tmp/instance.toml" }))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cannot honor the 'config' argument"),
            "{error}"
        );
        // No config arg -> no error.
        tool_brain_guide(&store, json!({})).expect("guide without config must succeed");
        // Blank config strings are treated as absent.
        tool_brain_guide(&store, json!({ "config": "  " }))
            .expect("blank config must be treated as absent");
    }

    #[test]
    fn dead_code_count_contract_is_consistent() {
        use nestweaver_schema::{Symbol, Visibility};

        // unreachable_count must be the UNFILTERED total (like total_symbols /
        // reachable_symbols / dead_percentage); the post-min_confidence count
        // is reported separately as matching_count.
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&Symbol {
                uid: "sym:orphan".to_string(),
                name: "orphan_fn".to_string(),
                kind: SymbolKind::Function,
                repo_uid: "repo:a".to_string(),
                file_path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn orphan_fn()".to_string(),
                summary: None,
                content_hash: "h1".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();

        let all = tool_dead_code(&store, json!({}), None).expect("dead_code low");
        assert_eq!(all["unreachable_count"], 1);
        assert_eq!(all["matching_count"], 1);
        assert_eq!(all["returned"], 1);

        // Inferred visibility maps to Medium confidence, so a High filter
        // drops it from the results but NOT from the unfiltered total.
        let high = tool_dead_code(&store, json!({ "min_confidence": "high" }), None)
            .expect("dead_code high");
        assert_eq!(
            high["unreachable_count"], 1,
            "unreachable_count must be unfiltered: {high}"
        );
        assert_eq!(high["matching_count"], 0);
        assert_eq!(high["returned"], 0);
        assert_eq!(high["truncated"], false);
    }

    /// nw-316. `forwarded_bool(&args, "include_components", true)` guessed the
    /// tool's default at the FORWARDING layer because the proto3 `bool` it fed
    /// could not carry absence — and its own doc comment recorded the real fix
    /// (`optional` proto fields) as blocked "because `nestweaver-federation`
    /// constructs these request messages with exhaustive struct literals". That
    /// is not a blocker, it is a second file to change; both are in one commit
    /// now. The tool is the layer that DOCUMENTS the default, so it must be the
    /// layer that applies it.
    #[cfg(feature = "daemon")]
    #[test]
    fn absent_presence_tracked_bools_forward_as_absent() {
        for (key, args) in [
            ("include_components", json!({})),
            ("include_body", json!({})),
        ] {
            assert_eq!(
                args.get(key).and_then(|value| value.as_bool()),
                None,
                "an absent `{key}` must arrive absent, so the TOOL decides"
            );
        }
        // Explicit values are still carried, in both directions — presence
        // tracking is what makes `false` distinguishable from silence.
        assert_eq!(
            json!({ "include_components": false })
                .get("include_components")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn tool_errors_are_not_mislabeled_as_grpc_transport_errors() {
        // A tool-execution failure forwarded by the daemon is not a gRPC
        // transport error; the prefix must not claim it is.
        let tool_err = grpc_status_err(tonic::Status::internal(
            "tool note_get failed: provide either 'uid' or 'title'",
        ));
        assert_eq!(
            tool_err.to_string(),
            "tool note_get failed: provide either 'uid' or 'title'"
        );
        let cancelled = grpc_status_err(tonic::Status::deadline_exceeded(
            "brain_impact query cancelled: timeout",
        ));
        assert!(
            !cancelled.to_string().starts_with("gRPC error:"),
            "{cancelled}"
        );
        // Genuine transport failures keep the prefix.
        let transport = grpc_status_err(tonic::Status::unavailable("transport error"));
        assert_eq!(transport.to_string(), "gRPC error: transport error");
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

    #[cfg(feature = "daemon")]
    #[test]
    fn extension_tool_arg_errors_are_not_mislabeled_as_grpc_errors() {
        // The daemon wraps set_extension/query_extensions argument errors in
        // the standard `tool <name> failed:` format, so stdio MCP clients must
        // not see a misleading "gRPC error:" prefix (they never speak gRPC).
        let err = grpc_status_err(tonic::Status::invalid_argument(
            "tool query_extensions failed: provide either 'uid' or both 'key' and 'value'",
        ));
        assert_eq!(
            err.to_string(),
            "tool query_extensions failed: provide either 'uid' or both 'key' and 'value'"
        );
        let err = grpc_status_err(tonic::Status::invalid_argument(
            "tool set_extension failed: 'value' is required",
        ));
        assert!(!err.to_string().starts_with("gRPC error:"), "{err}");
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn daemon_brain_search_json_omits_empty_matched_headings() {
        // Parity with the local path: symbol rows carry no `matched_headings`
        // key at all — the daemon conversion used to emit a spurious `[]`.
        let response = nestweaver_proto::BrainSearchResponse {
            query: "needle".to_string(),
            engine: "bm25".to_string(),
            total_matches: 2,
            results: vec![
                nestweaver_proto::SearchResultItem {
                    uid: "sym:needle".to_string(),
                    canonical_id: None,
                    kind: "Symbol/Function".to_string(),
                    title: "needle".to_string(),
                    score: 1.0,
                    location: Some("src/lib.rs:1".to_string()),
                    matched_headings: Vec::new(),
                    inline_body: None,
                    vault_uid: None,
                },
                nestweaver_proto::SearchResultItem {
                    uid: "note:needle".to_string(),
                    canonical_id: None,
                    kind: "note".to_string(),
                    title: "Needle Note".to_string(),
                    score: 0.9,
                    location: None,
                    matched_headings: vec!["Needle Heading".to_string()],
                    inline_body: None,
                    vault_uid: Some("vlt:default:needle".to_string()),
                },
            ],
            expansion_terms: Vec::new(),
            returned_matches: 2,
            total_matches_relation: "eq".to_string(),
            truncated: false,
            semantic_applied: false,
            degraded_components: Vec::new(),
        };

        let value = daemon_brain_search_response_to_json(&response, false);
        assert!(value["results"][0].get("matched_headings").is_none());
        assert_eq!(
            value["results"][1]["matched_headings"],
            json!(["Needle Heading"])
        );
        // Note rows carry their vault; symbol rows omit the key.
        assert!(value["results"][0].get("vault_uid").is_none());
        assert_eq!(
            value["results"][1]["vault_uid"],
            json!("vlt:default:needle")
        );

        let concise = daemon_brain_search_response_to_json(&response, true);
        assert!(concise["results"][0].get("matched_headings").is_none());
        assert_eq!(
            concise["results"][1]["matched_headings"],
            json!(["Needle Heading"])
        );
    }

    #[test]
    fn brain_search_rejects_undocumented_arguments() {
        // Bogus args used to be silently accepted; the hardened schema
        // (additionalProperties: false, like regex_search) rejects them.
        let err = assert_invalid("brain_search", json!({ "query": "x", "bogus": true }));
        assert!(err.contains("additionalProperties"), "{err}");
    }

    #[test]
    fn brain_search_accepts_cache_bypass_arguments() {
        // brain_search is cacheable, so the documented cache-bypass args must
        // keep validating under additionalProperties: false.
        assert_valid("brain_search", json!({ "query": "x", "cache": "bypass" }));
        assert_valid("brain_search", json!({ "query": "x", "no_cache": true }));
    }

    /// EVERY cacheable tool, not just the seven that happened to declare them.
    ///
    /// `additionalProperties: false` turned an undeclared `no_cache` from
    /// "ignored" into "rejected", so a tool the dispatch layer treats as
    /// bypassable while its schema refuses the argument is a contradiction the
    /// caller cannot work around. Pinning this per-tool by hand is what let 18
    /// tools drift; this walks `CACHEABLE_TOOLS` so a new entry is covered the
    /// day it is added.
    #[test]
    fn every_cacheable_tool_accepts_the_cache_bypass_arguments() {
        // Synthesize the smallest argument object each schema accepts, so the
        // only thing under test is the cache pair.
        fn minimal_args(schema: &Value) -> serde_json::Map<String, Value> {
            let mut args = serde_json::Map::new();
            let properties = schema["properties"].as_object();
            for required in schema["required"].as_array().into_iter().flatten() {
                let Some(field) = required.as_str() else {
                    continue;
                };
                let declared = properties.and_then(|properties| properties.get(field));
                let kind = declared
                    .and_then(|value| value["type"].as_str())
                    .unwrap_or("string");
                let value = match kind {
                    "array" => json!(["x"]),
                    "integer" | "number" => json!(1),
                    "boolean" => json!(true),
                    "object" => json!({}),
                    // An enum must be satisfied with one of its own members.
                    _ => declared
                        .and_then(|value| value["enum"].as_array())
                        .and_then(|values| values.first().cloned())
                        .unwrap_or_else(|| json!("x")),
                };
                args.insert(field.to_string(), value);
            }
            args
        }

        // Some tools express "one of these two" as an alias rule enforced
        // beside the schema (`missing_alias_requirement`), not as `required`.
        // Satisfy it by adding declared properties until the rule is happy,
        // so the assertion below is about the cache pair and nothing else.
        fn satisfy_alias_requirement(
            name: &str,
            schema: &Value,
            args: &mut serde_json::Map<String, Value>,
        ) {
            if missing_alias_requirement(name, &Value::Object(args.clone())).is_none() {
                return;
            }
            let Some(properties) = schema["properties"].as_object() else {
                return;
            };
            for (field, declared) in properties {
                if args.contains_key(field) {
                    continue;
                }
                let value = match declared["type"].as_str().unwrap_or("string") {
                    "array" => json!(["x"]),
                    "integer" | "number" => json!(1),
                    "boolean" => json!(true),
                    "object" => json!({}),
                    _ => json!("x"),
                };
                args.insert(field.clone(), value);
                if missing_alias_requirement(name, &Value::Object(args.clone())).is_none() {
                    return;
                }
                args.remove(field);
            }
        }

        let schemas = all_tool_schemas();
        for name in CACHEABLE_TOOLS {
            let tool = schemas
                .iter()
                .find(|tool| tool["name"] == *name)
                .unwrap_or_else(|| panic!("{name} is cacheable but not registered"));
            let schema = &tool["inputSchema"];
            let properties = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} has no properties"));
            for field in ["cache", "no_cache"] {
                assert!(
                    properties.contains_key(field),
                    "{name} is cacheable but does not declare `{field}`, so \
                     `additionalProperties: false` rejects a caller that sends it"
                );
            }

            // Declaring it is not enough — it has to actually validate.
            let mut base = minimal_args(schema);
            satisfy_alias_requirement(name, schema, &mut base);
            for bypass in [json!({ "cache": "bypass" }), json!({ "no_cache": true })] {
                let mut args = base.clone();
                for (key, value) in bypass.as_object().expect("bypass args are an object") {
                    args.insert(key.clone(), value.clone());
                }
                let args = Value::Object(args);
                assert!(
                    validate_tool_arguments(name, &args).is_ok(),
                    "{name} rejected a cache-bypass call: {args}"
                );
            }
        }
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
        ("code_context", "Core retrieval"),
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
        ("compact_embeddings", "Status & maintenance"),
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

/// Enforce the `--tools` allowlist and `--lite` mode for a dispatch. Called by
/// EVERY dispatch entry point (local store, daemon proxy, hybrid routing) so a
/// restricted server cannot be reached by routing around the local path
/// The allowlist error text is part of the CLI/MCP contract — keep it
/// in sync with the `tools/list` filtering in [`tool_list`].
pub fn enforce_tool_allowed(name: &str) -> Result<(), anyhow::Error> {
    if is_direct_read_only() && crate::http::MUTATING_TOOLS.contains(&name) {
        return Err(anyhow!(
            "tool '{name}' is unavailable in direct read-only mode; remove --no-daemon to route mutations through the daemon"
        ));
    }
    if is_lite_mode() && !LITE_TOOLS.contains(&name) {
        return Err(anyhow!(
            "tool '{name}' is not available in lite mode; allowed: {}",
            LITE_TOOLS.join(", ")
        ));
    }
    let allowed = ALLOWED_TOOLS.with(|c| c.borrow().clone());
    if let Some(ref names) = allowed
        && !names.iter().any(|a| a == name)
    {
        return Err(anyhow!(
            "tool '{name}' is not in the allowed tools list; allowed: {}",
            names.join(", ")
        ));
    }
    Ok(())
}

/// Dispatch a `tools/call` to the named tool. The optional `tantivy`
/// index, when present, drives hybrid retrieval in `brain_context` and
/// upgrades `brain_search` from substring to BM25.
///
/// When `--tools` was specified, calls to tools outside the allowlist
/// are rejected with a descriptive error.
/// The one author of result provenance, and the type that proves it ran.
///
/// # What nw-315 claimed, and what was true
///
/// Lane D-2 landed "author result provenance once, at the tool layer" and
/// asserted it with `every_tool_that_answers_stamps_its_provenance`, which
/// calls [`dispatch`]. That test passed. The claim was still false: there are
/// **two** dispatch tables in this file, not one.
///
/// - [`dispatch_cancellable`] — the in-process seam. Stamped.
/// - [`dispatch_via_daemon`] — a PEER, not a caller: a second, complete tool
///   table used whenever a daemon is running, which is the default
///   single-machine setup (`src/main.rs` picks it at `run_stdio_server_daemon`).
///   It stamped nothing.
///
/// Within that second table the behaviour split again, by whether the daemon
/// RPC is a typed proto or a JSON pass-through. `hub_nodes` and
/// `brain_doc_stats` forward `result_json` verbatim, so the daemon's own stamp
/// survived; `brain_search` is a typed `BrainSearchResponse` with no `_meta`
/// field, and the client then REBUILDS a fresh object in
/// `daemon_brain_search_response_to_json`. That is why `brain_search` over MCP
/// carried no `_meta` while `brain search --json` did, and why `hub_nodes` over
/// MCP carried one while `hubs --json` did not.
///
/// # Why this is a type and not a convention
///
/// A test cannot cover the daemon seam: reaching a successful response there
/// needs a live daemon, so every existing assertion about it (tools.rs:1143,
/// 1222, 1234) is on the ERROR path. A convention that cannot be tested is how
/// this defect got here. So the invariant is carried by the compiler instead:
/// [`Unstamped`]'s field is private to THIS MODULE, and [`stamp`] is the only
/// thing that can take a `Value` back out. A dispatch seam that returns an
/// `Unstamped` has no way to hand it to a caller without the stamp having run,
/// and a third seam added later cannot forget, because there is nothing else
/// for it to return.
mod provenance_seam {
    use serde_json::Value;

    /// A tool result that has NOT yet crossed the provenance seam.
    pub(super) struct Unstamped(Value);

    impl Unstamped {
        pub(super) fn new(value: Value) -> Self {
            Self(value)
        }
    }

    /// Stamp `_meta` and release the value. The only way out of [`Unstamped`].
    ///
    /// `ensure` and not `set`: a federating caller knows strictly more than this
    /// layer does (it can name upstreams and a background staleness verdict this
    /// process cannot compute without I/O), so its richer stamp wins — including
    /// the daemon's own stamp arriving through a `result_json` pass-through.
    /// What this layer can say honestly is that the answer came from the local
    /// graph, and saying that is what makes the absence of a richer verdict
    /// legible.
    pub(super) fn stamp(result: Unstamped) -> Value {
        let mut value = result.0;
        nestweaver_schema::provenance::ensure(
            &mut value,
            nestweaver_schema::provenance::SCOPE_LOCAL,
            &[nestweaver_schema::provenance::SOURCE_LOCAL],
            &[],
        );
        value
    }
}

use provenance_seam::Unstamped;

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
/// `visible` carries the caller's per-repo visibility, resolved by the
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
    // Enforce --tools allowlist and --lite mode.
    enforce_tool_allowed(name)?;

    validate_tool_arguments(name, &args)?;

    // nw-C2: a query landing inside a normal index-publication window used to
    // fail outright with an internal-looking assertion string. Wait it out
    // first — the window is normally well under a second, so this converts the
    // common case from a hard failure into a latency blip.
    //
    // `brain_status` is excluded on purpose: it is the diagnostic that REPORTS
    // this condition, so it must answer immediately rather than block on it.
    if name != "brain_status" {
        wait_out_index_publication(store, cancel);
    }

    // F16: serve cacheable read tools from (or populate) the response cache.
    // Correctness rests on the cache KEY — see `maybe_cached`.
    let result = if is_cacheable_tool(name) && !cache_bypassed(&args) {
        maybe_cached(store, tantivy, name, args, embed_model, cancel, visible)
    } else {
        dispatch_uncached(store, tantivy, name, args, embed_model, cancel, visible)
    };

    // nw-315: THE tool layer is the author of provenance, because it is the
    // only layer every route passes through.
    //
    // `_meta` (scope/sources/stale_repos) used to be written by the CLI
    // presentation layer — `attach_local_meta` on the direct route, the
    // federation client on the daemon route — and MCP over stdio has no
    // presentation layer, so it never received the field at all. Not dropped:
    // never added. Meanwhile `SERVER_INSTRUCTIONS` (lib.rs) promises the agent
    // "Results include `_meta.sources` indicating which data sources
    // contributed", so the server documented a field it did not send.
    //
    // Stamped here rather than in the stdio server so that the property holds
    // for EVERY route through `dispatch` — stdio, HTTP, and the daemon's
    // `dispatch_tool_json` — instead of for the one route that was reported.
    //
    // `ensure` and not `set`: a federating caller knows strictly more than this
    // layer does (it can name upstreams and a background staleness verdict this
    // process cannot compute without I/O), so its richer stamp wins. What this
    // layer can say honestly is that the answer came from the local graph, and
    // saying that is what makes the absence of a richer verdict legible.
    let result = result.map(|value| provenance_seam::stamp(Unstamped::new(value)));

    // Tools that do not consult PageRank still succeed during a dirty
    // publication, so the classification is applied to the ERROR rather than
    // used to fail early.
    result.map_err(|error| classify_index_publication_error(store, error))
}

/// Bounded wait for `NESTWEAVER_INDEX_PUBLICATION_WAIT_MS` (default 3000,
/// clamped to 30s) before letting a query meet a dirty publication.
fn index_publication_wait() -> std::time::Duration {
    std::time::Duration::from_millis(INDEX_PUBLICATION_WAIT_MS.with(|c| c.get()))
}

/// Poll the marker FILE until the publication is clean or the budget expires.
///
/// Two constraints, both deliberate:
///
/// * It polls `is_index_publication_dirty`, which is file-based and therefore
///   genuinely cross-process. `index_publication_lease.available` is an
///   **in-process** condvar and this reader is commonly in a different process
///   from the indexing writer, so a condvar-based wait could never fire for it
///   — and an in-process test of one would pass for the wrong reason.
/// * It does NOT acquire the publication lease. Acquisition is exclusive and
///   blocking, so a waiting reader would serialize every other reader behind
///   the writer, turning a latency blip into a real outage.
fn wait_out_index_publication(
    store: &GraphStore,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) {
    if !store.is_index_publication_dirty() {
        return;
    }
    let budget = index_publication_wait();
    if budget.is_zero() {
        return;
    }
    // Never wait on a publication that cannot complete. A wedged marker names
    // a writer we can prove is dead (or one we cannot attribute at all), so
    // waiting buys nothing and charges the full budget to EVERY tool on EVERY
    // call — including tools that never consult PageRank and would otherwise
    // have answered in milliseconds. Fail straight through to the classified
    // WEDGED error, which names the repair.
    if let Some(db_path) = store.db_path()
        && nestweaver_engine::index_publication::status(db_path).is_wedged()
    {
        return;
    }
    let started = std::time::Instant::now();
    let cancelled = || cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));
    if store.wait_until_index_publication_clean_interruptible(budget, &cancelled) {
        tracing::debug!(
            "waited {}ms for an in-flight index publication to finish",
            started.elapsed().as_millis()
        );
    }
}

/// Replace the verbatim `StoreError` string from a fail-closed ranked query
/// with a message that says whether the condition is TRANSIENT or WEDGED, and
/// — crucially — that "dirty" here means an index PUBLICATION, not a dirty git
/// working tree. A user who hit this concluded that NestWeaver is useless while
/// you work in a repo; that wrong conclusion is itself part of the bug.
fn classify_index_publication_error(store: &GraphStore, error: anyhow::Error) -> anyhow::Error {
    // Covers both fail-closed strings: "PageRank unavailable during dirty
    // index publication" and "graph generation exhausted during index
    // publication".
    if !format!("{error:#}").contains("index publication") {
        return error;
    }
    let Some(db_path) = store.db_path().map(std::path::Path::to_path_buf) else {
        return error;
    };
    let status = nestweaver_engine::index_publication::status(&db_path);
    if !status.dirty {
        return error;
    }
    let writer = match (status.writer_pid, status.writer_alive) {
        (Some(pid), Some(true)) => format!("writer pid {pid} is running"),
        (Some(pid), _) => format!("writer pid {pid} is NOT running"),
        _ => "no writer pid recorded in the marker".to_string(),
    };
    let age = status
        .marker_age_s
        .map(|s| format!("{s}s"))
        .unwrap_or_else(|| "unknown".to_string());
    let waited_ms = index_publication_wait().as_millis();
    let preamble = "This is an index PUBLICATION window, not a dirty git working tree — \
                    editing files in a repo does not cause it.";
    if status.is_wedged() {
        let repair = status.repair_command_for(&db_path);
        anyhow!(
            "index publication WEDGED: ranked queries are failing closed because {} exists \
             and {writer} (marker age {age}). {preamble} The PageRank and generation sidecars \
             may predate the committed graph, so serving them would return wrong ranks. \
             A HUMAN must recover with (there is no MCP tool for this): {repair}{}",
            status.marker_path,
            if status.writer_reason.as_deref()
                == Some(nestweaver_store::index_publication::MARKER_REASON_CANCELLED)
            {
                "  (that publication was left dirty by a run that committed after \
                 cancellation, so the graph may also be incomplete — follow up with \
                 `nestweaver index --repo <path> --force`)"
            } else {
                ""
            }
        )
    } else {
        anyhow!(
            "index publication TRANSIENT: an index publication is in flight ({writer}, marker \
             age {age}) and did not finish within the {waited_ms}ms wait, so ranked queries are \
             failing closed. {preamble} Retry shortly; raise \
             NESTWEAVER_INDEX_PUBLICATION_WAIT_MS to wait longer."
        )
    }
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
        "code_context" => tool_code_context(store, args),
        "brain_search" => tool_brain_search(store, tantivy, args, visible),
        "note_get" => tool_note_get(store, args),
        "backlinks" => tool_backlinks(store, args),
        "brain_status" => tool_brain_status(store, tantivy),
        "brain_add_source" => tool_brain_add_source(store, args),
        "brain_remove_source" => tool_brain_remove_source(store, args),
        "prune_stale" => tool_prune_stale(store),
        "compact_embeddings" => tool_compact_embeddings(store, args),
        "cross_repo_contracts" => tool_cross_repo_contracts(store, args),
        "brain_impact" => tool_brain_impact(store, args, cancel, visible),
        "brain_guide" => tool_brain_guide(store, args),
        "flow_trace" => tool_flow_trace(store, args, cancel),
        "detect_changes" => tool_detect_changes(store, args),
        "clusters" => tool_clusters(store, args),
        "stale_check" => tool_stale_check(store),
        "set_extension" => tool_set_extension(args),
        "query_extensions" => tool_query_extensions(args),
        "brain_diff" => tool_brain_diff(store, args, visible),
        "project_context" => tool_project_context(store, tantivy, args, embed_model, cancel),
        "dead_code" => tool_dead_code(store, args, cancel),
        "hub_nodes" => tool_hub_nodes(store, args),
        "bridge_nodes" => tool_bridge_nodes(store, args),
        "blast_radius" => tool_blast_radius(store, args, cancel, visible),
        "get_summary" => tool_get_summary(store, args),
        "read_symbols" => tool_read_symbols(store, args),
        "regex_search" => tool_regex_search(store, args, cancel),
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

// H1: `RESPONSE_SHAPE_VERSION` — a content digest of the workspace sources,
// computed by `build.rs`, that identifies the response SHAPES this binary
// produces.
//
// The cache's other validity checks (`graph_generation`, `scope_digest`, TTL)
// all describe the GRAPH. None of them describes the BINARY, so before this
// existed an upgrade that added a field to a cached tool's response kept
// serving pre-upgrade entries — the old shape, missing the new field — for up
// to the full 24h TTL on an untouched graph.
//
// It is DERIVED, not hand-maintained, precisely so that a future author who
// adds a response field does not have to remember anything: editing any
// workspace source changes the digest and the cache invalidates itself. See
// `crates/nestweaver-mcp/build.rs` for the scope and its one documented gap.
include!(concat!(env!("OUT_DIR"), "/response_shape_version.rs"));

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

/// Semantic context is a function of both graph contents and the exact model
/// instance available to this process. Include a versioned model namespace in
/// its cache/single-flight key so a loading request cannot join or hit a ready
/// model request, and a replaced model cannot inherit its predecessor's
/// response. Process-local identity intentionally prevents semantic entries
/// from being reused after a restart when the configured model name may point
/// at different artifacts or an external endpoint may have changed.
fn semantic_cache_salt(name: &str, embed_model: Option<&dyn EmbedQueryFn>) -> u64 {
    if !matches!(name, "brain_context" | "project_context") {
        return 0;
    }
    use std::hash::{Hash, Hasher};
    static PROCESS_NAMESPACE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let process_namespace = *PROCESS_NAMESPACE.get_or_init(|| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut hasher);
        (&PROCESS_NAMESPACE as *const std::sync::OnceLock<u64> as usize).hash(&mut hasher);
        hasher.finish()
    });
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    "semantic-response-cache-v2".hash(&mut hasher);
    process_namespace.hash(&mut hasher);
    embed_model.is_some().hash(&mut hasher);
    if let Some(model) = embed_model {
        let data_pointer = model as *const dyn EmbedQueryFn as *const () as usize;
        data_pointer.hash(&mut hasher);
    }
    hasher.finish()
}

fn semantic_response_is_degraded(name: &str, value: &Value) -> bool {
    matches!(name, "brain_context" | "project_context")
        && value
            .get("degraded_components")
            .and_then(Value::as_array)
            .is_some_and(|components| !components.is_empty())
}

fn ensure_dispatch_not_cancelled(
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), anyhow::Error> {
    if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
        return Err(anyhow::Error::new(nestweaver_store::StoreError::Cancelled(
            nestweaver_store::CancelReason::Timeout,
        )));
    }
    Ok(())
}

// ── Single-flight coalescing for cache misses ────────────────────────────────
/// Identical concurrent cacheable calls (same tool, args, visibility,
/// generation, scope) otherwise stampede: every dispatch thread misses its
/// thread-local response cache and runs the same expensive query — a
/// brain_context semantic leg is a full BERT forward pass, so 30 parallel
/// identical calls burned 30 embeds and piled into the tool timeout.
/// Coalesce them: the first caller (leader) computes, followers wait on the
/// shared slot and receive a clone of the leader's result. `anyhow::Error`
/// is not `Clone`, so the slot carries the error's message and followers
/// re-wrap it.
struct InFlightSlot {
    result: std::sync::Mutex<Option<Result<Value, String>>>,
    ready: std::sync::Condvar,
}

/// In-flight key: everything that identifies one deterministic computation —
/// db identity, the visibility-salted response-cache key, and the freshness
/// pair (graph generation, whole-db scope digest) the response cache uses.
type InFlightKey = (std::path::PathBuf, u64, u64, u64);

static IN_FLIGHT: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<InFlightKey, std::sync::Arc<InFlightSlot>>>,
> = std::sync::OnceLock::new();

/// RAII handle for the leader of a coalesced computation. Dropping it —
/// normal return, error, or panic unwind alike — removes the flight entry
/// and wakes every follower; if no result was stored (panic path) followers
/// receive an error instead of waiting forever.
struct FlightLeader {
    key: InFlightKey,
    slot: std::sync::Arc<InFlightSlot>,
}

impl FlightLeader {
    /// Store the outcome for waiting followers; `drop` then unregisters the
    /// flight and notifies them.
    fn finish(self, result: Result<Value, String>) {
        *self.slot.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    }
}

impl Drop for FlightLeader {
    fn drop(&mut self) {
        if let Some(flights) = IN_FLIGHT.get() {
            let mut flights = flights.lock().unwrap_or_else(|e| e.into_inner());
            // Remove only if the entry still points at THIS slot — a later
            // leader for the same key may already have replaced it.
            if flights
                .get(&self.key)
                .is_some_and(|s| std::sync::Arc::ptr_eq(s, &self.slot))
            {
                flights.remove(&self.key);
            }
        }
        let mut result = self.slot.result.lock().unwrap_or_else(|e| e.into_inner());
        if result.is_none() {
            *result = Some(Err(
                "in-flight leader dropped before producing a result".to_string()
            ));
        }
        drop(result);
        self.slot.ready.notify_all();
    }
}

/// Run `compute` at most once among concurrent callers sharing `key`
/// (single-flight). The first caller computes; followers block on the shared
/// slot's condvar and receive a clone of the leader's result. Cancellable
/// followers use bounded waits so they can stop independently without
/// disturbing the leader or other followers.
fn coalesce_in_flight(
    key: InFlightKey,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    compute: impl FnOnce() -> Result<Value, anyhow::Error>,
) -> Result<Value, anyhow::Error> {
    let (slot, leader) = {
        let flights =
            IN_FLIGHT.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        let mut flights = flights.lock().unwrap_or_else(|e| e.into_inner());
        match flights.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => {
                (std::sync::Arc::clone(e.get()), None)
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                let slot = std::sync::Arc::new(InFlightSlot {
                    result: std::sync::Mutex::new(None),
                    ready: std::sync::Condvar::new(),
                });
                e.insert(std::sync::Arc::clone(&slot));
                let leader = FlightLeader {
                    key,
                    slot: std::sync::Arc::clone(&slot),
                };
                (slot, Some(leader))
            }
        }
    };

    let Some(leader) = leader else {
        // Follower: wait for the leader to publish its result, then share it.
        let mut result = slot.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            ensure_dispatch_not_cancelled(cancel)?;
            if let Some(res) = &*result {
                return match res {
                    Ok(value) => Ok(value.clone()),
                    Err(msg) => Err(anyhow!("{msg}")),
                };
            }
            if cancel.is_some() {
                let (next_result, _) = slot
                    .ready
                    .wait_timeout(result, std::time::Duration::from_millis(25))
                    .unwrap_or_else(|e| e.into_inner());
                result = next_result;
            } else {
                result = slot.ready.wait(result).unwrap_or_else(|e| e.into_inner());
            }
        }
    };

    let result = compute();
    leader.finish(
        result
            .as_ref()
            .map(|v| v.clone())
            .map_err(|e| format!("{e:#}")),
    );
    result
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
    let key = mix_visibility_cache_key(key, semantic_cache_salt(name, embed_model));
    let generation = store.graph_generation();
    let scope_digest = whole_db_scope_digest(&db_path);

    // Lazily initialise the in-process cache for this db path, then check for a hit.
    let hit_bytes = RESPONSE_CACHE.with(|map| {
        let mut map = map.borrow_mut();
        let cache = map.entry(db_path.clone()).or_insert_with(|| {
            nestweaver_store::cache::ResponseCache::open(&db_path, max_mb, RESPONSE_SHAPE_VERSION)
        });
        cache.get(key, generation, scope_digest)
    });

    if let Some(bytes) = hit_bytes
        && !store.is_index_publication_dirty()
        && store.graph_generation() == generation
    {
        // No save() on hit — LRU timestamp update is not worth a disk round-trip.
        let value: Value =
            serde_json::from_slice(&bytes).with_context(|| "decode cached response")?;
        // Defense in depth for persisted entries produced by an older binary:
        // degraded semantic responses are transient readiness/inference states,
        // never durable answers. Ignore them even if their legacy key matches.
        if !semantic_response_is_degraded(name, &value) {
            CACHE_HITS.with(|c| c.set(c.get() + 1));
            return Ok(value);
        }
    }

    CACHE_MISSES.with(|c| c.set(c.get() + 1));
    // Single-flight: concurrent identical calls share one computation
    // instead of stampeding it (see `coalesce_in_flight`).
    let flight_key: InFlightKey = (db_path.clone(), key, generation, scope_digest);
    let result = coalesce_in_flight(flight_key, cancel, || {
        let result = dispatch_uncached(store, tantivy, name, args, embed_model, cancel, visible)?;
        // Check inside the leader computation so cancellation is converted to
        // an error before FlightLeader publishes to followers.
        ensure_dispatch_not_cancelled(cancel)?;
        Ok(result)
    })?;
    // Every caller must honor its own cancellation state. Followers bypass
    // the leader closure above, so re-check after the shared result arrives
    // before this caller can return or publish it.
    ensure_dispatch_not_cancelled(cancel)?;
    if store.is_index_publication_dirty() || store.graph_generation() != generation {
        return Ok(result);
    }
    if semantic_response_is_degraded(name, &result) {
        return Ok(result);
    }
    match serde_json::to_vec(&result) {
        Ok(bytes) => {
            // Insert into the in-process cache, then decide whether to flush.
            let should_flush = RESPONSE_CACHE.with(|map| {
                let mut map = map.borrow_mut();
                let cache = map.entry(db_path.clone()).or_insert_with(|| {
                    nestweaver_store::cache::ResponseCache::open(
                        &db_path,
                        max_mb,
                        RESPONSE_SHAPE_VERSION,
                    )
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
            let cache = nestweaver_store::cache::ResponseCache::open(
                db_path,
                max_mb,
                RESPONSE_SHAPE_VERSION,
            );
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
                let reader = nestweaver_engine::content_reader::FilesystemReader::with_limits(
                    &root,
                    configured_index_limits(),
                );
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
        let reader = nestweaver_engine::content_reader::FilesystemReader::with_limits(
            &root,
            configured_index_limits(),
        );
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
    let limits = current_instance_config()
        .map(|config| config.indexing.limits())
        .unwrap_or_default();
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
        nestweaver_engine::content_reader::GitBareReader::from_head_with_limits(&bare_path, limits)
            .ok()
    } else {
        Some(
            nestweaver_engine::content_reader::GitBareReader::with_limits(
                &bare_path,
                &repo.indexed_sha,
                limits,
            ),
        )
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
                    "description": "Symbol UIDs (sym:...), names, or FQNs to read. One of 'targets' or 'uids_or_fqns' is required."
                },
                "uids_or_fqns": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Backward-compatible alias for targets."
                },
                "include_neighbors": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 255,
                    "description": "Include N adjacent symbols in the same file (default 0, max 255)."
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
            "additionalProperties": false
        }
    })
}

/// F3: trigram-accelerated regex search over indexed text. Lets agents run a
/// real regex against Section bodies, Note titles, and Symbol signatures
/// without shelling out to rg/grep.
///
/// The node kinds `regex_search`/`count_patterns` can filter on, as advertised
/// in their schemas. Anything else used to silently match no candidates and
/// return empty results — fail loudly instead.
const REGEX_SEARCH_KINDS: &[&str] = &["Section", "Note", "Symbol"];

fn validate_regex_kinds(kinds: Option<&[String]>) -> Result<(), anyhow::Error> {
    if let Some(kinds) = kinds {
        for kind in kinds {
            if !REGEX_SEARCH_KINDS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(kind))
            {
                return Err(anyhow!(
                    "unknown kind '{kind}'; expected one of: {}",
                    REGEX_SEARCH_KINDS.join(", ")
                ));
            }
        }
    }
    Ok(())
}

fn tool_regex_search(
    store: &GraphStore,
    args: Value,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Value, anyhow::Error> {
    let pattern = args
        .get("pattern")
        .or_else(|| args.get("query"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'pattern' must be a string"))?;
    if pattern.trim().is_empty() {
        // Same policy as count_patterns: an empty pattern matches everything,
        // which is a scan-cost/response-amplification lever, not a query.
        return Err(anyhow!("empty pattern strings are not allowed"));
    }

    // Note: regex_search works in server mode — GraphStore::regex_search
    // searches over indexed symbol text, not raw source files on disk.

    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let kinds = parse_string_array(&args, "kinds");
    validate_regex_kinds(kinds.as_deref())?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let max_millis = args.get("max_millis").and_then(|v| v.as_u64());

    let res = store
        .regex_search_cancellable(
            pattern,
            path_prefix,
            kinds.as_deref(),
            limit,
            max_millis,
            cancel,
        )
        .map_err(|e| anyhow!("regex_search: {e}"))?;
    // nw-097: the note now rides on RegexSearchResult itself, attached by the
    // store, so the CLI and daemon paths carry it too. This tool used to bolt it
    // on here, which is exactly why only MCP had it.
    Ok(serde_json::to_value(res)?)
}

fn tool_schema_regex_search() -> Value {
    json!({
        "name": "regex_search",
        "description": "Run a Rust regex against indexed text (section bodies, note titles, symbol signatures) with database-bound, per-scope acceleration and final Rust-regex verification.\n\nGuidelines:\n- Use for exact pattern matching; for fuzzy/semantic lookup use brain_search instead\n- Output names ready/dirty/error scopes, posting hits, hydrated and verified candidate counts, exact truncation reason, and planning/hydration/verification timings\n- scanned_fallback means one or more scopes were safely scanned; stale_index means an existing shard was unavailable or stale, never that matches were dropped\n\nLimitations:\n- Candidate cap of 200000 or time budget (default 2000ms) may truncate results; truncation_reason distinguishes the bound\n- Does not search binary files or unindexed content",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust regex pattern. Example: \"fn\\\\s+authenticate\" or \"(?i)todo\". One of 'pattern' or 'query' is required." },
                "query": { "type": "string", "description": "Backward-compatible alias for pattern." },
                "path_prefix": { "type": "string", "description": "Restrict to nodes whose file path starts with this prefix." },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict to these node kinds: Section, Note, Symbol (case-insensitive)."
                },
                "limit": { "type": "integer", "minimum": 1, "maximum": 10000, "description": "Maximum results to return (1-10000; the candidate cap is 200000). Default: unlimited (capped by the candidate budget)." },
                "max_millis": { "type": "integer", "minimum": 1, "maximum": 600000, "description": "Wall-clock time budget in milliseconds (1-600000). Default 2000." },
                "cache": { "type": "string", "description": "Set to \"bypass\" to skip the response cache for this call." },
                "no_cache": { "type": "boolean", "description": "When true, skip the response cache for this call." }
            },
            "additionalProperties": false
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
    validate_regex_kinds(kinds.as_deref())?;

    let counts = store
        .count_patterns(&patterns, path_prefix, kinds.as_deref())
        .map_err(|e| anyhow!("count_patterns: {e}"))?;
    Ok(json!({ "patterns": serde_json::to_value(counts)? }))
}

fn tool_schema_count_patterns() -> Value {
    json!({
        "name": "count_patterns",
        "description": "Count regex matches across indexed text without returning the matches themselves — useful for frequency analysis.\n\nGuidelines:\n- Pass multiple patterns to compare counts in one call\n- Returns per-pattern {pattern, total_matches, files_matched, top_files:[{path,count}], stale_index}\n- For actual match text, use regex_search instead\n\nLimitations:\n- Counts occurrences (non-overlapping, leftmost-first), the same thing `grep -o | wc -l` counts\n- Frontmatter is not in the exact-match corpus\n- Same trigram/fallback behavior as regex_search (stale_index flags a bypassed stale posting table)",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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
    let max_suggestions = read_limit(&args, "max_suggestions", 5, 1, 50)?;
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
    // nw-341: the ONLY axis that unblocks verification. The rows sort
    // unresolved-first then by ascending confidence, so the 0.92
    // nearest-ancestor and 0.95 same-folder tiers are the tail -- exactly what
    // a cap removes and exactly what a reviewer of the tier ladder has to see.
    // Reversing the sort is not an option: it would regress nw-297's
    // `genuinely_broken_links_sort_before_lower_tier_resolutions`.
    let offset = read_limit(&args, "offset", 0, 0, RESULT_LIMIT_MAX)?;
    let all_links = broken_links(store, max_suggestions)?;
    // nw-297: classify over the POPULATION, before the window. The page is
    // a sample, and a caller that reads the page's own composition as the
    // vault's composition gets the wrong answer at every limit — which is
    // exactly what the CLI's summary line did.
    let unresolved = all_links.iter().filter(|l| l.is_unresolved()).count();
    let low_confidence = all_links.len() - unresolved;
    let rows: Vec<Value> = all_links
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, serde_json::Error>>()?;
    // nw-341: through `Bounded`, not hand-rolled. This was the last bounded
    // list in the catalogue still building its own (total, returned) pair, so
    // it was also the only one that never emitted `truncated` -- a caller could
    // not tell a complete page from a cut one without comparing two numbers.
    let mut out = json!({
        "unresolved": unresolved,
        "low_confidence": low_confidence,
        "offset": offset,
    });
    Bounded::window(rows, offset, limit).merge_into(&mut out, "broken_links");
    Ok(out)
}

fn tool_schema_brain_broken_links() -> Value {
    json!({
        "name": "brain_broken_links",
        "description": "Find wikilinks in the vault that did not resolve cleanly. TWO POPULATIONS are returned together: links that resolved at a lower tier (confidence < 1.0 — same-folder or filename-stem matches, which are NOT broken) and links that resolved to nothing (`resolved_target_uid` absent — the only genuinely broken ones).\n\nGuidelines:\n- `unresolved` and `low_confidence` count the WHOLE population, not the returned page; `returned` and `truncated` describe the page and `total` is the pre-offset population. Read the population counts, never the page composition\n- Results are ordered unresolved-first, then by ascending confidence, so the first page is the most severe — and the HIGHEST-confidence tiers are the tail, reachable only via `offset`\n- Each result includes fuzzy-matched suggested target UIDs for repair\n- Returns empty when no vault is indexed\n\nLimitations:\n- Only detects wikilink resolution issues, not broken external URLs\n- Suggestions are fuzzy title matches, not guaranteed correct targets",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                // `maximum: 50`, not 1000: suggestions multiply PER broken
                // link, so a 1000-suggestion cap across 1328 links is a cross
                // product. 10x the default is the same ratio `clusters.members`
                // uses against its own preview.
                "max_suggestions": limit_schema(
                    "Max suggested target UIDs per broken link (1-50, default 5).", 5, 1, 50),
                "limit": limit_schema(
                    "Max broken links to return (1-1000, default 50). The total count is always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX),
                "offset": bounded_integer_schema(
                    "Skip this many rows before the page (default 0). Rows sort unresolved-first then by ASCENDING confidence, so the high-confidence tiers (0.90/0.92/0.95) are the TAIL — offset is how you reach them. `total` stays the PRE-offset population.",
                    0, RESULT_LIMIT_MAX)
            }
        }
    })
}

fn tool_brain_orphan_documents(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let vault = args.get("vault").and_then(|v| v.as_str());
    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let allowlist = parse_string_array(&args, "allowlist").unwrap_or_default();
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
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
            "additionalProperties": false,
            "properties": {
                "vault": { "type": "string", "description": "Restrict to this vault UID." },
                "path_prefix": { "type": "string", "description": "Restrict to notes whose file path starts with this prefix." },
                "allowlist": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Note paths/titles to exclude (overrides the default index/MOC allowlist when provided)."
                },
                "limit": limit_schema(
                    "Max orphan documents to return (1-1000, default 50). The total count is always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
            }
        }
    })
}

fn tool_brain_topic_clusters(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let resolution = args
        .get("resolution")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
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
        "description": "Discover thematic structure of a vault by running Louvain-style local moving community detection over the note-to-note wikilink graph.\n\nGuidelines:\n- Each cluster is labelled by its most central member (highest PageRank)\n- Adjust resolution parameter: higher yields more, smaller clusters\n- Returns empty when no vault is indexed\n\nLimitations:\n- Only considers wikilink edges between notes, not tags or code references\n- Label quality depends on the most-central note having a descriptive title",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "resolution": {
                    "type": "number",
                    "description": "Community-detection resolution — higher yields more, smaller clusters (default 0.5).",
                    "default": 0.5
                },
                "limit": limit_schema(
                    "Max clusters to return (1-1000, default 50). The total count is always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
            }
        }
    })
}

fn tool_brain_tag_graph(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
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
            "additionalProperties": false,
            "properties": {
                "tag": { "type": "string", "description": "Optional focus tag (with or without leading #). When omitted, returns the full tag co-occurrence graph for all tags." },
                "limit": limit_schema(
                    "Max tags to return in the all-tags listing (1-1000, default 50). Ignored when a specific tag is queried.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
            }
        }
    })
}

fn tool_brain_doc_stats(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let top_tags_limit = read_limit(&args, "top_tags_limit", 10, 1, RESULT_LIMIT_MAX)?;
    let stats = doc_stats(store, top_tags_limit)?;
    Ok(serde_json::to_value(&stats)?)
}

fn tool_schema_brain_doc_stats() -> Value {
    json!({
        "name": "brain_doc_stats",
        "description": "Get a one-shot health summary of a vault's document graph — note counts, broken links, orphans, tag distribution, and notes-by-year.\n\nGuidelines:\n- Call once for a quick vault health overview before deeper analysis\n- All seven keys are always returned, even on an empty vault (zeros/empty collections)\n- Output: {total_notes, total_wikilinks, broken_wikilinks, orphans, avg_outdegree, top_tags, notes_by_year}\n\nLimitations:\n- Aggregates other brain document tools; for detailed broken links use brain_broken_links directly",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "top_tags_limit": limit_schema(
                    "Max entries in top_tags (1-1000, default 10).", 10, 1, RESULT_LIMIT_MAX)
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
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
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
            "additionalProperties": false,
            "properties": {
                "limit": limit_schema(
                    "Max results per lint category (1-1000, default 50). Totals are always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
            }
        }
    })
}

fn tool_brain_memory_consolidate(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let apply = args.get("apply").and_then(|v| v.as_bool()).unwrap_or(false);
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
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
            "additionalProperties": false,
            "properties": {
                "apply": {
                    "type": "boolean",
                    "description": "Opt into write-mode: move files to their promoted destinations (default false = safe dry-run).",
                    "default": false
                },
                // The ONE parameter on the entire mutating surface with
                // unverified bounds (the other five mutators declare no numeric
                // parameter at all). Fixed schema-side, so no write path had to
                // be exercised to close it.
                "limit": limit_schema(
                    "Max proposals to return (1-1000, default 50). The total count is always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
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
            "additionalProperties": false,
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

/// The node kinds `brain_context` can filter on, as advertised in its schema.
/// Anything else silently matched no nodes and returned an empty context —
/// fail loudly instead (same policy as `validate_regex_kinds`). `Symbol`
/// sub-kinds (e.g. "Symbol/Function") stay valid because the filter is a
/// case-insensitive kind-PREFIX match.
const BRAIN_CONTEXT_KINDS: &[&str] = &["Symbol", "Note", "Section", "Tag", "Heading"];

fn validate_brain_context_kinds(kinds: &[String]) -> Result<(), anyhow::Error> {
    for kind in kinds {
        let is_base = BRAIN_CONTEXT_KINDS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(kind));
        let is_symbol_subkind = kind
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("symbol/"))
            && kind.len() > "symbol/".len();
        if !is_base && !is_symbol_subkind {
            return Err(anyhow!(
                "unknown kind '{kind}'; expected one of: {} (or a 'Symbol/<sub-kind>' prefix)",
                BRAIN_CONTEXT_KINDS.join(", ")
            ));
        }
    }
    Ok(())
}

// ── 1. brain_context ────────────────────────────────────────────────────────

fn tool_schema_code_context() -> Value {
    json!({
        "name": "code_context",
        "description": "Structural subgraph around seed SYMBOLS: personalized PageRank over the code graph alone. Returns the seeds plus the most relevant connected symbols, ranked.\n\nGuidelines:\n- Use when the question is about code structure — what surrounds this function, what is near this class\n- Seeds are symbol names or `sym:` UIDs\n\nLimitations:\n- CODE ONLY. It does not consider notes, tags, or wikilinks, and it does not resolve taxonomy aliases — use brain_context for the unified code+notes view\n- Relevance is PPR over the symbol graph, so scores are NOT comparable with brain_context's, which ranks over a different graph",
        "inputSchema": {
            "type": "object",
            "properties": {
                "seeds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Symbol names or `sym:` UIDs to seed the traversal."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    // A bound, because the handler asks the engine for
                    // `limit + 1` and an unbounded value overflows that: a
                    // debug panic, or a wrap to zero in release. The schema is
                    // not the only defence — the handler saturates too — but a
                    // knob with a minimum and no maximum is an omission either
                    // way.
                    "maximum": 5000,
                    "description": "Maximum connected symbols to return. Defaults to 500 when omitted; the response reports `connected_count` and `truncated` so an omitted limit is never silently lossy."
                },
                "intent": intent_schema(
                    "Tunes PPR damping and edge weights. Omit for the standard damping (0.85)."
                )
            },
            "required": ["seeds"],
            "additionalProperties": false
        }
    })
}

fn tool_schema_brain_context() -> Value {
    json!({
        "name": "brain_context",
        "description": "Retrieve PPR-ranked structural context from the knowledge graph, seeded by symbol names, note titles, or keywords. Returns mixed-kind results (Symbol, Note, Section, Tag, Heading) within a token budget.\n\nGuidelines:\n- Primary entry point for understanding a topic — use before reading files\n- Seed with specific names (e.g. 'AuthService.validate'), not broad terms\n- Filter with repos, tags, path_prefix, kinds for precision; use response_format 'concise' unless you need full bodies\n\nLimitations:\n- Only searches indexed repos/vaults — check stale_check if results seem stale\n- Ranked by graph proximity, not recency (use recency_weight to add time decay)\n- May fail with 'index publication TRANSIENT/WEDGED' while an index is being published. This refers to INDEX PUBLICATION, not a dirty git working tree: editing files in a repo does NOT cause it, and NestWeaver is fully usable while you work. TRANSIENT resolves on its own — retry. WEDGED means a prior indexer died mid-publication; ASK THE OPERATOR to run the `nestweaver repair` command named in the error — repair is a destructive publication recovery with no MCP tool, so it cannot be done from here — or check brain_status.index_publication.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "seeds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One or more seed strings to anchor the PPR walk. Accepts note titles, tag names (with or without #), symbol names, free-text terms, or UIDs (sym:/note:/head:/sec:/tag:)."
                },
                "token_budget": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 16000,
                    "description": "Approximate cap on the connected list (chars / 4, 1-16000). Default 2000. Increase for broader context, decrease for focused results.",
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
                    "description": "Include only nodes tagged with any of these tags (applies to Note and Section nodes; Symbol nodes are always kept). Matching is case-insensitive and includes NESTED descendants: \"project\" matches \"project/nestweaver\" but never \"projectile\"."
                },
                "exclude_tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exclude nodes tagged with any of these tags (applies to Note and Section nodes). Matching is case-insensitive and includes NESTED descendants: \"project\" matches \"project/nestweaver\" but never \"projectile\". An excluded parent therefore drops its whole subtree."
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
                    "description": "Semantic embedding weight for hybrid RRF fusion. Effective default is 0.0 — the semantic leg is skipped entirely (no BERT embed of the query) until embeddings are generated for the database (`nestweaver embed`); on embedded databases the default is 0.35."
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
                "intent": intent_schema(
                    "Optional query intent hint that adjusts ranking strategy. 'find-definition' boosts exact name matches; 'understand-architecture' broadens to structural neighbors; 'analyze-impact' (alias 'blast-radius') follows dependency edges; 'general-context' uses balanced defaults."
                ),
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

/// Truncate to `limit`, reporting whether anything was dropped.
///
/// The caller asks the engine for `limit + 1` so the extra row proves more rows
/// exist without paying for a second query; this drops it and says so. Silent
/// truncation is the failure mode being avoided: a caller that cannot tell a
/// complete answer from a capped one will treat the cap as the whole graph.
fn truncate_reporting<T>(items: &mut Vec<T>, limit: usize) -> bool {
    let truncated = items.len() > limit;
    if truncated {
        items.truncate(limit);
    }
    truncated
}

/// `code_context` — the CODE-only structural subgraph around seed symbols.
///
/// Distinct from `brain_context`, and the distinction is the whole point.
/// `brain_context` runs a HYBRID over code and notes with taxonomy-alias seed
/// resolution; this runs PPR over the symbol graph alone.
///
/// It exists because `nestweaver context` had no RPC of its own. Its daemon
/// route sent `brain_context`, while its direct path called
/// `build_context_with_intent` — so one command ran a different algorithm over
/// a different node set depending on whether a daemon happened to be running,
/// and the relevance numbers differed for the same query. The command's own
/// help says "structural subgraph around seed symbols", so the direct path was
/// the correct one and the daemon route was silently substituting a different
/// capability.
fn tool_code_context(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let seeds: Vec<String> = args
        .get("seeds")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if seeds.is_empty() {
        anyhow::bail!("code_context requires at least one seed");
    }
    // An omitted `limit` used to mean NO CAP, matching the engine's
    // `limit.unwrap_or(usize::MAX)`. That made the advertised 500-result
    // safeguard unreachable by simply not sending the field, so a seed in a
    // dense region serialized every connected symbol in the graph.
    //
    // It now defaults, and the CLI's direct path defaults to the SAME constant
    // — a cap on one route only is how the two drifted apart to begin with.
    // Truncation is disclosed rather than silent: `total` is the number of
    // connected symbols found, `connected_count` the number returned.
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(nestweaver_engine::CODE_CONTEXT_DEFAULT_LIMIT);
    let intent = args
        .get("intent")
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<nestweaver_store::ranking::QueryIntent>()
                .map_err(|error| anyhow::anyhow!("invalid intent: {error}"))
        })
        .transpose()?;

    // One over the cap, so `truncated` can be reported without a second pass:
    // the extra row proves more exist, and is dropped before rendering.
    // `saturating_add`, not `+`. Schema validation bounds `limit` on the MCP
    // path, but this function is also reached from routes that do not validate
    // against the schema, and `usize::MAX + 1` is a debug panic and a silent
    // wrap to ZERO in release — which would turn "give me everything" into
    // "give me nothing".
    let mut result = nestweaver_engine::build_context_with_intent(
        store,
        &seeds,
        intent,
        Some(limit.saturating_add(1)),
    )?;
    let truncated = truncate_reporting(&mut result.connected, limit);

    let render = |node: &nestweaver_engine::ContextNode| {
        json!({
            "uid": node.uid,
            "name": node.name,
            "kind": node.kind,
            "file_path": node.file_path,
            "start_line": node.start_line,
            "signature": node.signature,
            "relevance": node.relevance,
        })
    };
    // nw-320. `connected_count` reports what was RETURNED, so it agrees with
    // the item list by construction and a capped answer looked complete. The
    // engine knows how many MATCHED — it stopped pushing at the cap and threw
    // the number away — and now reports it. `total`/`returned` are the
    // spellings 8.0.0 corrected `brain_impact` and `brain_search` to; a `total`
    // that counts survivors is not a total of anything.
    let returned = result.connected.len();
    let total = result.connected_total.unwrap_or(returned).max(returned);
    let payload = json!({
        "seeds": result.seeds.iter().map(render).collect::<Vec<_>>(),
        "connected": result.connected.iter().map(render).collect::<Vec<_>>(),
        "cross_repo_links": serde_json::to_value(&result.cross_repo_links)?,
        "seeds_resolved": result.seeds.len(),
        // Retained as the returned count, which is what it has always meant
        // and what `merge_json_results` recomputes after a federated cap.
        // `total` is the field that was missing.
        "connected_count": returned,
        "returned": returned,
        "total": total,
        "limit": limit,
        "truncated": truncated || total > returned,
    });
    Ok(payload)
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
    if let Some(ref kinds) = filter_kinds {
        validate_brain_context_kinds(kinds)?;
    }
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
    // nw-295. Validated and NORMALISED at the boundary, once. The value used
    // to go straight into `WHERE n.modified_at >= $since`, which is a
    // LEXICOGRAPHIC comparison against a String column and therefore can never
    // fail — so `since: "garbage"` was byte-identical to `since: "2099-12-31"`:
    // both matched no note and silently dropped every Note and Section from
    // the answer. The `.filter(|s| !s.is_empty())` matters too: the CLI's
    // daemon route sends `""` for an absent `--since`, and it survives today
    // only because the daemon strips empty strings before dispatch.
    if let Some(since) = args
        .get("since")
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
    {
        let since = nestweaver_engine::parse_since(since).map_err(|e| anyhow!("{e}"))?;
        let recent_notes = store
            .list_note_uids_modified_since(&since)
            .map_err(|e| anyhow!("list_note_uids_modified_since: {e}"))?;
        let recent_sections = store
            .list_section_uids_modified_since(&since)
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
        "semantic_applied": result.semantic_applied,
        "degraded_components": &result.degraded_components,
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

    // nw-102: how many of `seeds` are semantic nearest-neighbour guesses rather
    // than resolutions of the query. Without this the daemon path could not
    // distinguish them and reported every guess as "resolved" — the same
    // response then claimed a seed both resolved AND unresolved.
    if result.semantic_seed_count > 0 {
        resp["semantic_seed_count"] = json!(result.semantic_seed_count);
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

/// Estimated token cost of rendering one node, in the shape the renderer will
/// actually emit.
///
/// `pub` because `src/main.rs` had a COPY of this — `render_cost_tokens` —
/// which was the `concise == false` branch unconditionally, while
/// `project-context` defaults to concise. It therefore charged roughly
/// `(uid.len() + 40) / 4` tokens per node more than the renderer would spend
/// and took fewer nodes for the same budget (nw-316). One function is the only
/// arrangement in which the estimate and the renderer cannot disagree.
pub fn render_cost(n: &nestweaver_engine::BrainNode, concise: bool) -> usize {
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
        "description": "Find notes, headings, sections, tags, and code symbols by keyword or phrase using BM25 full-text search.\n\nGuidelines:\n- Use for keyword/phrase lookup; for structural context ('what's connected to X') use brain_context instead\n- Returns both notes and code symbols in a single call, with UIDs for follow-up queries; note rows also carry vault_uid and matched_headings (matched_headings is omitted when empty)\n- Use response_format 'concise' for scanning many results; limit is applied per-kind\n- total_matches counts distinct note/tag and symbol entities independently of the display limit; total_matches_relation 'gte' marks a stable lower bound from bounded counting\n- returned_matches is the actual response length, and truncated is true for every lower bound or when fewer rows are returned than total_matches\n- semantic_applied is always false and degraded_components always empty: this tool is keyword/BM25-only and never runs a semantic leg, so ranking is lexical. The fields are reported rather than omitted so their absence is never mistaken for an older server\n\nLimitations:\n- Does not read full note bodies — use note_get after finding the note here\n- Falls back to substring matching when the Tantivy BM25 index is unavailable\n- Keyword matching only — it will not find conceptually related wording that shares no terms with the query",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Free-text query — natural language works. Example: \"database migration\" or \"AuthService\"."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results PER KIND, not in total. Defaults to the configured `[limits] default_result_limit`, or 20 when none is set. Notes and code symbols are capped separately, so a query matching both can return up to 2x this value — budget accordingly and read returned_matches for the true count. Set lower for focused lookups, higher for broad discovery.",
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
                },
                "cache": { "type": "string", "description": "Set to \"bypass\" to skip the response cache for this call." },
                "no_cache": { "type": "boolean", "description": "When true, skip the response cache for this call." }
            },
            "required": ["query"],
            "additionalProperties": false
        }
    })
}

/// The node kind behind a grouped brain_search row.
///
/// nw-169: this was hardcoded `"note"` for every row, but the grouped results
/// also contain TAG nodes (uid `tag:<vault>:<hash>`, minted by
/// nestweaver_schema::uid). On this vault `brain_search{query:"Home"}` returned
/// two tag rows scoring 44.05 and 39.56 above the best real note at 28.9,
/// labelled `"note"`, carrying no location, and unfetchable — following the
/// tool's own documented workflow with `note_get{uid}` failed. A client had no
/// field to filter them on. Labelling them honestly is the fix; they remain
/// legitimate hits.
fn grouped_row_kind(uid: &str) -> &'static str {
    if uid.starts_with("tag:") {
        "tag"
    } else {
        "note"
    }
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
        // Honour `[limits] default_result_limit` when the operator set one.
        // This hardcoded 20 while every paginated sibling consulted the config,
        // so a configured limit was silently ignored by the one tool most
        // likely to be tuned. 20 stays the DOCUMENTED default when nothing is
        // configured, matching this tool's own schema.
        .unwrap_or_else(|| configured_result_limit_or(20))
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
            if (hit.kind == "heading" || hit.kind == "section")
                && !group.matched_headings.contains(&hit.title)
            {
                // A heading and its section share a title — report it once.
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
        // Parity with the Tantivy path: detailed note rows carry their vault
        // (resolved from the note list already fetched above).
        let note_sources: HashMap<&str, (&str, &str)> = notes
            .iter()
            .map(|n| (n.uid.as_str(), (n.vault_uid.as_str(), n.file_path.as_str())))
            .collect();
        let rows: Vec<Value> = note_order
            .iter()
            .take(limit)
            .filter_map(|nuid| groups.get(nuid))
            .map(|g| {
                let mut row = if concise {
                    json!({
                        "uid": g.note_uid,
                        "kind": grouped_row_kind(&g.note_uid),
                        "title": g.best_title,
                    })
                } else {
                    json!({
                        "uid": g.note_uid,
                        "kind": grouped_row_kind(&g.note_uid),
                        "title": g.best_title,
                        "score": g.best_score,
                    })
                };
                if let Some((vault_uid, file_path)) = note_sources.get(g.note_uid.as_str()).copied()
                {
                    if !vault_uid.is_empty() {
                        row["vault_uid"] = json!(vault_uid);
                    }
                    if !file_path.is_empty() {
                        row["location"] = json!(file_path);
                    }
                }
                if !g.matched_headings.is_empty() {
                    row["matched_headings"] = json!(g.matched_headings);
                }
                row
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
        // Dropping a candidate whose symbol cannot be read is FAIL-CLOSED, and
        // for an authorization filter that is the right default — including a
        // hit we cannot attribute to a repo would leak it. The behaviour stays.
        //
        // What was wrong is that it was SILENT: a store error and a genuine
        // authz exclusion were indistinguishable, so a scoped search could
        // return fewer results (or none) with nothing anywhere saying a read
        // had failed. `authorized_symbol_total` accounts for the filtering, not
        // for the failure.
        code_hits.retain(|candidate| match store.lookup_symbol(&candidate.uid) {
            Ok(symbol) => !symbol.repo_uid.trim().is_empty() && repos.contains(&symbol.repo_uid),
            Err(error) => {
                tracing::warn!(
                    uid = %candidate.uid,
                    "brain_search: dropping candidate whose symbol could not be read \
                     during repo-scope filtering: {error}"
                );
                false
            }
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
        // `brain_search` is keyword/BM25-only; it must not claim a
        // semantic leg was requested or degraded. Emitted unconditionally
        // (and on both the bm25 and substring branches, which converge
        // here) so callers can tell "no semantic leg" apart from "field
        // not implemented on this path".
        //
        // Other sites that must stay in agreement when a semantic leg is
        // added. This list is not a claim that they currently agree:
        //   - the gRPC daemon's `brain_search` handler (honest today)
        //   - `daemon_brain_search_response_to_json` below, which forwards
        //     the proto fields verbatim (honest today)
        //   - `nestweaver-federation/src/results.rs::merge_json_results`,
        //     the hybrid/federated merge. It rebuilds the response from
        //     `wrap_merged_response`, so every field has to be re-added
        //     deliberately; it merges these two via `merge_honesty_fields`
        //     (AND for `semantic_applied`, dedup union for
        //     `degraded_components`). Honest today.
        //   - the STRUCTURED branch of `merge_structured_results` (the
        //     `connected`-schema path used by brain_context /
        //     project_context), which rebuilds its envelope the same way and
        //     now calls the SAME `merge_honesty_fields` with the same rules.
        //     Honest today. It matters more there than here: those tools have
        //     a real semantic leg, so the fields are not trivially
        //     `false`/`[]`, and the mixed case (one tier ranked semantically,
        //     the other lexically) is reachable — the AND rule reports `false`
        //     for it, which is correct but coarser than the two-tier truth.
        //     See `merge_honesty_fields` for why that is not encoded in
        //     `degraded_components`.
        //
        // None of this interacts with response caching:
        // `semantic_response_is_degraded` (above) has exactly two call sites,
        // both inside `maybe_cached`, reached only from `dispatch_cancellable`
        // — the local in-process dispatch, strictly upstream of any federated
        // merge. `hybrid.rs` has no response cache and never writes merged
        // output back, so nothing downstream of the merge reads these values.
        "semantic_applied": false,
        "degraded_components": [],
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
        if (h.kind == "heading" || h.kind == "section")
            && !group.matched_headings.contains(&h.title)
        {
            // A heading and its section share a title — report it once.
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
            let mut row = if concise {
                json!({
                    "uid": g.note_uid,
                    "kind": grouped_row_kind(&g.note_uid),
                    "title": g.best_title,
                })
            } else {
                json!({
                    "uid": g.note_uid,
                    "kind": grouped_row_kind(&g.note_uid),
                    "title": g.best_title,
                    "score": g.best_score,
                })
            };
            if !g.file_path.is_empty() {
                row["location"] = json!(g.file_path);
            }
            if !g.vault_uid.is_empty() {
                row["vault_uid"] = json!(g.vault_uid);
            }
            if !g.matched_headings.is_empty() {
                row["matched_headings"] = json!(g.matched_headings);
            }
            row
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
            frontmatter_raw: None,
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

    /// Both contract surfaces must report a repo whose derivation failed as
    /// degraded. `contract_drift`'s `clean` field is the one that actively lied:
    /// a degraded repo has zero declared and zero implemented contracts, so the
    /// old `dni_total == 0 && ind_total == 0` asserted `clean: true` about a
    /// repo with no contract graph at all.
    #[test]
    fn degraded_repo_is_not_reported_clean_by_either_contract_tool() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:broken".to_string(),
                url: "https://example.test/broken".to_string(),
                indexed_sha: "broken-sha".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test-instance".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .set_contract_derivation_failed("repo:broken", "COPY Contract: duplicate primary key")
            .unwrap();

        let drift = tool_contract_drift(&store, json!({})).unwrap();
        assert_eq!(drift["clean"], json!(false), "drift envelope: {drift}");
        assert_eq!(drift["contracts_status"], json!("degraded"));
        assert_eq!(drift["degraded_repos"], json!(["repo:broken"]));

        let cross = tool_cross_repo_contracts(&store, json!({ "uid": "sym:absent" })).unwrap();
        assert_eq!(cross["contracts_status"], json!("degraded"), "{cross}");
        assert_eq!(cross["degraded_repos"], json!(["repo:broken"]));
    }

    /// The distinction has to be a distinction: an empty graph with no
    /// derivation failure is still clean and still complete.
    #[test]
    fn empty_graph_is_reported_clean_by_either_contract_tool() {
        let store = GraphStore::in_memory().unwrap();

        let drift = tool_contract_drift(&store, json!({})).unwrap();
        assert_eq!(drift["clean"], json!(true), "drift envelope: {drift}");
        assert_eq!(drift["contracts_status"], json!("complete"));
        assert_eq!(drift["degraded_repos"], json!([]));

        let cross = tool_cross_repo_contracts(&store, json!({ "uid": "sym:absent" })).unwrap();
        assert_eq!(cross["contracts_status"], json!("complete"), "{cross}");
        assert_eq!(cross["degraded_repos"], json!([]));
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

        // nw-101: the `limit` property itself must disclose that the cap is
        // per-kind. The tool description said so while this said "Maximum
        // results to return", and a caller reading the field they are actually
        // setting was told a total cap that does not exist — `--limit 3` on a
        // mixed query returns 6. The per-kind cap is deliberate (symbols carry
        // a fixed 0.5 score against BM25 notes scoring 15+, so a merged cap
        // would evict every symbol), so the contract is what needs correcting.
        let limit_doc = schema["inputSchema"]["properties"]["limit"]["description"]
            .as_str()
            .unwrap();
        assert!(
            limit_doc.contains("PER KIND"),
            "the limit field must state it is per-kind, not a total: {limit_doc}"
        );
        assert!(
            limit_doc.contains("2x"),
            "it must state the consequence — up to 2x limit rows: {limit_doc}"
        );
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

    #[test]
    fn group_search_hits_dedups_shared_heading_section_titles() {
        // A heading hit and its section hit share a title; matched_headings
        // must report it once, not twice.
        let store = GraphStore::in_memory().unwrap();
        let hit = |uid: &str, kind: &str, title: &str, score: f32| nestweaver_store::SearchHit {
            uid: uid.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            vault_uid: "vlt:v".to_string(),
            note_uid: "note:n1".to_string(),
            score,
        };
        let hits = vec![
            hit("head:1", "heading", "Shared", 1.0),
            hit("sec:1", "section", "Shared", 0.9),
            hit("head:2", "heading", "Other", 0.8),
        ];
        let grouped =
            group_search_hits_by_note(&store, &hits, SearchTotal::exact(1), 10, false).unwrap();
        assert_eq!(grouped.rows.len(), 1);
        assert_eq!(
            grouped.rows[0]["matched_headings"],
            json!(["Shared", "Other"])
        );
    }

    #[test]
    fn local_brain_search_omits_empty_optional_arrays_like_daemon_route() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&note("note:title-only", "Titleonlyneedle"))
            .unwrap();
        let hit = nestweaver_store::SearchHit {
            uid: "note:title-only".to_string(),
            kind: "note".to_string(),
            title: "Titleonlyneedle".to_string(),
            vault_uid: "vlt:test".to_string(),
            note_uid: "note:title-only".to_string(),
            score: 1.0,
        };

        for concise in [false, true] {
            let grouped = group_search_hits_by_note(
                &store,
                std::slice::from_ref(&hit),
                SearchTotal::exact(1),
                10,
                concise,
            )
            .unwrap();
            assert!(grouped.rows[0].get("matched_headings").is_none());
            assert_eq!(grouped.rows[0]["vault_uid"], "vlt:test");
        }
    }

    #[test]
    fn substring_search_dedups_shared_heading_section_titles() {
        // Same dedup on the no-tantivy substring fallback path: the heading
        // and its section both match the query and share the heading title.
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&note("note:dup", "Unrelated Title"))
            .unwrap();
        store
            .insert_heading(&heading("heading:dup", "note:dup", "Dedupneedle Alpha"))
            .unwrap();
        store
            .insert_section(&section(
                "section:dup",
                "note:dup",
                "heading:dup",
                "dedupneedle body text",
            ))
            .unwrap();
        let value = tool_brain_search(
            &store,
            None,
            json!({ "query": "dedupneedle", "response_format": "detailed" }),
            None,
        )
        .unwrap();
        let rows = value["results"].as_array().unwrap();
        let note_row = rows
            .iter()
            .find(|r| r["uid"] == "note:dup")
            .expect("note row present");
        assert_eq!(note_row["matched_headings"], json!(["Dedupneedle Alpha"]));
    }

    /// Both MCP `brain_search` response paths — the daemon-routed conversion
    /// and the in-process tool — must agree field-for-field on the
    /// semantic-honesty keys. An absent field is indistinguishable from an
    /// unsupported one, so both emit them unconditionally.
    ///
    /// Scope limit, deliberate: the gRPC daemon handler is the third emitter,
    /// but it lives in `nestweaver-daemon`, which this crate does not depend
    /// on, so its constants are MIRRORED below rather than observed. Editing
    /// the handler to set `semantic_applied: true` would NOT fail this test.
    /// Real cross-process coverage belongs in the root package's
    /// `tests/daemon_test.rs`, which links both crates and already stands up
    /// live daemons. What this test does pin is the proto struct literal:
    /// it is exhaustive, so any new `BrainSearchResponse` field breaks
    /// compilation here and forces a decision about honesty reporting.
    #[cfg(feature = "daemon")]
    #[test]
    fn brain_search_semantic_honesty_fields_agree_across_mcp_paths() {
        // 1. gRPC daemon constants, mirrored (see the scope limit above).
        let grpc = nestweaver_proto::BrainSearchResponse {
            query: "honestyneedle".to_string(),
            engine: "bm25".to_string(),
            total_matches: 0,
            results: Vec::new(),
            expansion_terms: Vec::new(),
            returned_matches: 0,
            total_matches_relation: "eq".to_string(),
            truncated: false,
            semantic_applied: false,
            degraded_components: Vec::new(),
        };

        // 2. Daemon-routed MCP: forwards the proto fields verbatim.
        let daemon_json = daemon_brain_search_response_to_json(&grpc, false);

        // 3. In-process MCP, both branches. No tantivy index → substring
        //    fallback; with one → bm25. Both converge on one response literal,
        //    but assert each so a future split cannot silently drop a field.
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&note("note:honesty", "Honestyneedle Title"))
            .unwrap();
        let substring_json = tool_brain_search(
            &store,
            None,
            json!({ "query": "honestyneedle", "response_format": "detailed" }),
            None,
        )
        .unwrap();
        assert_eq!(substring_json["engine"], "substring");

        let dir = tempfile::tempdir().unwrap();
        let index = TantivyIndex::open_or_create(dir.path()).unwrap();
        let bm25_json = tool_brain_search(
            &store,
            Some(&index),
            json!({ "query": "honestyneedle", "response_format": "detailed" }),
            None,
        )
        .unwrap();
        assert_eq!(bm25_json["engine"], "bm25");

        for (label, value) in [
            ("daemon-routed MCP", &daemon_json),
            ("in-process MCP (substring)", &substring_json),
            ("in-process MCP (bm25)", &bm25_json),
        ] {
            assert_eq!(
                value.get("semantic_applied"),
                Some(&json!(grpc.semantic_applied)),
                "{label} disagrees with the mirrored gRPC constants on `semantic_applied`"
            );
            assert_eq!(
                value.get("degraded_components"),
                Some(&json!(grpc.degraded_components)),
                "{label} disagrees with the mirrored gRPC constants on `degraded_components`"
            );
        }
    }
}

fn tool_schema_note_get() -> Value {
    json!({
        "name": "note_get",
        "description": "Fetch a vault note's full markdown body or specific sections, plus structural metadata (frontmatter, heading outline, tags).\n\nRequires either 'uid' or 'title' (at least one must be provided).\n\nGuidelines:\n- Use after brain_search or brain_context identifies a relevant note\n- Pass uid for unambiguous lookup, or title for case-insensitive first-match\n- Use sections parameter to retrieve only specific heading sections — much more token-efficient for large notes\n\nLimitations:\n- Markdown notes only — for code symbols use read_symbols\n- Not a discovery tool — use brain_search or brain_context to find notes first",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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
        match resolve_note_by_title(store, title)? {
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
            "additionalProperties": false,
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

/// Resolve a note by title, tolerating case, slug form, and FILENAME STEM.
///
/// nw-168: `note_get`/`backlinks` matched only the H1 title (plus a
/// case/slug fallback), while the wikilink resolver's priority-3b tier also
/// matches the filename stem. So `~/brain/Home.md`, whose H1 is
/// "Brain - Command Center", could be linked as `[[Home]]` by 26 notes and
/// resolved every time, yet `note_get{title:"Home"}` reported "no note found".
/// The filename is often the only handle a user has.
///
/// Stem matching runs LAST, after exact and slug matches, so a real title
/// always wins over a coincidental filename.
fn resolve_note_by_title(
    store: &GraphStore,
    title: &str,
) -> Result<Option<nestweaver_schema::Note>, anyhow::Error> {
    let mut matches = store
        .lookup_notes_by_title(title)
        .with_context(|| format!("failed to look up notes with title '{title}'"))?;
    if let Some(note) = matches.drain(..).next() {
        return Ok(Some(note));
    }

    // Uses list_notes_lite to avoid loading full note bodies during the scan.
    //
    // Propagated, not swallowed. `let Ok(..) else { return Ok(None) }` turned a
    // failed scan into "no such note", so a store error reached the caller as a
    // proven absence — and the caller renders that as
    // "no note found with title '<title>'", which is a claim about the VAULT
    // made on the strength of a failure to read it.
    let all_notes = store
        .list_notes_lite(None)
        .with_context(|| format!("scan notes while resolving title '{title}'"))?;
    let needle = title.to_lowercase();
    let wanted_slug = slug_normalize(title);
    let stem_of = |path: &str| {
        std::path::Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
    };
    let hit = all_notes
        .iter()
        .find(|n| n.title.to_lowercase() == needle || slug_normalize(&n.title) == wanted_slug)
        .or_else(|| {
            all_notes.iter().find(|n| {
                stem_of(&n.file_path)
                    .is_some_and(|stem| stem == needle || slug_normalize(&stem) == wanted_slug)
            })
        });
    match hit {
        // `.ok()` here was the sharpest form of the same defect: the title
        // MATCHED a row, and then a failed hydration of that row became `None`
        // — "no note found with that title" about a note we had just found.
        // The uid branch at the top of this function already propagates with
        // `with_context(...)?`; same function, opposite handling.
        Some(hit) => store
            .lookup_note(&hit.uid)
            .map(Some)
            .with_context(|| format!("hydrate note '{}' matched by title", hit.uid)),
        None => Ok(None),
    }
}

fn tool_backlinks(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let target_uid = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        uid.to_string()
    } else if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        match resolve_note_by_title(store, title)? {
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
        "description": "Show what knowledge sources are indexed: vault/repo counts, note/tag/wikilink totals, staleness warnings, and search engine availability. No parameters required.\n\nGuidelines:\n- Call at session start to verify expected vaults and repos are loaded\n- Surfaces staleness warnings when repos are behind git HEAD\n- If counts are zero, use brain_add_source to index content — but a count of `null` means it could NOT BE READ, which is NOT zero and is not a reason to re-index. Check `unavailable` (and `counts_complete`) before acting on any count\n\nLimitations:\n- Metadata-only — does not search content (use brain_search for that)\n- For detailed per-repo staleness, use stale_check\n\nServer-mode and daemon-runtime fields (server_mode, indexing_active, indexing_repo, queue_depth, write_queue_depth, write_holder, write_holder_seconds, embedding_status) are ALWAYS present in the document. `write_queue_depth` counts write RPCs blocked on the daemon write lock — a different population from `queue_depth`, which counts index jobs. Inside `embedding_status`, `pass_active` / `pass_processed` / `pass_total` / `pass_started_at` / `pass_scope` describe an in-flight embedding pass. While a pass runs, `state` reads `embedding` rather than `ready` — a strictly narrower `ready`, so the daemon can still answer semantic queries; prefer the boolean `pass_active` over matching the state string. `pass_total` is 0 until the eligibility preflight finishes, which means \"not yet counted\", not \"nothing to do\". Only a live daemon can answer the daemon-owned fields honestly (`server_mode` is the exception — a bool that is simply `false` off-daemon): they carry live values on the daemon's gRPC surface and explicit nulls when no daemon serves the answer (direct `--no-daemon`, MCP-over-HTTP, in-process MCP — nothing was bypassed there, so `degraded_components` stays empty). The CLI's daemon-bypassed fallback additionally marks the nulls via `degraded_components: [\"daemon_runtime\"]` and a `daemon_bypassed` warning.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }
    })
}

fn tool_brain_status(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
) -> Result<Value, anyhow::Error> {
    brain_status_json(store, tantivy)
}

/// The ONE `brain_status` document builder, shared by every serving path:
/// the daemon's gRPC surface (which overwrites the daemon-runtime fields with
/// live values), the in-process MCP server, and the CLI's direct
/// (daemon-bypassed) fallback, which marks the result with
/// [`mark_brain_status_daemon_bypassed`].
///
/// Fields only a live daemon can answer honestly — see
/// [`DAEMON_RUNTIME_STATUS_FIELDS`] — are ALWAYS present, so a
/// `--json 2>/dev/null` consumer can never silently receive a different
/// schema on the direct path. The seven daemon-owned runtime fields are
/// explicit nulls here (the daemon's gRPC handler overwrites them with live
/// values); the two tantivy fields are derived from the builder's own
/// `tantivy` argument, and the CLI's direct fallback re-nulls even those via
/// [`mark_brain_status_daemon_bypassed`] — a process without the daemon's
/// index open cannot claim `false`/`0` honestly. `degraded_components`
/// follows the `brain_search` precedent: always present, empty unless a
/// component was bypassed.
pub fn brain_status_json(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
) -> Result<Value, anyhow::Error> {
    let vaults = store
        .list_vaults(None)
        .map_err(|e| anyhow::anyhow!("brain_status: failed to list vaults: {e}"))?;
    let repos = store
        .list_repos(None)
        .map_err(|e| anyhow::anyhow!("brain_status: failed to list repos: {e}"))?;
    // A count that could not be READ is not a count of zero.
    //
    // These were `unwrap_or(0)`, which is CWE-390 — "Detection of Error
    // Condition Without Action" — and aggravated here because `0` is
    // semantically loaded: this tool's own description tells the caller "if
    // counts are zero, use brain_add_source to index content". A single failed
    // query therefore advised re-indexing a healthy vault.
    //
    // Note the asymmetry that gave it away: `list_vaults` and `list_repos`
    // eight lines up already `map_err` the identical error class.
    //
    // Null, plus an `unavailable` list naming what could not be read, follows
    // Google AIP-217: never return incomplete data silently — enumerate what
    // is missing, or fail. Nothing here makes the whole answer meaningless
    // (vaults and repos still list), so it degrades and says so rather than
    // failing outright.
    let mut unavailable: Vec<&'static str> = Vec::new();
    let mut count_or_null =
        |label: &'static str, result: Result<usize, nestweaver_store::StoreError>| -> Value {
            match result {
                Ok(count) => json!(count),
                Err(error) => {
                    tracing::warn!("brain_status: {label} count unavailable: {error}");
                    unavailable.push(label);
                    Value::Null
                }
            }
        };
    let notes = count_or_null("notes", store.count_notes());
    let headings = count_or_null("headings", store.count_headings());
    let sections = count_or_null("sections", store.count_sections());
    let tags = count_or_null("tags", store.count_tags());
    let wikilinks = count_or_null("wikilinks", store.count_wikilink_edges());

    let db_path = match current_db_path(store) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(
                "brain_status: db_path unavailable ({e}), extension store lookups will be skipped"
            );
            None
        }
    };

    // Counted rather than named per vault, because `unavailable` is a list of
    // stable labels and a vault name is neither stable nor bounded.
    let mut vault_count_failures = 0usize;
    let vaults_json: Vec<Value> = vaults
        .iter()
        .map(|v| {
            // Same defect as the totals above, and it survived the fix that
            // wrote that comment: `unwrap_or_default()` turns a failed read
            // into a vault holding zero notes. The per-vault number is the one
            // a caller actually looks at when deciding whether a vault indexed
            // correctly, so a confident zero here is the most misleading of
            // the set.
            let (notes, note_count) = match store.list_notes(Some(&v.uid)) {
                Ok(notes) => {
                    let count = notes.len();
                    (notes, json!(count))
                }
                Err(error) => {
                    tracing::warn!(
                        vault = %v.uid,
                        "brain_status: per-vault note count unavailable: {error}"
                    );
                    vault_count_failures += 1;
                    (Vec::new(), Value::Null)
                }
            };
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

    // nw-C2: index-publication state, read ONCE from the store's own path
    // and shared between the warnings builder and the `index_publication`
    // block below, so the two can never disagree about which database they
    // describe (thread-local vs store path) or race a marker cleared between
    // two reads. Unlike the daemon-runtime fields (`write_queue_depth`,
    // `write_holder`, the embedding `pass_*` set), which only a live daemon
    // answers honestly — explicit nulls here, live values on the daemon's
    // gRPC surface — this is derived from the marker FILE and so populates
    // with no daemon running. The user who reported the wedge was on exactly
    // that direct path.
    let publication_status = store
        .db_path()
        .map(nestweaver_engine::index_publication::status);
    // Structured warnings (duplicate-root collisions, wedged index
    // publication) come from the shared builder so the CLI's direct
    // (`--no-daemon`) path forwards the SAME array instead of re-deriving a
    // subset locally.
    let warnings = brain_status_warnings_for(store, store.db_path(), publication_status.as_ref());
    let repos_json: Vec<Value> = repos
        .iter()
        .map(|r| json!({ "url": r.url, "sha": r.indexed_sha }))
        .collect();

    // Every instance id present in this database, sorted for a stable
    // document. Previously a direct-path-only key; the daemon path gains it
    // additively so both serve the same top-level schema.
    let mut instance_ids: std::collections::BTreeSet<&str> =
        vaults.iter().map(|v| v.instance_id.as_str()).collect();
    instance_ids.extend(repos.iter().map(|r| r.instance_id.as_str()));
    let instance_ids_json: Vec<String> = instance_ids.into_iter().map(str::to_string).collect();

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

    // The `index_publication` payload reuses the single status read taken
    // above; the matching `index_publication_wedged` warning is pushed by
    // `brain_status_warnings_for` from that same read.
    let index_publication = publication_status.as_ref().map(|status| {
        json!({
            "dirty": status.dirty,
            "determinable": status.determinable,
            "marker_age_s": status.marker_age_s,
            "writer_pid": status.writer_pid,
            "writer_alive": status.writer_alive,
            "writer_reason": status.writer_reason,
            "wedged": status.is_wedged(),
            "marker_path": status.marker_path,
        })
    });

    // Fold the per-vault failures into the same disclosure the totals use, so
    // a caller has ONE place to look for "what could not be read".
    if vault_count_failures > 0 {
        unavailable.push("per-vault note counts");
    }

    Ok(json!({
        // `db` and `instance_ids` were direct-path-only keys; the daemon
        // path gains them additively so both paths serve one schema.
        "db": db_path.as_ref().map(|p| p.display().to_string()),
        "instance_ids": instance_ids_json,
        "vaults": vaults_json,
        // Deprecated alias of `vaults`, kept through the deprecation window
        // for scripts written against the old local-only shape.
        "vault_details": vaults_json,
        "vault_count": vaults.len(),
        "notes": notes,
        "headings": headings,
        "sections": sections,
        "tags": tags,
        "wikilinks": wikilinks,
        // AIP-217. Empty on a healthy brain; a subsystem named here means its
        // count is `null` because it could not be READ — not that it is zero.
        // A caller must not act on a null the way it would act on a 0.
        "unavailable": unavailable,
        // `counts_complete` must account for the PER-VAULT counts too, or it
        // claims completeness for a payload that carries nulls.
        "counts_complete": unavailable.is_empty() && vault_count_failures == 0,
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
        // and should be measured in real usage. A serving daemon adds its
        // `requests_served` witness counter to this block.
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
        // File-derived index-publication state; present on the direct
        // `--no-daemon` path too. `null` for in-memory stores, which have no
        // marker. `dirty` means an index PUBLICATION is in flight or was
        // abandoned — it has nothing to do with a dirty git working tree.
        "index_publication": index_publication,
        // Daemon-runtime fields (see DAEMON_RUNTIME_STATUS_FIELDS). Only a
        // live daemon can answer these honestly, so they are explicit nulls
        // here; the daemon's gRPC handler overwrites every one with live
        // values, and the CLI's direct fallback re-nulls the two tantivy
        // fields (a process without the daemon's index open cannot claim
        // `false`/`0` honestly) and marks the document degraded.
        "embedding_status": Value::Null,
        "indexing_active": Value::Null,
        "indexing_repo": Value::Null,
        "queue_depth": Value::Null,
        "write_queue_depth": Value::Null,
        "write_holder": Value::Null,
        "write_holder_seconds": Value::Null,
        // The `brain_search` precedent: always present, empty unless a
        // component was bypassed. The direct fallback sets
        // ["daemon_runtime"] via `mark_brain_status_daemon_bypassed`.
        "degraded_components": Vec::<String>::new(),
    }))
}

/// Daemon-runtime fields of the `brain_status` document that only a live
/// daemon can answer honestly. They are ALWAYS present in the document —
/// live values when a daemon serves it, explicit nulls on the direct
/// (daemon-bypassed) path — so a `--json 2>/dev/null` consumer can never
/// silently receive a different schema.
pub const DAEMON_RUNTIME_STATUS_FIELDS: &[&str] = &[
    "embedding_status",
    "indexing_active",
    "indexing_repo",
    "queue_depth",
    "write_queue_depth",
    "write_holder",
    "write_holder_seconds",
    "tantivy_available",
    "tantivy_doc_count",
];

/// The `degraded_components` marker for a `brain_status` answer that carries
/// no daemon-runtime state (the `brain_search` precedent).
pub const DAEMON_RUNTIME_DEGRADED: &str = "daemon_runtime";

/// Mark a [`brain_status_json`] document as answered by the direct,
/// daemon-bypassed read-only path: every [`DAEMON_RUNTIME_STATUS_FIELDS`]
/// entry becomes an explicit null (a process without the daemon's runtime
/// cannot even claim `false`/`0` honestly), and the degradation is disclosed
/// in THREE places so no consumer misses it — top-level
/// `degraded_components`, a synthesized `_meta` mirroring it, and a
/// `daemon_bypassed` entry in `warnings` carrying the bypass cause.
pub fn mark_brain_status_daemon_bypassed(value: &mut Value, cause: &str) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for field in DAEMON_RUNTIME_STATUS_FIELDS {
        obj.insert((*field).to_string(), Value::Null);
    }
    obj.insert(
        "degraded_components".to_string(),
        json!([DAEMON_RUNTIME_DEGRADED]),
    );
    // nw-315: built through the one provenance author so this cannot become a
    // sixth spelling of the same three keys; the `degraded_components` leg is
    // this path's own addition and is merged on top.
    let mut meta = nestweaver_schema::provenance::provenance("direct", &["direct"], &[]);
    if let Some(meta_obj) = meta.as_object_mut() {
        meta_obj.insert(
            "degraded_components".to_string(),
            json!([DAEMON_RUNTIME_DEGRADED]),
        );
    }
    obj.insert(nestweaver_schema::provenance::META_KEY.to_string(), meta);
    if let Some(warnings) = obj
        .entry("warnings".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
    {
        warnings.push(json!({
            "kind": "daemon_bypassed",
            "warning": "answered by the read-only direct path; daemon-runtime fields \
                        (embedding_status, write/index queue, tantivy stats) are null",
            "cause": cause,
        }));
    }
}

/// Build the `warnings` array for `brain_status` from the store's current
/// state: duplicate-vault-root collisions and a wedged index publication.
///
/// Shared by `tool_brain_status` and the CLI's direct (`--no-daemon`) text
/// path so both forward the SAME warnings — the direct path previously
/// re-derived only the duplicate-root subset locally and could never report
/// a wedged publication at all. Every entry carries a `kind` so renderers
/// can special-case a shape and still forward the rest generically.
pub fn brain_status_warnings(store: &GraphStore, db_path: Option<&std::path::Path>) -> Vec<Value> {
    let db_path = db_path.or_else(|| store.db_path());
    let publication = db_path.map(nestweaver_engine::index_publication::status);
    brain_status_warnings_for(store, db_path, publication.as_ref())
}

/// Core of [`brain_status_warnings`] with the index-publication status
/// already read, so `tool_brain_status` can share ONE marker read — against
/// ONE db path — between the wedge warning and its `index_publication`
/// block. `db_path` and `publication` must describe the same database.
fn brain_status_warnings_for(
    store: &GraphStore,
    db_path: Option<&std::path::Path>,
    publication: Option<&nestweaver_engine::index_publication::IndexPublicationStatus>,
) -> Vec<Value> {
    // Detect duplicate-root collisions. The CLI's local (non-daemon) path
    // emits these warnings to stderr; forward them through the JSON-RPC
    // response so daemon-routed callers and `--json` consumers see the
    // same diagnostic.
    let vaults = store.list_vaults(None).unwrap_or_default();
    let mut root_to_rows: std::collections::HashMap<&str, Vec<&nestweaver_schema::Vault>> =
        std::collections::HashMap::new();
    for v in &vaults {
        root_to_rows
            .entry(v.root_path.as_str())
            .or_default()
            .push(v);
    }
    let mut warnings: Vec<Value> = root_to_rows
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
                        db_path.and_then(|p| p.to_str()).unwrap_or("the database"),
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

    // nw-C2: a wedged index publication. Derived from the marker FILE, so
    // this populates with no daemon running — the user who reported the
    // wedge was on exactly that direct path. `kind` makes the entry
    // addressable for renderers that special-case warning shapes.
    if let (Some(p), Some(status)) = (db_path, publication)
        && status.is_wedged()
    {
        warnings.push(json!({
            "kind": "index_publication_wedged",
            "warning": if status.determinable {
                "index publication is wedged: ranked queries (brain_context, \
                 project_context, investigate) fail closed until it is reconciled. \
                 This is an index PUBLICATION marker, not a dirty git working tree."
            } else {
                "index publication marker state cannot be determined (permissions or \
                 I/O error on the sidecar directory); ranked queries fail closed."
            },
            "action": status.repair_command_for(p),
        }));
    }

    warnings
}

// ── 6. brain_add_source ─────────────────────────────────────────────────────

fn tool_schema_brain_add_source() -> Value {
    json!({
        "name": "brain_add_source",
        "description": "Index a new vault, code repo, or markdown folder into the brain graph. Auto-detects source type from directory contents.\n\nGuidelines:\n- Check brain_status first to avoid re-indexing already-indexed sources\n- Path must be absolute or start with ~/ (tilde expanded to $HOME)\n- Optional name sets a friendly display name for vaults (ignored for repos)\n\nLimitations:\n- Cannot index remote URLs directly — only local filesystem paths\n- Re-indexing an existing source overwrites the previous index",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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

fn resolve_add_source_path(raw_path: &str) -> Result<std::path::PathBuf, anyhow::Error> {
    nestweaver_engine::resolve_user_path(raw_path).map_err(anyhow::Error::new)
}

#[cfg(test)]
mod add_source_path_tests {
    use super::*;

    #[test]
    fn empty_path_is_rejected_instead_of_becoming_the_working_directory() {
        let cwd = std::env::current_dir().expect("working directory");
        assert!(cwd.is_dir(), "test requires an existing working directory");

        for input in ["", " ", "\t\r\n"] {
            let error = resolve_add_source_path(input)
                .expect_err("an empty source path must fail before source indexing");
            assert!(error.is::<nestweaver_engine::ResolveUserPathError>());
            assert!(error.to_string().contains("non-empty path"));
        }
    }
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
             Use a daemon-enabled build and daemon mode (the default)."
            ));
        }
        let raw_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("'path' must be a string"))?;
        let path = resolve_add_source_path(raw_path)?;
        let path = path.as_path();
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
                "coverage_status": if result.skipped.is_empty() { "complete" } else { "degraded" },
                "skipped_count": result.skipped.len(),
                "skipped_files": result.skipped,
            }));
        }

        if has_git {
            let db_path = current_db_path(store)?;
            // Identity: prefer the git origin remote when configured (used
            // only as an identity string — never fetched); fall back to a
            // file:// URL. The engine persists the disk location as
            // `root_path` on the Repo node.
            let url = nestweaver_engine::mint_repo_identity(path);
            let result = nestweaver_engine::index::index_directory_with_options_and_limits(
                path,
                &db_path,
                "default",
                &url,
                "local",
                false,
                None,
                configured_index_limits(),
            )
            .context("index repo")?;
            return Ok(json!({
                "kind": "repo",
                "url": url,
                "files": result.files_count,
                "symbols": result.symbols_count,
                "edges": result.edges_count,
                "coverage_status": if result.skipped_files.is_empty() { "complete" } else { "degraded" },
                "skipped_count": result.skipped_files.len(),
                "skipped_files": result.skipped_files,
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
        "description": "Remove an indexed code repository or markdown vault from the brain graph permanently.\n\nGuidelines:\n- Accepts repo name, vault name, filesystem path, file:// URL, or UID\n- Auto-detects whether the target is a repo or vault\n- To re-index (not remove), use brain_add_source instead\n\nReading the response:\n- `committed: true` means the removal HAPPENED and is durable. If `reconciliation_warnings` is also non-empty, some post-commit bookkeeping step failed — the removal still succeeded. Do NOT retry as though nothing happened, and do not take corrective action against the removed data; re-run only to retry the bookkeeping.\n- `committed: false` means nothing was removed.\n\nLimitations:\n- Removal is permanent — the source must be re-indexed with brain_add_source to restore\n- Ambiguous targets (matching multiple sources) require a UID to disambiguate",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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
    let expand = |input: &str| -> Option<String> {
        nestweaver_engine::resolve_user_path(input)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    };
    let target_trimmed = target.trim_end_matches('/');
    let expanded_target = expand(target_trimmed);
    if (target_trimmed == "~" || target_trimmed.starts_with("~/")) && expanded_target.is_none() {
        return Vec::new();
    }
    let canonical_target = expanded_target
        .as_deref()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .map(|p| format!("file://{}", p.display()))
        .unwrap_or_default();
    let url_target = if target_trimmed.starts_with("file://") {
        target_trimmed.to_string()
    } else if std::path::Path::new(target_trimmed).is_absolute() || target_trimmed.starts_with("~/")
    {
        expanded_target
            .as_deref()
            .map(|expanded| {
                std::fs::canonicalize(expanded)
                    .map(|p| format!("file://{}", p.display()))
                    .unwrap_or_else(|_| format!("file://{expanded}"))
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    // A path target may refer to a repo identified by its git origin remote
    // rather than a file:// URL — try that identity too (read from git config,
    // never fetched).
    let origin_target = expanded_target
        .as_deref()
        .and_then(|path| std::fs::canonicalize(path).ok())
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
        "description": "Remove all indexed repos and vaults whose source directories no longer exist on disk. No parameters required.\n\nGuidelines:\n- Use after moving, renaming, or deleting project directories\n- Returns the list of removed repos and vaults\n\nReading the response:\n- `committed: true` means the prune HAPPENED and is durable. If `reconciliation_warnings` is also non-empty, some post-commit bookkeeping step failed — the prune still succeeded. Do NOT retry as though nothing happened; re-run only to retry the bookkeeping.\n- `committed: false` means nothing was pruned.\n\nLimitations:\n- Only checks filesystem existence, not content staleness (use stale_check for that)\n- Cannot undo — removed sources must be re-indexed with brain_add_source",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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

// ── 6d. compact_embeddings ──────────────────────────────────────────────────

fn tool_schema_compact_embeddings() -> Value {
    json!({
        "name": "compact_embeddings",
        "description": "Reclaim embedding vectors left behind by deleted graph nodes, then rewrite the sidecar without them. Does NOT load the embedding model and does NOT re-embed anything.\n\nGuidelines:\n- Two reasons to run it. Disk is the obvious one. The other is result quality: the vector scan skips tombstoned rows, so a dead vector that was never tombstoned is still SCORED, and one outranking a live result silently consumes a top-k slot in semantic search.\n- Ongoing tombstoning is automatic. Use this on a brain that accumulated orphans before that existed, or to force the reclaim rather than waiting for the ratio threshold.\n- Check `brain_status` first: its `index_tombstoned` / `index_stored` fields show whether there is anything to reclaim.\n\nReading the response:\n- `reclaimed` is the number of dead vectors removed; `stored_before` / `stored_after` and `bytes_before` / `bytes_after` bracket the change.\n- `dry_run: true` reports only what is ALREADY tombstoned. Orphans that were never tombstoned are invisible to a dry run and are found by the reconcile a real run performs, so a dry run is a FLOOR, not a total.\n\nLimitations:\n- Requires daemon mode: it is a write, and rewriting the artifact outside the daemon's write gate can race an index or watcher batch.\n- Reclaims nothing on a brain with no deletions, which is the correct outcome, not a failure.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "dry_run": {
                    "type": "boolean",
                    "description": "Report occupancy without writing anything. Defaults to false.",
                    "default": false
                }
            },
            "required": [],
            "additionalProperties": false
        }
    })
}

fn tool_compact_embeddings(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    #[cfg(feature = "daemon")]
    {
        let dry_run = args
            .get("dry_run")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let db_path = current_db_path(store)?;
        let db_path_buf = std::path::PathBuf::from(&db_path);
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| anyhow::anyhow!("failed to create tokio runtime: {e}"))?;
        let sock_path = inline_ensure_daemon(&db_path_buf)
            .map_err(|e| anyhow::anyhow!("failed to start daemon: {e}"))?;
        let mut client = rt.block_on(inline_connect_daemon(&sock_path))?;
        let resp = rt
            .block_on(
                client.compact_embeddings(nestweaver_proto::CompactEmbeddingsRequest { dry_run }),
            )
            .map_err(|e| anyhow!("compact_embeddings RPC failed: {e}"))?;
        let inner = resp.into_inner();
        Ok(json!({
            "dry_run": inner.dry_run,
            "reclaimed": inner.reclaimed,
            "live": inner.live_after,
            "stored_before": inner.stored_before,
            "stored_after": inner.stored_after,
            "tombstoned_before": inner.tombstoned_before,
            "bytes_before": inner.bytes_before,
            "bytes_after": inner.bytes_after
        }))
    }
    #[cfg(not(feature = "daemon"))]
    {
        let _ = (store, args);
        Err(anyhow!("compact_embeddings requires daemon mode"))
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

    let mut client =
        nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient::new(channel)
            .max_decoding_message_size(256 * 1024 * 1024)
            .max_encoding_message_size(256 * 1024 * 1024);

    // nw-201 landed a version check on `daemon start --config` only. This path
    // — how every MCP tool reaches the daemon — had NONE: `inline_ensure_daemon`
    // returns as soon as the socket accepts a connection, and this function
    // never issued a HealthCheck at all. So after an upgrade an agent kept
    // talking to the previous binary, indexing through the old engine, with
    // nothing anywhere reporting a skew. The commit that added that check cites
    // "`brain status` looked healthy" as the tell; this is the path that made it
    // look healthy.
    //
    // Verified HERE rather than at the six call sites, so a new caller cannot
    // reintroduce the gap by forgetting it.
    let health = client
        .health_check(nestweaver_proto::HealthCheckRequest {})
        .await
        .map_err(|e| anyhow::anyhow!("daemon health check failed: {e}"))?
        .into_inner();
    if let Some(skew) =
        nestweaver_schema::describe_version_skew(&health.version, env!("CARGO_PKG_VERSION"))
    {
        anyhow::bail!("{skew} Restart the daemon to apply it.");
    }

    Ok(client)
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
        "description": "Find cross-repository references to a symbol — other repos that import, re-export, or implement the same symbol name.\n\nRequires either 'uid' or 'name' (at least one must be provided).\n\nGuidelines:\n- Use when modifying a shared symbol to understand cross-repo blast radius\n- Pass uid or name; returns other repos with confidence scores and link types\n- Only useful when multiple repos are indexed in the same brain\n\nLimitations:\n- For single-repo impact use brain_impact; for general search use brain_search\n- Contract links are hypotheses — check confidence scores before acting\n\nTrust contract: contracts_status (complete/degraded) + degraded_repos report whether contract derivation ran to completion at index time. Derivation failure is atomic, so a degraded repo keeps its PREVIOUS contract graph — its contract links are stale, not absent. Treat every `contract` link involving a degraded repo as 'unknown', not 'none' and not current.\n\nIn server mode, the server has the full org-wide view of cross-repo contracts. Through the hybrid client, results include _meta.sources indicating which data sources contributed; a raw single-daemon connection returns local results only.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "uid": { "type": "string", "description": "Symbol UID (e.g. sym:repo:...:hash:42). Preferred for unambiguous lookup." },
                "name": { "type": "string", "description": "Symbol name (e.g. \"UserService\"). Uses first match if multiple symbols share the name." },
                "limit": limit_schema(
                    "Max contract links to return (1-1000, default 50). The total count is always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
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
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;

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

    // Trust signal: contract derivation is best-effort at index time, so a repo
    // whose derivation failed contributes no `contract` rows at all. Without
    // this, "this symbol implements no contract" and "this symbol's repo has no
    // contract graph" look identical. The check is DB-wide rather than scoped
    // to the symbol's repo: contract UIDs carry no repo component, so the rows
    // above cannot be attributed to one repo, and a degraded repo anywhere can
    // cost this symbol a link.
    let degraded_repos = store
        .contract_derivation_failures(None)
        .map_err(|e| anyhow!("contract_derivation_failures: {e}"))?;
    let contracts_status = if degraded_repos.is_empty() {
        "complete"
    } else {
        "degraded"
    };

    Ok(json!({
        "uid": uid,
        "total": total,
        "returned": rows.len(),
        "note": "Links are hypotheses, not ground truth — check confidence. \
                 link_type \"contract\" denotes an implemented API contract.",
        "contracts_status": contracts_status,
        "degraded_repos": degraded_repos,
        "contracts": rows,
    }))
}

// ── 35. contract_drift ──────────────────────────────────────────────────────

fn tool_schema_contract_drift() -> Value {
    json!({
        "name": "contract_drift",
        "description": "Audit API contract drift: routes declared in specs (OpenAPI, .proto, GraphQL) but not implemented, and routes implemented but not declared in any spec.\n\nGuidelines:\n- Use to spot missing endpoints or undocumented APIs\n- Optional repo filter scopes to a single repository\n- Returns two buckets: declared_not_implemented and implemented_not_declared\n\nTrust contract (read before trusting a clean result):\n- contracts_status (complete/degraded) + degraded_repos: contract derivation is best-effort at index time and never fails the index. Failure is atomic, so a degraded repo's contracts and drift findings reflect its PREVIOUS successful derivation — stale, not wiped. `clean` is true only when the analysis ALSO ran to completion — a degraded repo reports clean: false with contracts_status \"degraded\"\n- Empty buckets on a complete run mean 'no drift'; empty buckets on a degraded run mean 'unknown', not 'safe'\n\nLimitations:\n- Contract links are hypotheses derived from spec parsing and handler heuristics (same-repo only)\n- Only supports OpenAPI/Swagger, .proto, and GraphQL spec formats\n\nIn server mode, the server has the full org-wide view of contract drift. Through the hybrid client, results include _meta.sources indicating which data sources contributed; a raw single-daemon connection returns local results only.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "repo": { "type": "string", "description": "Optional repo UID to scope the analysis to a single repository." },
                "limit": limit_schema(
                    "Max results per drift bucket (1-1000, default 50). Totals are always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
            }
        }
    })
}

fn tool_contract_drift(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let repo = args.get("repo").and_then(|v| v.as_str());
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
    let report = nestweaver_engine::contracts::drift_for_store(store, repo)
        .map_err(|e| anyhow!("drift_for_store: {e}"))?;
    // Shared with the CLI's local path so the two serializations cannot drift
    // apart again (they previously disagreed on totals, `clean`, `limit`, and
    // whether truncation happened at all).
    Ok(nestweaver_engine::contracts::drift_envelope(report, limit))
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
                "depth": { "type": "integer", "minimum": 1, "maximum": 15, "description": "Max traversal depth (1-15). Higher values find more transitive dependents but take longer. Default 3.", "default": 3 },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Max impact nodes to return (default 50). The total count is always reported.",
                    "default": DEFAULT_RESULT_LIMIT
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns affected symbol names only; \"detailed\" (default) adds file paths, edge types, confidence scores, and depth levels."
                },
                "cache": { "type": "string", "description": "Set to \"bypass\" to skip the response cache for this call." },
                "no_cache": { "type": "boolean", "description": "When true, skip the response cache for this call." }
            },
            "required": ["symbol"],
            "additionalProperties": false
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
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;
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
        // Fail closed on unknown/garbage/cross-DB UIDs — verify the UID
        // actually resolves in this store instead of trusting its shape. Keeps
        // the same not_found contract as the name path; a legit zero-dependent
        // symbol still resolves and returns status ok with an empty list.
        match store.lookup_symbol(symbol) {
            Ok(sym) if uid_is_visible(&sym.uid) => sym.uid,
            Ok(_) | Err(nestweaver_store::StoreError::NotFound) => {
                return Ok(json!({
                    "status": "not_found",
                    "symbol": symbol,
                    "impact_nodes": [],
                    "total": 0,
                    "returned": 0,
                }));
            }
            Err(e) => return Err(anyhow!("lookup_symbol: {e}")),
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

    let result = if let Some(owners) = &owners {
        let allowed: HashSet<String> = owners
            .iter()
            .filter(|(_, repo_uid)| repo_is_visible(repo_uid, visible))
            .map(|(uid, _)| uid.clone())
            .collect();
        store.impact_with_flags_within(&uid, depth, 0.0, &allowed, cancel)?
    } else {
        store.impact_with_flags(&uid, depth, 0.0, cancel)?
    };
    let truncated_by_threshold = result.truncated_by_threshold;
    let truncated_by_depth = result.truncated_by_depth;
    // nw-317 leg 1. Built by the SAME function the CLI's direct path calls,
    // so the default (daemon) route can no longer be the weaker disclosure.
    // `impact_with_flags` prunes at `DEFAULT_IMPACT_THRESHOLD`, so that is the
    // threshold this note reports.
    let note = result.truncation_note(nestweaver_store::DEFAULT_IMPACT_THRESHOLD, depth);
    let mut nodes = result.nodes;
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
        // Honesty flags (same semantics as blast_radius): when either is set
        // the reported impact set is a FLOOR, not the full reverse-closure.
        "truncated_by_threshold": truncated_by_threshold,
        "truncated_by_depth": truncated_by_depth,
        // F-PARITY-11: the CLI emits `truncated` and MCP did not, so an MCP
        // caller could not detect truncation here at all. Same "confident
        // answer to a partial read" family as nw-320, one field over.
        "truncated": truncated_by_threshold || truncated_by_depth || rows.len() < total,
        "note": note,
    }))
}

// ── 9. brain_guide ──────────────────────────────────────────────────────────

fn tool_schema_brain_guide() -> Value {
    json!({
        "name": "brain_guide",
        "description": "Generate a comprehensive orientation guide covering all indexed repos, vaults, cross-repo relationships, and available tools.\n\nGuidelines:\n- Call at session start for a read-once overview before issuing specific queries\n- Regenerated from current graph state on each call\n- The tools section is generated from the live MCP registry, so it never drifts from the actual tool set\n- Not a query tool — use brain_context or brain_search for specific lookups\n\nLimitations:\n- Can be expensive on large graphs; prefer brain_status for lightweight session initialization\n- Output size scales with number of indexed sources",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "format": {
                    "type": "string",
                    "enum": ["markdown", "skill", "cursor-rule", "agents-md", "claude-md"],
                    "default": "markdown",
                    "description": "Output format. \"markdown\" (default) is the full orientation guide; \"skill\" emits a Claude skill; the others emit the matching agent-instruction file. All formats render the tool list from the live registry."
                },
                "config": {
                    "type": "string",
                    "description": "Path to an instance config TOML. NOT supported by this handler (it generates from the graph only and cannot honor per-instance settings); passing it returns an explicit error. Use the CLI 'nestweaver generate-guide --config <path>' local path instead."
                }
            }
        }
    })
}

fn tool_brain_guide(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // This handler generates from the graph only and has no
    // InstanceConfig to honor. Silently ignoring a caller-supplied `config`
    // would return a guide shaped by the wrong instance — fail loudly instead
    // (the CLI already falls back to the local path when --config is given).
    if args
        .get("config")
        .and_then(|v| v.as_str())
        .is_some_and(|c| !c.trim().is_empty())
    {
        return Err(anyhow!(
            "brain_guide cannot honor the 'config' argument in this context; \
             use the CLI local path instead: nestweaver generate-guide --config <path>"
        ));
    }
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
        "description": "Trace forward execution flow from a symbol: what it calls, what those call, and so on. Returns a tree of callees.\n\nGuidelines:\n- Best for tracing from entry points (main, request handlers) to understand execution paths\n- Every child carries `edge_type`: CALLS and IMPORTS are observed in code, CROSS_REPO_LINK is an INFERRED cross-repo link. A CROSS_REPO_LINK child is NOT an observed call — filter it out when you need a real execution path\n- Cycles are detected and pruned; use max_depth to control tree depth (default 10)\n- Classes are auto-expanded to their methods since classes have no direct CALLS edges\n\nLimitations:\n- For reverse dependencies ('what calls this?') use brain_impact instead\n- For general structural context use brain_context",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name (e.g. \"handleRequest\") or full UID (e.g. \"sym:repo:...:hash:42\") to trace from." },
                "max_depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 15,
                    "description": "Maximum traversal depth (1-15). Default 10. Matches the CLI's --max-depth range; values outside it are rejected rather than silently accepted.",
                    "default": 10
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns function name chain only; \"detailed\" (default) adds file paths, UIDs, and depth at each node."
                }
            },
            "required": ["symbol"],
            // A mistyped argument name must fail loudly rather than be dropped.
            // Without this, `max_dpeth: 100` validated, the handler silently
            // used 10, and the caller believed they had set 100 — the same
            // silent-typo failure the config half of this change removes.
            "additionalProperties": false
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
        .unwrap_or(10)
        // The schema now declares 1..=15 (the CLI's `--max-depth` range, which
        // this tool accepted no equivalent of). Schema validation rejects
        // out-of-range MCP calls; this defensive clamp covers direct
        // unit/internal calls that bypass dispatch validation, matching
        // brain_search.
        .clamp(1, 15);
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

    // A failed callee lookup is NOT "this function calls nothing".
    //
    // This was `if let Ok(callees) = ...`, so a store error produced
    // `"children": []` — byte-identical to a genuine leaf, with no flag and no
    // note, on the flagship "what does this call" surface. The cancellation
    // path a few lines above already refuses to serve a truncated tree as a
    // real answer; an unreadable one is no different, and this function
    // already returns a `Result` to say so.
    if depth < opts.max_depth {
        let callees = store.callees_with_edge_types_of(uid).map_err(|error| {
            anyhow::anyhow!(
                "flow_trace: could not read the callees of {uid}: {error}. Refusing to \
                 report an empty call tree, which is indistinguishable from a leaf."
            )
        })?;
        for (callee, edge_type) in &callees {
            if visited.contains(&callee.uid) {
                continue;
            }
            visited.insert(callee.uid.clone());
            let mut child = build_flow_tree(
                store,
                &callee.uid,
                &callee.name,
                &callee.file_path,
                depth + 1,
                visited,
                opts,
            )?;
            // Label the edge that reached this node. The traversal spans CALLS,
            // IMPORTS and CROSS_REPO_LINK, and the last is an INFERRED link
            // between repos — following it as a call produced fabricated
            // execution paths (a Rust function appearing to call JavaScript
            // symbols in unrelated repos). Without this field they were
            // indistinguishable from real calls, on the flagship "what does this
            // call" surface. `impact` has always labelled its edges; this brings
            // flow_trace to parity (nw-111).
            if let Some(obj) = child.as_object_mut() {
                obj.insert("edge_type".to_string(), json!(edge_type));
            }
            children.push(child);
        }
    }

    if opts.concise {
        // The label ships in concise mode too: a caller scanning a compact tree
        // is exactly the one who cannot afford to mistake a cross-repo guess for
        // a call.
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
            "additionalProperties": false,
            "properties": {
                "changed_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "List of changed file paths (repo-relative). Example: [\"src/auth/login.ts\", \"src/utils/validate.ts\"]. One of 'changed_files' or 'files' is required."
                },
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Backward-compatible alias for changed_files."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Max affected symbols AND max affected processes to inline (default 50). The totals are always reported as affected_symbol_count / affected_process_count, and truncated says whether anything was omitted. NOTE: the cut is positional, not by importance — symbols are ordered by (file_path, name) and processes by name, because neither carries an impact score. Raise limit rather than assuming the first N are the most impactful; for ranked results use blast_radius.",
                    "default": DEFAULT_RESULT_LIMIT
                }
            }
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

    // nw-174: this tool inlined EVERY affected symbol and process. On a
    // high-fanout file that was 156 KB (~39K tokens) with 416 processes in one
    // call — a fifth of a context window — while every neighbouring tool
    // (brain_broken_links, brain_orphan_documents, blast_radius) already
    // capped and reported total vs returned. The counts below were already
    // honest; what was missing was the bound.
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_RESULT_LIMIT)
        .clamp(1, 1000);

    let impact = detect_changes_impact(store, &files, 10).context("detect_changes_impact")?;

    // Sorted before truncating. `affected_symbols` is built in
    // changed_files x symbols_in_file discovery order with no scoring, so a
    // bare `take(limit)` keeps an ARBITRARY subset — on a large change the
    // highest-fanout symbol could be dropped while trivial helpers from the
    // first file are kept, and which ones you got would shift with the order
    // the caller happened to list their files in.
    //
    // `AffectedSymbol` carries no impact score, so this CANNOT rank by
    // importance the way blast_radius does. Sorting by (file_path, name) buys
    // determinism and nothing more — which is why the schema says the cut is
    // positional rather than implying the first N matter most.
    let mut ranked_symbols: Vec<_> = impact.affected_symbols.iter().collect();
    ranked_symbols.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.name.cmp(&b.name)));

    let affected_symbols: Vec<Value> = ranked_symbols
        .iter()
        .take(limit)
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
        .take(limit)
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

    let symbols_omitted = impact
        .affected_symbols
        .len()
        .saturating_sub(affected_symbols.len());
    let processes_omitted = impact
        .affected_processes
        .len()
        .saturating_sub(affected_processes.len());

    Ok(json!({
        "files": files,
        "risk": risk_str,
        "status": serde_json::to_value(impact.status)?,
        "gate_state": serde_json::to_value(impact.gate_state)?,
        "notifications": serde_json::to_value(&impact.notifications)?,
        "blast_radius": impact.blast_radius,
        "affected_symbols": affected_symbols,
        // The TOTAL, not the returned length — so `affected_symbol_count`
        // against the array length is how a caller sees what was dropped, and
        // `truncated` says it outright rather than requiring the comparison.
        "affected_symbol_count": impact.affected_symbols.len(),
        "affected_processes": affected_processes,
        "affected_process_count": impact.affected_processes.len(),
        "limit": limit,
        "truncated": symbols_omitted > 0 || processes_omitted > 0,
        "symbols_omitted": symbols_omitted,
        "processes_omitted": processes_omitted,
    }))
}

// ── 31. affected_tests ──────────────────────────────────────────────────────

fn tool_schema_affected_tests() -> Value {
    json!({
        "name": "affected_tests",
        "description": "Prioritize which test files a PR should run by mapping changed files through the call/import graph to test files. Results bucketed into priority tiers.\n\nRequires either 'changed_files' or 'base_ref' (at least one must be provided).\n\nGuidelines:\n- Provide changed_files (repo-relative) or base_ref (git ref like 'main') to diff against\n- tier_1 = directly references changed symbol, tier_2 = direct caller, tier_3 = transitive\n- For symbol-level blast radius use brain_impact; for risk scoring use detect_changes\n- `recommendation` is a machine-readable CI directive: 'run-full-suite' on any non-complete run (fail-safe widening), 'selection-usable' otherwise\n\nLimitations:\n- Static call-graph regression test selection — misses reflection, DI, codegen, and integration/e2e tests\n- 'No tests found' does NOT mean safe to skip testing. IMPORTANT: keep periodic full test runs in CI\n\nWhen queried through the hybrid client (a local daemon connected to an upstream server), returns two-tier results (local_impact + org_wide_impact) with _meta.sources indicating provenance; a raw MCP connection to a single daemon returns single-tier local results.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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

/// Ceiling on `clusters.limit`. Public because the CLI must CLAMP to it before
/// forwarding `--limit`: clap's `--limit` is an unbounded `usize`, and under
/// `additionalProperties: false` an out-of-range value fails the entire call
/// rather than being clamped for us. Restating `1000` on the CLI side would be
/// a second copy of a bound that has already drifted once.
pub const CLUSTERS_LIMIT_MAX: usize = RESULT_LIMIT_MAX;

/// Ceiling on `clusters.members` — 200, NOT [`CLUSTERS_LIMIT_MAX`]. `limit` and
/// `members` multiply, and 1000 clusters x 2000 members is the original
/// 98.7 MB failure with extra steps. The asymmetry is the point, and is why the
/// CLI clamps against two named constants rather than one.
pub const CLUSTERS_MEMBERS_MAX: usize = 200;

fn tool_schema_clusters() -> Value {
    json!({
        "name": "clusters",
        "description": "View the codebase's high-level architecture via Louvain-style local moving community detection. Groups tightly-connected symbols into named functional clusters.\n\nGuidelines:\n- Adjust resolution: higher = more smaller clusters, lower = fewer larger clusters (default 0.5)\n- Returns cluster name, cohesion score, key files, and a 20-member preview per cluster (full `size` reported)\n- Pass cluster_id to get ONE cluster's full member list (paging deep clusters); `members_truncated` flags when even that is capped\n- For specific symbol lookup use brain_search; for dependency analysis use brain_impact\n\nLimitations:\n- Clustering is recomputed on each call (the result is persisted to a sidecar cache that the CLI `cluster` command reads)\n- Quality depends on the density and accuracy of indexed call/import edges",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                // F-MCP-6: no `default` key. The applied resolution depends
                // on the corpus (0.3 above 10K symbols, 0.5 at or below), which
                // JSON Schema cannot express — so the schema advertised 0.5
                // while the handler applied 0.3 on every graph this tool exists
                // to serve. Same precedent as `project_context.token_budget`:
                // state the truth in the description rather than a different
                // wrong number in the key.
                "resolution": {
                    "type": "number",
                    "exclusiveMinimum": 0.0,
                    "description": "Community-detection resolution parameter. Higher = more, smaller clusters; lower = fewer, larger clusters. When omitted the applied value depends on graph size: 0.3 for large graphs (>10K symbols), 0.5 at or below. Try 2.0 for fine-grained modules."
                },
                // nw-299(a). This was the only list-returning tool in the
                // catalogue with no bounding parameter, and
                // `additionalProperties: false` meant a caller-supplied `limit`
                // was actively REJECTED — 98.7 MB on the wire from a default
                // call, with no way for a client to prevent it. 50/20 are the
                // CLI twin's existing defaults, not new numbers; any others
                // would be a THIRD set for one concept.
                "limit": limit_schema(
                    "Max clusters to return (0 = all, default 50). `total`/`returned`/`truncated` always report what was dropped.",
                    50,
                    0,
                    CLUSTERS_LIMIT_MAX,
                ),
                // Maximum 200, not 2000: `limit` and `members` multiply, and
                // 1000 clusters x 2000 members is the original failure with
                // extra steps. The documented way to page INTO one cluster is
                // `cluster_id`, which this bound does not touch.
                "members": limit_schema(
                    "Max members previewed per cluster in the multi-cluster listing (0 = all, default 20). Ignored when `cluster_id` is set — that path returns the cluster's full member list, capped at 2000.",
                    20,
                    0,
                    CLUSTERS_MEMBERS_MAX,
                ),
                "cluster_id": {
                    "type": "integer",
                    "description": "Return only this cluster (by its numeric `id`), with its FULL member list instead of the preview. Use the same resolution as the call that produced the id."
                }
            }
        }
    })
}

fn tool_clusters(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // F-DC-7: the adaptive default now comes from the engine, so this tool,
    // the `clusters`/`cluster` CLI commands and `summary --level cluster` all
    // partition the graph the same way. They did not: summaries hard-coded
    // resolution 1.0, which put its cluster IDs in a different ID SPACE from
    // the one `cluster <id>` resolves against — 26 of 50 IDs did not resolve.
    let resolution = args
        .get("resolution")
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| nestweaver_engine::default_cluster_resolution(store));
    let limit = read_limit(&args, "limit", 50, 0, CLUSTERS_LIMIT_MAX)?;
    let preview_members = read_limit(&args, "members", 20, 0, CLUSTERS_MEMBERS_MAX)?;

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
    // cluster can't blow the context window), a caller-sized preview otherwise.
    const FULL_MEMBER_CAP: usize = 2000;
    let matching: Vec<&nestweaver_engine::CommunityInfo> = output
        .communities
        .iter()
        .filter(|c| requested_id.is_none_or(|id| c.id as i64 == id))
        .collect();
    // Cut BEFORE rendering, and capture the pre-cut total. Rendering the whole
    // corpus for a bounded answer is the other half of this defect class.
    let bounded = Bounded::take(matching, limit).map(|c| {
        {
            let member_cap = if requested_id.is_some() {
                FULL_MEMBER_CAP
            } else if preview_members == 0 {
                c.members.len()
            } else {
                preview_members
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
                        // Include kind so the daemon path renders the same
                        // member shape as the CLI direct path (ClusterMember).
                        "kind": m.kind,
                    })
                })
                .collect();
            let members_returned = members.len();
            json!({
                "id": c.id,
                "name": c.name,
                "size": c.member_count,
                "cohesion": c.cohesion,
                "key_files": c.key_files,
                "members": members,
                "returned_members": members_returned,
                "members_truncated": c.members.len() > member_cap,
            })
        }
    });

    let symbol_count: usize = output.communities.iter().map(|c| c.member_count).sum();

    let mut payload = json!({
        "resolution": resolution,
        // The graph-wide community count, unchanged. `total` below is the
        // number that MATCHED this call's filter; with no `cluster_id` the two
        // agree, and with one they must not.
        "cluster_count": output.communities.len(),
        "symbol_count": symbol_count,
        "modularity": output.modularity,
        "limit": limit,
    });
    bounded.merge_into(&mut payload, "clusters");
    Ok(payload)
}

// ── 13. stale_check ────────────────────────────────────────────────────────

fn tool_schema_stale_check() -> Value {
    json!({
        "name": "stale_check",
        "description": "Check whether the graph index is current by comparing each repo's indexed git SHA against HEAD. No parameters required.\n\nGuidelines:\n- Call at session start or after code changes to verify index freshness\n- Returns per-repo staleness with indexed SHA, HEAD SHA, and commits-behind count\n- If stale, re-index with brain_add_source or CLI nestweaver index\n\nLimitations:\n- Only checks git repos, not vault/note freshness\n- For viewing what actually changed, use brain_diff",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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
    let mut any_needs_reindex = false;

    for repo in &repos {
        // A local working tree that no longer exists on disk is
        // unverifiable — flag it `[missing]` and count it as stale instead of
        // silently reporting `[ok]`.
        let local_missing = repo
            .local_root()
            .map(|p| !std::path::Path::new(p).exists())
            .unwrap_or(false);

        // Local working tree → read HEAD from disk; otherwise ask the remote.
        // nw-266: shared with the CLI route, which hand-rolled this and
        // dropped the remote branch entirely. See `repo_head`.
        let current_head =
            nestweaver_engine::repo_head::current_head(local_missing, repo.local_root(), &repo.url);

        // Compute commits behind for local repos when HEAD differs from indexed SHA.
        let is_valid_sha = nestweaver_engine::repo_head::is_full_sha(&repo.indexed_sha);
        //
        // `unwrap_or(0)` here produced a CONTRADICTION rather than a missed
        // staleness: this branch is only reached when HEAD differs from the
        // indexed SHA, so `is_stale` is already true — and the row then read
        // "stale, 0 commits behind". Exactly the kind of self-inconsistent
        // output nw-163 (below) was raised to remove.
        //
        // `None` means "we could not count", which is what a failed `git
        // rev-list` actually tells us, and is distinguishable from a real zero.
        let commits_behind: Option<u64> = match (&current_head, repo.local_root()) {
            (Some(head), Some(path)) if is_valid_sha && *head != repo.indexed_sha => {
                nestweaver_engine::repo_head::commits_between(path, &repo.indexed_sha, head)
            }
            _ => Some(repo.staleness_commits_behind as u64),
        };

        // nw-163: `is_stale` means BEHIND HEAD, and nothing else.
        //
        // It used to be the union of three unrelated conditions — behind HEAD,
        // working tree missing, and index incomplete — so a repo sitting
        // exactly at HEAD with zero commits behind reported `is_stale: true`,
        // which reads as a contradiction. `status` already classified the
        // three correctly; only this flag conflated them.
        let is_stale = match &current_head {
            Some(head) => head != &repo.indexed_sha,
            // Working tree gone: HEAD is unknowable, so "behind HEAD" cannot
            // be asserted. The stored counter is a leftover from the last
            // successful check. `status: "missing"` + `needs_reindex` carry
            // the actionable truth without guessing.
            None if local_missing => false,
            // An uncountable distance is not a claim of zero: if HEAD is
            // unknown AND the stored counter cannot be read, staleness is
            // simply not assertable here.
            None => commits_behind.is_some_and(|behind| behind > 0),
        };

        // A repo whose SHA was committed but whose content never landed
        // (interrupted index) compares equal to HEAD yet serves an empty graph.
        // That is NOT staleness — it is incompleteness — but it needs the same
        // remedy, which is what `needs_reindex` expresses.
        let content_missing = store
            .repo_index_incomplete(repo)
            .map_err(|e| anyhow!("repo_index_incomplete: {e}"))?;

        let status = if local_missing {
            "missing"
        } else if content_missing {
            "incomplete"
        } else if is_stale {
            "stale"
        } else {
            "ok"
        };
        // The ACTIONABLE union, and the only thing a CI gate should key on:
        // every non-`ok` status is fixed by re-indexing.
        let needs_reindex = status != "ok";

        if is_stale {
            any_stale = true;
        }
        if needs_reindex {
            any_needs_reindex = true;
        }

        results.push(json!({
            "url": repo.url,
            "indexed_sha": repo.indexed_sha,
            "current_head": current_head,
            "is_stale": is_stale,
            "needs_reindex": needs_reindex,
            "staleness_commits_behind": commits_behind,
            "status": status,
        }));
    }

    // nw-315: the two PRE-SUMMARISED lists, authored here rather than in the
    // CLI. Both routes of `stale-check --json` derived them from `repos`
    // themselves — Route A by post-processing the daemon's payload
    // (`src/main.rs`), Route B by a complete second implementation of this
    // whole loop — so an MCP caller was told "at least one repo needs
    // re-indexing" and had to linearly scan a 43-entry array to learn which.
    // `needs_reindex_repos` is the field 8.0.0 added as a documented breaking
    // change and it had never reached the MCP surface at all.
    //
    // Deriving them from `results` (not from a parallel pass) is what keeps
    // them from drifting from `is_stale`/`needs_reindex` the way they drifted
    // three times before.
    let urls_where = |field: &str| -> Vec<Value> {
        results
            .iter()
            .filter(|repo| repo[field].as_bool().unwrap_or(false))
            .filter_map(|repo| repo["url"].as_str().map(Value::from))
            .collect()
    };
    let stale_repos = urls_where("is_stale");
    let needs_reindex_repos = urls_where("needs_reindex");

    Ok(json!({
        "repo_count": repos.len(),
        // Behind HEAD only. Kept because it is what the word means; a CI gate
        // wanting "is my graph usable" should read `any_needs_reindex`.
        "any_stale": any_stale,
        "any_needs_reindex": any_needs_reindex,
        // Behind HEAD only, matching `any_stale`/`is_stale`.
        "stale_repos": stale_repos,
        // The ACTIONABLE set, matching `any_needs_reindex`/`needs_reindex`.
        "needs_reindex_repos": needs_reindex_repos,
        "repos": results,
    }))
}

// ── 14. set_extension ──────────────────────────────────────────────────────

fn tool_schema_set_extension() -> Value {
    json!({
        "name": "set_extension",
        "description": "Attach custom key-value metadata to any node (symbol, note, section, tag) in a JSON sidecar alongside the database.\n\nGuidelines:\n- Use for information not in core schema: team ownership, deprecation status, review flags\n- Value accepts any JSON type (string, number, boolean, array, object); overwrites existing\n- Properties persist across sessions and are queryable via query_extensions\n\nLimitations:\n- Stored in a sidecar file, not the main graph — not included in graph traversals\n- To query existing properties use query_extensions, not this tool",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
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
            "additionalProperties": false,
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Property name to filter by (e.g. \"team_owner\", \"deprecated\"). Required when not using uid mode."
                },
                "value": {
                    "description": "Value to match — any JSON value. Required when key is provided. Exact match, plus membership: a SCALAR query matches when the stored property is an array containing it (so key=\"aliases\", value=\"Widget\" matches [\"Widget\",\"widget\"]). An ARRAY query stays an exact whole-array comparison, not any-of. Pass a real JSON value, not a stringified one."
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
            "additionalProperties": false,
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repo name or substring of its URL (e.g. \"nestweaver\" or \"github.com/org/repo\"). Matched against indexed repos."
                },
                "since_sha": {
                    "type": "string",
                    "description": "Git SHA to compare against. Defaults to the repo's indexed_sha. Use a specific SHA to diff against an older baseline."
                },
                "limit": limit_schema(
                    "Max affected symbols to return (1-1000, default 50). The total count is always reported.",
                    DEFAULT_RESULT_LIMIT, 1, RESULT_LIMIT_MAX)
            },
            "required": ["repo"]
        }
    })
}

fn tool_brain_diff(
    store: &GraphStore,
    args: Value,
    visible: Option<&nestweaver_engine::authz::VisibleRepos>,
) -> Result<Value, anyhow::Error> {
    use nestweaver_engine::git_diff;

    let repo_name = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'repo' must be a string"))?;
    let since_sha_arg = args.get("since_sha").and_then(|v| v.as_str());
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;

    // Find the repo in the graph using the shared deterministic selector. The
    // caller supplies the already-visible repository set in authenticated
    // server dispatches, so the helper cannot widen authorization scope.
    let repos = store
        .list_repos(None)?
        .into_iter()
        .filter(|repo| visible.is_none_or(|scope| scope.allows(&repo.uid)))
        .collect::<Vec<_>>();
    let repo = nestweaver_engine::resolve_repo_selector(&repos, repo_name)?;

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
        "description": "Retrieve context for a named project: notes, symbols, and sections ranked by PPR within the project's subgraph, bounded by token budget.\n\nGuidelines:\n- Use when you know the project name — for ad-hoc topics use brain_context with seeds instead\n- Returns a CONCISE orientation by default (~1000 tokens: kind/title/location per node); pass response_format:'detailed' for full metadata (uid + relevance, ~3000 tokens)\n- Narrow with repos, path_prefix, tags/exclude_tags, kinds, since, recency_weight — carry the same filter names over to brain_context when drilling in\n- For composite projects, include_components pulls in sub-project content\n\nLimitations:\n- Requires projects to be defined in the graph (via vault taxonomy or instance config)\n- If you don't know the project name, use brain_search to find it first\n- May fail with 'index publication TRANSIENT/WEDGED' while an index is being published. This refers to INDEX PUBLICATION, not a dirty git working tree: editing files in a repo does NOT cause it, and NestWeaver is fully usable while you work. TRANSIENT resolves on its own — retry. WEDGED means a prior indexer died mid-publication; ASK THE OPERATOR to run the `nestweaver repair` command named in the error — repair is a destructive publication recovery with no MCP tool, so it cannot be done from here — or check brain_status.index_publication.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project name (e.g. \"AuthService\"), alias, or UID. Resolved via name match, then alias match, then UID substring match."
                },
                "token_budget": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 16000,
                    "description": "Approximate token cap for the result (chars / 4, 1-16000). When omitted the budget follows `response_format`: ~1000 for concise (the default), ~3000 for detailed. Increase for comprehensive context, decrease for quick overview."
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
                "intent": intent_schema(
                    "Optional query intent hint that adjusts ranking strategy. 'find-definition' boosts exact name matches; 'understand-architecture' broadens to structural neighbors (default for project_context); 'analyze-impact' (alias 'blast-radius') follows dependency edges; 'general-context' uses balanced defaults."
                ),
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "concise",
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
                    "description": "Keep only note/section nodes tagged with any of these tags. Symbol nodes are always kept. Matching is case-insensitive and includes NESTED descendants: \"project\" matches \"project/nestweaver\" but never \"projectile\"."
                },
                "exclude_tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Drop note/section nodes tagged with any of these tags. Matching is case-insensitive and includes NESTED descendants: \"project\" matches \"project/nestweaver\" but never \"projectile\". An excluded parent therefore drops its whole subtree."
                },
                "cache": { "type": "string", "description": "Set to \"bypass\" to skip the response cache for this call." },
                "no_cache": { "type": "boolean", "description": "When true, skip the response cache for this call." }
            },
            "required": ["project"],
            "additionalProperties": false
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
    // nw-295. Validated and NORMALISED at the boundary, once. The value used
    // to go straight into `WHERE n.modified_at >= $since`, which is a
    // LEXICOGRAPHIC comparison against a String column and therefore can never
    // fail — so `since: "garbage"` was byte-identical to `since: "2099-12-31"`:
    // both matched no note and silently dropped every Note and Section from
    // the answer. The `.filter(|s| !s.is_empty())` matters too: the CLI's
    // daemon route sends `""` for an absent `--since`, and it survives today
    // only because the daemon strips empty strings before dispatch.
    if let Some(since) = args
        .get("since")
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
    {
        let since = nestweaver_engine::parse_since(since).map_err(|e| anyhow!("{e}"))?;
        let recent_notes = store
            .list_note_uids_modified_since(&since)
            .map_err(|e| anyhow!("list_note_uids_modified_since: {e}"))?;
        let recent_sections = store
            .list_section_uids_modified_since(&since)
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
    // nw-188: a caller could not distinguish "this project is empty" from
    // "your budget was too small", because `connected: []` was reported
    // identically in both cases with no flag. Seeds are charged against the
    // budget FIRST, so a budget smaller than the seed overhead leaves zero for
    // connected and drops every one of them silently — reproduced as
    // token_budget 200 / tokens_used 257 / connected [] / seeds_expanded 114.
    // Provisional: `budgeted_cut` decided this, but the probe loop below can
    // drop more. The authoritative count is recomputed after it.
    let provisional_dropped = result.connected.len().saturating_sub(cut);
    // Overwritten by the probe block with the ACTUAL serialized size.
    #[allow(unused_assignments)]
    let mut final_payload_tokens = used_tokens;

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
            "more_available": provisional_dropped,
            "truncated": provisional_dropped > 0,
            "budget_exceeded": used_tokens > token_budget,
            "semantic_applied": result.semantic_applied,
            "degraded_components": &result.degraded_components,
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
        let mut actual_tokens = serialized.len().div_ceil(4);
        if actual_tokens > token_budget {
            while connected_json.len() > 1 {
                connected_json.pop();
                probe["connected"] = json!(connected_json);
                let check = serde_json::to_string(&probe)?;
                actual_tokens = check.len().div_ceil(4);
                if actual_tokens <= token_budget {
                    break;
                }
            }
        }
        final_payload_tokens = actual_tokens;
    }

    // RECOMPUTED after the probe loop, not before it. The loop above pops
    // further entries off `connected_json` to fit the SERIALIZED payload under
    // the budget, so a count taken from `budgeted_cut` alone understates what
    // the caller actually lost — the response would have reported a smaller
    // `more_available` than the number of items genuinely missing, which is the
    // same class of dishonesty this item exists to remove.
    let connected_dropped = result.connected.len().saturating_sub(connected_json.len());

    // nw-188: `more_available` and `truncated` name what was dropped;
    // `budget_exceeded` says plainly that `tokens_used` is over `token_budget`
    // rather than leaving a caller to notice the arithmetic. Reporting the
    // real `tokens_used` and flagging it is the honest pair — silently
    // clamping the number would trade one lie for another.
    let mut resp = json!({
        "project": project.name,
        "project_uid": project.uid,
        "seeds_expanded": result.seeds.len(),
        "connected": connected_json,
        "token_budget": token_budget,
        "more_available": connected_dropped,
        "truncated": connected_dropped > 0,
        // The ACTUAL serialized size, not the pre-serialization estimate.
        //
        // The estimate charges `seed_tokens` against the budget even when
        // `include_seeds` is false and the seeds are therefore NOT in the
        // payload — which is how the reported case produced `tokens_used: 257`
        // for a response that serialized to a fraction of that. Reporting the
        // estimate as "used" was itself the dishonesty; `seed_tokens_charged`
        // below explains where the budget actually went.
        "tokens_used": final_payload_tokens,
        "seed_tokens_charged": seed_tokens,
        "budget_exceeded": final_payload_tokens > token_budget,
        "semantic_applied": result.semantic_applied,
        "degraded_components": &result.degraded_components,
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
        "description": "Find potentially unreachable symbols by walking forward from all entry points (main, HTTP handlers, event listeners, test runners).\n\nGuidelines:\n- Confidence scoring: High (private BY CONVENTION — leading underscore, or a lowercase-initial name in a Go file), Medium (everything else, INCLUDING an explicitly private symbol), Low (explicitly public — could be library API)\n- Use min_confidence to filter; 'low' shows all, 'high' shows only strong candidates\n- unreachable_count is the unfiltered total (consistent with total_symbols/reachable_symbols/dead_percentage); matching_count is the post-min_confidence count; returned/truncated disclose the limit cap\n- For understanding what depends on a specific symbol use brain_impact instead\n\nLimitations:\n- Static reachability analysis — misses runtime reflection, DI, and dynamic dispatch\n- Confidence ranks how UNADDRESSABLE a symbol is from outside its file, not how certain the reachability walk is. Treat every tier as review candidates: a reference the parser does not capture is indistinguishable from no reference. `private` visibility alone does NOT reach High — on a real index that population measured ~0% precision (known limitation)\n- Public symbols flagged as Low confidence may be consumed by external code",
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
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Max unreachable symbols to return (defaults to the configured result limit). The response reports the true total in 'unreachable_count' and sets 'truncated' when the cap applied."
                },
                "cache": { "type": "string", "description": "Set to \"bypass\" to skip the response cache for this call." },
                "no_cache": { "type": "boolean", "description": "When true, skip the response cache for this call." }
            },
            "additionalProperties": false
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
    // add_limit_metadata, but the stdio path had no bound).
    // Count contract: `unreachable_count` is the UNFILTERED total (consistent
    // with `total_symbols`/`reachable_symbols`/`dead_percentage`, which are
    // also unfiltered); `matching_count` is the post-`min_confidence` count;
    // `returned`/`truncated` disclose the cap.
    let limit = read_limit(
        &args,
        "limit",
        configured_result_limit(),
        1,
        RESULT_LIMIT_MAX,
    )?;

    let result = detect_dead_code_cancellable(store, cancel).context("detect_dead_code")?;

    let total_unreachable = result.unreachable_symbols.len();
    let all_matching: Vec<_> = result
        .unreachable_symbols
        .iter()
        .filter(|s| s.confidence >= min_conf)
        .collect();
    let matching_count = all_matching.len();
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
        "matching_count": matching_count,
        "returned": filtered.len(),
        "truncated": matching_count > filtered.len(),
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
        "description": "Identify the most connected symbols in the codebase ranked by total degree (incoming + outgoing edges). These are the architectural core.\n\nGuidelines:\n- Use for quick orientation on which abstractions are most central\n- Includes optional cluster membership when clustering sidecar exists\n- For chokepoints between communities use bridge_nodes instead\n\nLimitations:\n- Degree centrality only — does not account for path importance (use bridge_nodes for betweenness)\n- For specific symbol dependencies use brain_impact or flow_trace\n- If `rankings_stale` is true, some repos were indexed before the nw-103 import-fan-out fix and these scores are computed over unrepaired edges — read `note` and re-index before trusting them",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Number of top hubs to return. Default 10.",
                    "default": 10
                },
                "top_n": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Backward-compatible alias for limit."
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns name + total degree only; \"detailed\" (default) adds UIDs, file paths, PageRank scores, and cluster IDs."
                },
                "cache": { "type": "string", "description": "Set to \"bypass\" to skip the response cache for this call." },
                "no_cache": { "type": "boolean", "description": "When true, skip the response cache for this call." }
            },
            "additionalProperties": false
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
        attach_note(
            &mut resp,
            "cluster_id is null because clustering has not been computed. Run 'nestweaver cluster' to populate.".to_string(),
        );
    }
    if let Some(note) = ranking_staleness_note(store) {
        resp["rankings_stale"] = json!(true);
        attach_note(&mut resp, note);
    }
    Ok(resp)
}

// ── 20. bridge_nodes ──────────────────────────────────────────────────────

fn tool_schema_bridge_nodes() -> Value {
    json!({
        "name": "bridge_nodes",
        "description": "Find architectural chokepoints — symbols with high betweenness centrality that sit on many shortest paths between other nodes.\n\nGuidelines:\n- Use to identify symbols with outsized blast radius if changed\n- Returns betweenness score plus which community clusters each bridge connects\n- For most-connected nodes (degree centrality) use hub_nodes instead\n\nLimitations:\n- Betweenness computed via Brandes' algorithm with sampling — approximate for large graphs\n- For single-symbol impact analysis use brain_impact\n- If `rankings_stale` is true, some repos were indexed before the nw-103 import-fan-out fix and these scores are computed over unrepaired edges — read `note` and re-index before trusting them",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "limit": {
                    "type": "integer",
                    // nw-251: bounded to match `top_n` below. `limit` carried
                    // NEITHER bound while its alias carried both — and the
                    // handler PREFERS `limit`, so the bounded field was the
                    // one that never applied.
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Number of top bridges to return (1-1000). Default 10.",
                    "default": 10
                },
                "top_n": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Alias for `limit`, accepted for backward compatibility."
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

    let mut resp = json!({
        "top_n": top_n,
        "count": nodes_json.len(),
        "bridges": nodes_json,
    });
    if let Some(note) = ranking_staleness_note(store) {
        resp["rankings_stale"] = json!(true);
        attach_note(&mut resp, note);
    }
    Ok(resp)
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
                    "minimum": 1,
                    "maximum": 15,
                    "description": "Maximum transitive traversal depth (1-15). Default 3. Higher values find more distant dependents.",
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
                // Found by asserting the bound over the REGISTRY rather than
                // the ten sites the report named: this one declared `minimum`
                // and no `maximum`. Omission still means "the full set" — the
                // ceiling bounds an EXPLICIT request, so there is nothing to
                // gain by asking for more than it.
                "limit": bounded_integer_schema(
                    "Cap on returned affected_symbols (most-impactful first, 1-1000). Omit for the full set; a truncation note reports the true total.",
                    1,
                    RESULT_LIMIT_MAX,
                ),
                "format": {
                    "type": "string",
                    "enum": ["json", "sarif"],
                    "description": "Output format. 'json' (default) is the native result; 'sarif' emits SARIF v2.1.0 for GitHub code scanning / Azure DevOps / the VS Code SARIF viewer.",
                    "default": "json"
                },
                "cache": { "type": "string", "description": "Set to \"bypass\" to skip the response cache for this call." },
                "no_cache": { "type": "boolean", "description": "When true, skip the response cache for this call." }
            },
            "required": ["changed_files"],
            "additionalProperties": false
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
            "additionalProperties": false,
            "properties": {
                "level": {
                    "type": "string",
                    "enum": ["symbol", "file", "cluster", "hub"],
                    "description": "Summary granularity. 'symbol' = per-function/class, 'file' = per-file exports, 'cluster' = per-community architecture, 'hub' = top hub nodes with call-graph shape + role (architectural orientation).",
                    "default": "file"
                },
                "name": {
                    "type": "string",
                    "description": "Alias for `target`, accepted for backward compatibility."
                },
                "target": {
                    "type": "string",
                    "description": "Optional filter: file path, symbol name, or cluster name substring. Only matching summaries are returned."
                },
                "token_budget": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Approximate token cap for the result. Defaults to 20000; pass 0 for unlimited. The response reports `total_available` and sets `truncated` when the cap applied.",
                }
            }
        }
    })
}

/// `generate_summaries`, plus the count its return type cannot express.
///
/// The shape is per-level on purpose, so a capped level cannot be added
/// without deciding what it reports. That paid off: this arm's earlier note
/// said Cluster was the only generator that caps, and a sweep of all four
/// levels found `Hub` doing the same thing with an internal `HUB_COUNT` of 30
/// — measured at `{returned: 30, total: 30, truncated: false}` on a
/// 180-candidate graph. `File` genuinely does not cap; `Symbol` is handled on
/// its own fast path above.
fn generate_summaries_reporting_cap(
    store: &GraphStore,
    level: nestweaver_engine::SummaryLevel,
    cap_dropped: &mut usize,
) -> Result<Vec<nestweaver_engine::Summary>, anyhow::Error> {
    match level {
        nestweaver_engine::SummaryLevel::Cluster => {
            let bounded = nestweaver_engine::summaries::generate_cluster_summaries_bounded(
                store,
                nestweaver_engine::summaries::MAX_CLUSTER_SUMMARIES,
            )?;
            *cap_dropped = bounded
                .matched_total
                .saturating_sub(bounded.summaries.len());
            Ok(bounded.summaries)
        }
        nestweaver_engine::SummaryLevel::Hub => {
            let bounded = nestweaver_engine::summaries::generate_hub_summaries_bounded(store)?;
            *cap_dropped = bounded
                .matched_total
                .saturating_sub(bounded.summaries.len());
            Ok(bounded.summaries)
        }
        _ => generate_summaries(store, level),
    }
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
    // Defaults to the SAME constant the CLI's `--token-budget` defaults to, and
    // `0` still means unlimited on both. nw-182 bounded this on the CLI because
    // `summary --level file --json` emitted 8.3 MB — output proportional to the
    // corpus, not the question — and the reason recorded for fixing it was
    // "this is exposed as an MCP tool where that is a context-window bomb".
    // The cap then went in on the CLI only, so the surface the fix was FOR kept
    // emitting the 8.3 MB, to an agent, unasked.
    let requested_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(nestweaver_engine::SUMMARY_DEFAULT_TOKEN_BUDGET);
    let token_budget = (requested_budget > 0).then_some(requested_budget);

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
            // nw-321: `returned`/`total` is the one pair of count names, and
            // `summaries` is STRUCTURED. The CLI twin returned a list with
            // `returned`/`total` while this returned a "\n"-joined string with
            // `count`/`total_available`, so the human got structure and the
            // agent got prose to re-parse — the inverse of who benefits from
            // it — and no caller could be written against both. `count` /
            // `total_available` are kept as aliases of the SAME values for one
            // release; they are not a second contract.
            "returned": display.len(),
            "total": matched_total,
            "count": display.len(),
            "total_available": matched_total,
            "tokens_used": total_tokens,
            "token_budget": token_budget,
            "truncated": truncated_by_budget || capped,
            "partial": capped,
            "cached": false,
            "note": note,
            "summaries": display,
            "summaries_text": render_text(&display),
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
    // F-DC-11. `generate_cluster_summaries` truncates to 50 INSIDE the
    // generator, so a total taken from what it returned reports 50 against a
    // 71,184-community graph and `truncated` computes to false. The honesty
    // machinery existed and was wired for `SummaryLevel::Symbol` only.
    let mut cap_dropped: usize = 0;
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
            let fresh = generate_summaries_reporting_cap(store, level, &mut cap_dropped)?;
            (fresh, false)
        } else {
            (level_filtered, true)
        }
    } else {
        let fresh = generate_summaries_reporting_cap(store, level, &mut cap_dropped)?;
        (fresh, false)
    };

    // Persist freshly generated summaries so subsequent calls hit the cache,
    // preserving cached entries at other levels (shared invariant).
    if !from_cache && let Some(ref db) = db_path {
        merge_and_save_summaries(db, store.graph_generation(), level, &summaries);
    }

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
    // nw-321. This was `summaries.len() + cap_dropped`, computed BEFORE
    // `filter_by_target`, while the CLI twin's `total` is computed AFTER it.
    // The two coincide only when no `target` is passed — which is how the QA
    // saw both report 9657 and concluded the names were a pure rename. Pass a
    // `target` and they are different quantities under different names.
    //
    // Reconciled on the AFTER-filter side, matching the CLI: a total that
    // ignores the filter the caller asked for is not a total of anything the
    // caller can see. `cap_dropped` is still added because those rows matched
    // and were dropped by the generator's cap, not by the caller's filter.
    let total_available = after_filter_len + cap_dropped;
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
        // See the symbol-level payload above: `returned`/`total` is the one
        // pair of names, `summaries` is the structured list the CLI twin
        // returns, and `count`/`total_available` are aliases of the same
        // values for one release.
        "returned": display.len(),
        "total": total_available,
        "count": display.len(),
        "total_available": total_available,
        "tokens_used": total_tokens,
        "token_budget": token_budget,
        // Either cause: the generator's cap upstream, or the budget here.
        // Reporting only the second made the first vanish (F-DC-11).
        "truncated": display.len() < after_filter_len || cap_dropped > 0,
        "cached": from_cache,
        "summaries": display,
        "summaries_text": text,
    }))
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
    static DIRECT_READ_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
    // nw-C2: bounded wait budget for an in-flight index publication.
    // Seeded from NESTWEAVER_INDEX_PUBLICATION_WAIT_MS on first use so the
    // env var is read once per thread rather than per dispatch, and so tests
    // can override it without racing on process-wide environment mutation.
    static INDEX_PUBLICATION_WAIT_MS: std::cell::Cell<u64> =
        std::cell::Cell::new(env_index_publication_wait_ms());
}

/// Default bounded wait, in milliseconds, from the environment.
fn env_index_publication_wait_ms() -> u64 {
    const DEFAULT_MS: u64 = 3_000;
    const MAX_MS: u64 = 30_000;
    std::env::var("NESTWEAVER_INDEX_PUBLICATION_WAIT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MS)
        .min(MAX_MS)
}

/// Override the bounded index-publication wait for this thread. `0` disables
/// the wait entirely (the pre-nw-C2 behaviour).
pub fn set_index_publication_wait_ms(ms: u64) {
    INDEX_PUBLICATION_WAIT_MS.with(|c| c.set(ms.min(30_000)));
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
    configured_result_limit_or(DEFAULT_RESULT_LIMIT)
}

/// The operator's configured result limit, or `builtin` when they set none.
///
/// Tools document different defaults on purpose — `brain_search` says 20 while
/// the paginated tools say 50 — so each passes its own rather than inheriting a
/// shared constant it never advertised.
fn configured_result_limit_or(builtin: usize) -> usize {
    current_instance_config()
        .and_then(|cfg| cfg.limits.default_result_limit)
        .unwrap_or(builtin)
}

// ── The (bound, total, truncated) seam ──────────────────────────────────────
//
// `configured_result_limit_or` above governs the DEFAULT and nothing else —
// not the clamp, not the reported total, not the truncation disclosure. So
// the `(bound, total, truncated)` triple was reimplemented independently in
// sixteen-plus places across this file and `src/main.rs`, under five key
// spellings (`total`, `total_available`, `more_available`, `connected_count`,
// `proposals_total`), ABSENT in two commands, and computed AFTER the cap in a
// third — which is how `summary --level cluster` came to report
// `{returned: 50, total: 50, truncated: false}` against 71,184 communities.
//
// The three items below are the seam that was missing. `limit_schema` emits
// the declaration, `read_limit` parses the argument against the SAME bounds,
// and `Bounded` captures `total` BEFORE the cut so a capped answer structurally
// cannot report itself complete. New bounded tools call these rather than
// writing a seventeenth copy.

/// JSON Schema fragment for an `intent` parameter, with the `enum` GENERATED
/// from `QueryIntent::from_str` rather than restated beside it.
///
/// nw-317 leg 2. Two schemas hand-listed four values while the parser accepted
/// fourteen, and a third (`code_context`) declared no enum at all — so
/// `--intent blast-radius`, which `brain context --help` documents and the
/// direct route accepts, was rejected through the daemon with a raw
/// JSON-Schema error naming an internal MCP tool, while the same string was
/// valid on a sibling tool. Restating an enumeration is the same anti-pattern
/// the `CACHEABLE_TOOLS` decoration loop was invented to prevent.
fn intent_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "enum": nestweaver_store::ranking::QueryIntent::accepted_spellings(),
        "description": description,
    })
}

/// The upper bound every list-returning tool in this catalogue declares.
///
/// Not a new number: `brain_impact`, `detect_changes`, `dead_code`,
/// `hub_nodes` and `bridge_nodes` already declare exactly this. A second
/// ceiling for the params that lacked one would be the drift this seam exists
/// to stop.
pub(crate) const RESULT_LIMIT_MAX: usize = 1000;

/// JSON Schema fragment for a limit-shaped integer parameter.
///
/// `minimum` is what turns a negative into a REJECTION. Without it
/// `as_u64()` returns `None` for `-1`, `unwrap_or_else` fires, and the
/// caller's explicit `-1` silently becomes the default — the caller is told
/// nothing and gets a confident answer to a request they did not make.
fn limit_schema(description: &str, default: usize, min: usize, max: usize) -> Value {
    let mut schema = bounded_integer_schema(description, min, max);
    schema["default"] = json!(default);
    schema
}

/// The same bounds WITHOUT a `default` key, for a parameter whose omitted
/// behaviour is not a number.
///
/// `blast_radius.limit` is the case: omitting it means "the full set", so
/// advertising any default would be a claim the handler does not honour — the
/// same dishonesty `clusters.resolution` and `project_context.token_budget`
/// were corrected for.
fn bounded_integer_schema(description: &str, min: usize, max: usize) -> Value {
    json!({
        "type": "integer",
        "minimum": min,
        "maximum": max,
        "description": description,
    })
}

/// Read a limit-shaped argument, REJECTING an out-of-range value rather than
/// silently substituting the default.
///
/// Parses with `as_i64`, not `as_u64`: `as_u64` collapses "absent",
/// "negative" and "not an integer" into one `None`, which the caller then
/// converts into a confident default. The value the caller actually sent is
/// examined here.
///
/// Schema validation already rejects out-of-range values on the MCP path, but
/// `dispatch` is also reached from routes that never validate against the
/// schema, so the bound is enforced in both places on purpose.
fn read_limit(
    args: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, anyhow::Error> {
    let Some(raw) = args.get(key) else {
        return Ok(default);
    };
    if raw.is_null() {
        return Ok(default);
    }
    let value = raw
        .as_i64()
        .ok_or_else(|| anyhow!("invalid '{key}': expected an integer, got {raw}"))?;
    let min_i64 = i64::try_from(min).unwrap_or(i64::MAX);
    let max_i64 = i64::try_from(max).unwrap_or(i64::MAX);
    if value < min_i64 || value > max_i64 {
        anyhow::bail!("invalid '{key}': {value} is out of range (expected {min}..={max})");
    }
    Ok(usize::try_from(value).unwrap_or(default))
}

/// A list plus the two facts that make it honest: how many matched, and
/// whether the caller is looking at all of them.
///
/// `total` is captured at construction, BEFORE the cut. That ordering is the
/// whole point — every instance of this defect class in the codebase came
/// from computing `total` on an already-truncated vector.
pub(crate) struct Bounded<T> {
    items: Vec<T>,
    total: usize,
}

impl<T> Bounded<T> {
    /// Cut `items` to `limit`, capturing the pre-cap total.
    ///
    /// `limit == 0` means unlimited, matching the CLI's documented
    /// `--limit 0 = all` convention. A tool that does not offer that escape
    /// hatch declares `minimum: 1` and never passes 0 here.
    fn take(items: Vec<T>, limit: usize) -> Self {
        Self::window(items, 0, limit)
    }

    /// Cut `items` to the window `[offset, offset + limit)`, capturing the
    /// PRE-WINDOW total.
    ///
    /// nw-341: `take` can only ever return the HEAD of a list. When the
    /// ordering deliberately puts the rows a reviewer must inspect at the TAIL
    /// -- as `broken_wikilinks` does, sorting unresolved-first then by
    /// ASCENDING confidence so the most severe rows come first (nw-297) -- the
    /// head is precisely the wrong page and there is no second one. A health
    /// tool that cannot be used to verify its own fixes.
    ///
    /// `total` stays PRE-offset for the same reason it stays pre-cap: it
    /// answers "how many matched", not "how many are left". Reporting the
    /// remainder would make a caller's page arithmetic drift with every step.
    fn window(mut items: Vec<T>, offset: usize, limit: usize) -> Self {
        let total = items.len();
        items.drain(..offset.min(total));
        if limit != 0 && items.len() > limit {
            items.truncate(limit);
        }
        Self { items, total }
    }

    fn total(&self) -> usize {
        self.total
    }

    fn returned(&self) -> usize {
        self.items.len()
    }

    fn truncated(&self) -> bool {
        self.items.len() < self.total
    }

    /// Render only what survived the cut. Rendering before the cut is the
    /// other half of this defect class: work proportional to the corpus for
    /// an answer bounded to the limit.
    fn map<U>(self, f: impl FnMut(T) -> U) -> Bounded<U> {
        Bounded {
            items: self.items.into_iter().map(f).collect(),
            total: self.total,
        }
    }
}

impl Bounded<Value> {
    /// Merge the canonical disclosure triple into an object payload:
    /// `{<key>: [...], returned, total, truncated}`.
    ///
    /// `returned`/`total` are the spellings 8.0.0 standardised `brain_impact`
    /// and `brain_search` on; every new bounded list uses them so an agent
    /// parses one shape rather than five. Merging rather than constructing,
    /// because every tool in this catalogue carries additional top-level keys
    /// beside its list.
    fn merge_into(self, target: &mut Value, key: &str) {
        let (returned, total, truncated) = (self.returned(), self.total(), self.truncated());
        let Some(object) = target.as_object_mut() else {
            return;
        };
        object.insert(key.to_string(), Value::Array(self.items));
        object.insert("returned".to_string(), json!(returned));
        object.insert("total".to_string(), json!(total));
        object.insert("truncated".to_string(), json!(truncated));
    }
}

#[cfg(test)]
mod bounds_seam_tests {
    use super::*;

    #[test]
    fn total_is_captured_before_the_cut() {
        let bounded = Bounded::take((0..120).collect::<Vec<i32>>(), 5);
        assert_eq!(bounded.returned(), 5);
        assert_eq!(bounded.total(), 120, "total must count what MATCHED");
        assert!(bounded.truncated());
    }

    #[test]
    fn a_zero_limit_means_unlimited() {
        let bounded = Bounded::take((0..7).collect::<Vec<i32>>(), 0);
        assert_eq!(bounded.returned(), 7);
        assert!(!bounded.truncated());
    }

    #[test]
    fn merge_into_emits_one_canonical_shape() {
        let mut payload = json!({ "unrelated": true });
        Bounded::take(vec![json!("a"), json!("b"), json!("c")], 2).merge_into(&mut payload, "rows");
        assert_eq!(payload["rows"].as_array().unwrap().len(), 2);
        assert_eq!(payload["returned"], json!(2));
        assert_eq!(payload["total"], json!(3));
        assert_eq!(payload["truncated"], json!(true));
    }

    #[test]
    fn a_negative_limit_is_rejected_rather_than_silently_defaulted() {
        let error = read_limit(&json!({ "limit": -1 }), "limit", 50, 1, RESULT_LIMIT_MAX)
            .expect_err("-1 must not become 50");
        assert!(
            format!("{error}").contains("out of range"),
            "the rejection must name the violated bound: {error}"
        );
    }

    #[test]
    fn an_absent_limit_takes_the_documented_default() {
        assert_eq!(
            read_limit(&json!({}), "limit", 50, 1, RESULT_LIMIT_MAX).unwrap(),
            50
        );
    }

    #[test]
    fn an_over_ceiling_limit_is_rejected() {
        assert!(
            read_limit(
                &json!({ "limit": 999_999_999 }),
                "limit",
                50,
                1,
                RESULT_LIMIT_MAX
            )
            .is_err()
        );
    }

    #[test]
    fn the_schema_fragment_and_the_parser_share_their_bounds() {
        let schema = limit_schema("Max rows", 50, 1, RESULT_LIMIT_MAX);
        let min = schema["minimum"].as_i64().unwrap();
        let max = schema["maximum"].as_i64().unwrap();
        // Whatever the declaration rejects, the parser must reject too.
        for out_of_range in [min - 1, max + 1] {
            assert!(
                read_limit(
                    &json!({ "limit": out_of_range }),
                    "limit",
                    50,
                    1,
                    RESULT_LIMIT_MAX
                )
                .is_err(),
                "{out_of_range} is outside the declared bounds but the parser accepted it"
            );
        }
    }
}

fn configured_index_limits() -> nestweaver_engine::index_limits::IndexLimits {
    current_instance_config()
        .map(|config| config.indexing.limits())
        .unwrap_or_default()
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

pub fn set_direct_read_only(direct: bool) {
    DIRECT_READ_ONLY.with(|value| value.set(direct));
}

pub fn is_direct_read_only() -> bool {
    DIRECT_READ_ONLY.with(|value| value.get())
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
                });
                if let Some(location) = &result.location {
                    item["location"] = json!(location);
                }
                if let Some(body) = &result.inline_body {
                    item["inline_body"] = json!(body);
                }
                item
            };
            // Parity with the local path: symbol rows carry no
            // `matched_headings` key at all — omit it when empty instead of
            // emitting a spurious `[]`.
            if !result.matched_headings.is_empty() {
                item["matched_headings"] = json!(result.matched_headings);
            }
            if let Some(canonical_id) = &result.canonical_id {
                item["canonical_id"] = json!(canonical_id);
            }
            // Parity with the local path: note rows carry their vault.
            if let Some(vault_uid) = &result.vault_uid {
                item["vault_uid"] = json!(vault_uid);
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
        "semantic_applied": response.semantic_applied,
        "degraded_components": &response.degraded_components,
    });
    if !response.expansion_terms.is_empty() {
        value["expansion_terms"] = json!(response.expansion_terms);
    }
    value
}

/// Map a tonic Status from a daemon tool RPC into an anyhow error. The
/// daemon's dispatch layer reports tool-EXECUTION failures as
/// `Status::internal("tool <name> failed: ...")` and cancellations as
/// `"<name> query cancelled: ..."` — those are tool errors, not transport
/// failures, so surfacing them under a "gRPC error:" prefix misleads clients
/// (stdio MCP clients never speak gRPC). Only genuine transport/RPC failures
/// keep the prefix.
#[cfg(feature = "daemon")]
fn grpc_status_err(status: tonic::Status) -> anyhow::Error {
    let message = status.message();
    if message.starts_with("tool ") || message.contains(" query cancelled:") {
        anyhow::anyhow!("{message}")
    } else {
        anyhow::anyhow!("gRPC error: {message}")
    }
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
    dispatch_via_daemon_inner(client, rt, name, args).map(provenance_seam::stamp)
}

/// The daemon-route tool table. Returns [`Unstamped`] so that no arm — the
/// typed-proto rebuilds, the hand-built `json!` early returns, or the generic
/// `result_json` pass-through — can reach a caller without crossing the
/// provenance seam. See [`provenance_seam`].
#[cfg(feature = "daemon")]
fn dispatch_via_daemon_inner(
    client: &mut DaemonGrpcClient,
    rt: &tokio::runtime::Runtime,
    name: &str,
    args: serde_json::Value,
) -> Result<Unstamped, anyhow::Error> {
    use nestweaver_proto::JsonRequest;

    // The daemon-proxy path must enforce the same --tools/--lite gate
    // as the local path, before any RPC is proxied.
    enforce_tool_allowed(name)?;

    validate_tool_arguments(name, &args)?;

    let args_json = serde_json::to_string(&args)?;

    // brain_add_source is special: it maps to IndexRepo or IndexVault
    // (streaming RPCs) depending on the path content.
    if name == "brain_add_source" {
        return dispatch_add_source_via_daemon(client, rt, args).map(Unstamped::new);
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
            return Ok(Unstamped::new(json!({
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
            })));
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
            return Ok(Unstamped::new(json!({
                "kind": "vault",
                "name": vault.name.clone(),
                "uid": vault.uid,
                "notes_deleted": inner.notes_deleted,
                "committed": inner.committed,
                "reconciliation_warnings": reconciliation_warnings(&inner.reconciliation_failures),
            })));
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
        return Ok(Unstamped::new(json!({
            "removed_repos": inner.removed_repos,
            "removed_vaults": inner.removed_vaults
        })));
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
                let resp = client.search(req).await.map_err(grpc_status_err)?;
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
                let resp = client.get_context(req).await.map_err(grpc_status_err)?;
                let inner = resp.into_inner();
                let mut value: Value = serde_json::from_str(&inner.result_json)
                    .unwrap_or(Value::String(inner.result_json));
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "semantic_applied".to_string(),
                        json!(inner.semantic_applied),
                    );
                    object.insert(
                        "degraded_components".to_string(),
                        json!(inner.degraded_components),
                    );
                }
                Ok(serde_json::to_string(&value)?)
            }
            "project_context" => {
                use nestweaver_proto::ProjectContextRequest;
                let req = tonic::Request::new(ProjectContextRequest {
                    project: str_field("project"),
                    token_budget: i32_field("token_budget"),
                    kinds: str_array("kinds"),
                    // nw-316: absence must survive to the tool, which is the
                    // one place the default is documented.
                    include_components: args
                        .get("include_components")
                        .and_then(|value| value.as_bool()),
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
                    .map_err(grpc_status_err)?;
                Ok(resp.into_inner().result_json)
            }
            "note_get" => {
                use nestweaver_proto::NoteGetRequest;
                let req = tonic::Request::new(NoteGetRequest {
                    uid: opt_str_field("uid"),
                    title: opt_str_field("title"),
                    // nw-316: preserve absence; see `include_components`.
                    include_body: args.get("include_body").and_then(|value| value.as_bool()),
                    sections: str_array("sections"),
                });
                let resp = client.get_note(req).await.map_err(grpc_status_err)?;
                let inner = resp.into_inner();
                let mut value = serde_json::json!({
                    "uid": inner.uid,
                    "title": inner.title,
                    "path": inner.path,
                    "note_kind": inner.note_kind,
                    "word_count": inner.word_count,
                    "section_count": inner.section_count,
                    // Parity with the local path: frontmatter and outline are
                    // always present (local defaults to {} / []).
                    "frontmatter": serde_json::from_str::<serde_json::Value>(
                        &inner.frontmatter_json
                    )
                    .unwrap_or_else(|_| serde_json::json!({})),
                    "outline": inner
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
                    .map_err(grpc_status_err)?;
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
                let resp = client.hub_nodes(req).await.map_err(grpc_status_err)?;
                Ok(resp.into_inner().result_json)
            }
            // `compact_embeddings` is a typed RPC, not a `JsonRequest`, so the
            // generic arm below cannot carry it — and it was absent from this
            // table entirely. The tool is advertised by `tool_list` and answers
            // on the direct route (`tool_compact_embeddings` dials a daemon of
            // its own), but over MCP with a daemon running — the default — it
            // fell through to `unknown` and failed.
            //
            // This is the THIRD list to drift the same way. nw-232 fixed the
            // federation router and the hybrid stdio server after
            // `compact_embeddings` went missing from both; nothing then checked
            // this table, because nothing enumerated it. See
            // `every_registered_tool_routes_to_a_real_arm_on_the_daemon_seam`,
            // which found this one.
            "compact_embeddings" => {
                let req = tonic::Request::new(nestweaver_proto::CompactEmbeddingsRequest {
                    dry_run: args
                        .get("dry_run")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                });
                let resp = client
                    .compact_embeddings(req)
                    .await
                    .map_err(grpc_status_err)?;
                let inner = resp.into_inner();
                Ok(serde_json::to_string(&json!({
                    "dry_run": inner.dry_run,
                    "reclaimed": inner.reclaimed,
                    "live": inner.live_after,
                    "stored_before": inner.stored_before,
                    "stored_after": inner.stored_after,
                    "tombstoned_before": inner.tombstoned_before,
                    "bytes_before": inner.bytes_before,
                    "bytes_after": inner.bytes_after
                }))?)
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
                    "code_context" => client.code_context(req).await,
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
                let resp = resp.map_err(grpc_status_err)?;
                Ok(resp.into_inner().result_json)
            }
        }
    })?;

    serde_json::from_str(&result_json)
        .map(Unstamped::new)
        .map_err(Into::into)
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

#[cfg(all(test, feature = "daemon"))]
async fn consume_daemon_index_progress<S>(
    source: DaemonIndexSource,
    mut stream: S,
) -> Result<String, anyhow::Error>
where
    S: tokio_stream::Stream<Item = Result<nestweaver_proto::IndexProgress, tonic::Status>> + Unpin,
{
    Ok(consume_daemon_index_progress_detailed(source, &mut stream)
        .await?
        .0)
}

#[cfg(feature = "daemon")]
async fn consume_daemon_index_progress_detailed<S>(
    source: DaemonIndexSource,
    mut stream: S,
) -> Result<(String, Option<nestweaver_proto::IndexProgress>), anyhow::Error>
where
    S: tokio_stream::Stream<Item = Result<nestweaver_proto::IndexProgress, tonic::Status>> + Unpin,
{
    let mut terminal = None;
    let message = nestweaver_proto::consume_index_progress(&mut stream, |progress| {
        if progress.phase == nestweaver_proto::Phase::Done as i32 {
            terminal = Some(progress.clone());
        }
    })
    .await
    .map_err(|error| anyhow::anyhow!("{} index failed: {error}", source.label()))?;
    Ok((message, terminal))
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

/// Instance id `brain_add_source` stamps vaults under when routing through the
/// daemon, mirroring the CLI's nw-019 precedence (`resolve_instance_id`):
/// config's `instance_id` > `"default"`. An empty id would defer to the daemon,
/// whose config-less fallback is the db-path hash — a different identity than
/// the CLI's `"default"`, duplicating any vault added via both paths.
#[cfg(feature = "daemon")]
/// The instance this MCP process should ask the daemon to write under.
///
/// nw-207: returns EMPTY when this process has no config of its own, which is
/// the protocol's "you decide" sentinel — the daemon then uses its own
/// configured identity. It used to return the literal "default", which is a
/// FALLBACK masquerading as a choice: an MCP server started without `--config`
/// would override a daemon configured as `kory-brain` and create the vault
/// under `default`, splitting the graph.
///
/// Only a config this process actually has is an instruction worth sending.
fn resolve_add_source_instance_id() -> String {
    current_instance_config()
        .map(|c| c.instance_id.clone())
        .unwrap_or_default()
}

#[cfg(feature = "daemon")]
fn dispatch_add_source_via_daemon(
    client: &mut DaemonGrpcClient,
    rt: &tokio::runtime::Runtime,
    args: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let raw_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("'path' is required for brain_add_source"))?;
    let path = resolve_add_source_path(raw_path)?
        .to_string_lossy()
        .into_owned();

    // Vault schema promises "Defaults to the directory name" — resolve it
    // here, but ONLY for vaults: forwarding "" blanks a vault's stored
    // friendly name on every nameless re-add (the daemon treats the value
    // literally), while for code repos an empty name is meaningful — the
    // daemon derives the repo name (package/remote) and a directory-name
    // default would override that derivation.
    let name_arg = args.get("name").and_then(|v| v.as_str()).map(String::from);

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

    // Match the CLI's nw-019 precedence (`resolve_instance_id`): config's
    // `instance_id` > "default". The old `unwrap_or_default` sent an empty id,
    // which a config-less daemon resolves to its db-path hash — so the same
    // vault added once via `brain add` ("default") and once via MCP (hash) was
    // duplicated under two vault UIDs.
    let instance_id = resolve_add_source_instance_id();

    rt.block_on(async {
        if is_vault {
            let vault_name = name_arg.clone().or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            });
            let req = tonic::Request::new(nestweaver_proto::IndexVaultRequest {
                vault_path: path.clone(),
                vault_name: vault_name.unwrap_or_default(),
                extra_ignore_patterns: vec![],
                instance_id: instance_id.clone(),
            });
            let stream = client
                .index_vault(req)
                .await
                .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?
                .into_inner();
            let (last_msg, terminal) =
                consume_daemon_index_progress_detailed(DaemonIndexSource::Vault, stream).await?;
            Ok(serde_json::json!({
                "status": "indexed",
                "path": path,
                "type": "vault",
                "message": last_msg,
                "coverage_status": terminal.as_ref().map(|progress| if progress.coverage_status == nestweaver_proto::CoverageStatus::Degraded as i32 { "degraded" } else { "complete" }),
                "skipped_count": terminal.as_ref().map_or(0, |progress| progress.skipped_count),
                "skipped_files": terminal.as_ref().map(|progress| progress.skipped_files.iter().map(|file| serde_json::json!({
                    "path": file.path,
                    "reason_code": file.reason_code,
                    "detail": file.detail,
                    "observed_bytes": file.observed_bytes,
                    "limit_bytes": file.limit_bytes,
                })).collect::<Vec<_>>()).unwrap_or_default(),
            }))
        } else {
            let req = tonic::Request::new(nestweaver_proto::IndexRepoRequest {
                repo_path: path.clone(),
                // Empty when omitted: the daemon derives the repo name
                // (package/remote) — a directory-name default would override
                // that derivation.
                name: name_arg.clone().unwrap_or_default(),
                force: false,
                with_trigrams: false,
                with_git_activity: false,
                rebuild_trigrams: false,
                // Inherit the daemon's `[indexing] with_trigrams`. This path has
                // no flags to express a policy, so asserting "off" here would
                // discard the operator's configuration exactly as the configless
                // CLI path did.
                trigram_policy: nestweaver_proto::TrigramPolicy::Unspecified as i32,
                max_source_file_bytes: current_instance_config()
                    .map(|config| config.indexing.limits().max_source_file_bytes())
                    .unwrap_or(0),
                // nw-019: no explicit instance here — let the daemon decide
                // (config's logical name, else runtime hash).
                instance_id: String::new(),
            });
            let stream = client
                .index_repo(req)
                .await
                .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?
                .into_inner();
            let (last_msg, terminal) =
                consume_daemon_index_progress_detailed(DaemonIndexSource::Repo, stream).await?;
            Ok(serde_json::json!({
                "status": "indexed",
                "path": path,
                "type": "repo",
                "message": last_msg,
                "coverage_status": terminal.as_ref().map(|progress| if progress.coverage_status == nestweaver_proto::CoverageStatus::Degraded as i32 { "degraded" } else { "complete" }),
                "skipped_count": terminal.as_ref().map_or(0, |progress| progress.skipped_count),
                "skipped_files": terminal.as_ref().map(|progress| progress.skipped_files.iter().map(|file| serde_json::json!({
                    "path": file.path,
                    "reason_code": file.reason_code,
                    "detail": file.detail,
                    "observed_bytes": file.observed_bytes,
                    "limit_bytes": file.limit_bytes,
                })).collect::<Vec<_>>()).unwrap_or_default(),
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

    #[test]
    fn add_source_sends_its_own_config_or_defers_to_the_daemon() {
        // The MCP daemon path must land vaults under the SAME instance the CLI
        // would resolve. nw-019 achieved that by sending the literal "default"
        // when this process had no config, because a config-less daemon's data
        // identity was its DB-PATH HASH and an empty id would have duplicated
        // the vault under two UIDs.
        //
        // nw-207 removed that hash: a config-less daemon's data identity is now
        // "default" too. So the compensation is not merely unnecessary, it was
        // actively harmful — a literal "default" OVERRODE a daemon that had a
        // configured instance, creating the vault under `default` and splitting
        // the graph. Sending EMPTY defers to the daemon, which is the only
        // party that knows its own data identity.
        set_current_instance_config(None);
        assert_eq!(
            resolve_add_source_instance_id(),
            "",
            "with no config of its own, this process must defer rather than \
             assert a default it merely fell back to"
        );

        let cfg: nestweaver_engine::InstanceConfig = serde_json::from_value(serde_json::json!({
            "instance_id": "cfg-instance",
            "repos": [],
            "snapshot_storage": { "backend": "local", "path": "/tmp" },
            "workspace": { "backend": "local", "path": "/tmp" },
            "inference": { "endpoint": "", "embedding_model": "", "summary_model": "" },
            "git": { "credential_method": "ssh" }
        }))
        .expect("valid test config");
        // A config this process ACTUALLY HAS is an instruction worth sending,
        // and still is.
        set_current_instance_config(Some(std::sync::Arc::new(cfg)));
        assert_eq!(resolve_add_source_instance_id(), "cfg-instance");
        set_current_instance_config(None);
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
        "description": "Orient on an unfamiliar topic in ONE call: runs hybrid PPR+BM25 retrieval, groups results into architectural domains, inlines high-confidence source bodies, and returns a token-budgeted map with a bundle_id for drill-down.\n\nGuidelines:\n- Use scope 'project:<slug>' or 'repo:<name>' to restrict; omit for unrestricted\n- Entries with is_seed: true are direct query/seed hits and are listed first; the rest are graph-connected neighbors\n- Drill into entries with investigate_expand (by asset_id) or fill all bodies with investigate_hydrate\n- more_available counts entries dropped by token budget — raise token_budget to see them\n\nLimitations:\n- Token budget hard-capped at 16000\n- Bundles expire 24h after creation",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The topic/feature/subsystem to orient on (e.g. \"device pairing\", \"how indexing works\")." },
                "scope": { "type": "string", "description": "Optional scope. \"project:<slug>\" and \"repo:<name>\" genuinely restrict results; \"vault\" and \"all\" are PASS-THROUGHS that restrict nothing (default: \"all\"). Read `scope_filtered` in the response to know whether a filter was actually applied — `scope` only echoes what you asked for." },
                "token_budget": { "type": "integer", "minimum": 1, "maximum": 16000, "default": 4000, "description": "Approximate token cap for the map (chars/4). Hard-capped at 16000." },
                "root": { "type": "string", "description": "Filesystem root for reading inline source bodies. Defaults to the server's working directory." }
            },
            "required": ["query"],
            "additionalProperties": false
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
    // nw-189: "all", not "vault". The result echoes this string back, and
    // defaulting to "vault" told an agent that supplied NO scope that its
    // results were vault-scoped while code symbols from every repo came back.
    // Both are documented pass-throughs; "all" is the honest name for the
    // default. `scope_filtered` in the response reports whether a filter was
    // actually applied.
    let scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or("all");
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
            "additionalProperties": false,
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
            "additionalProperties": false,
            "properties": {
                "bundle_id": { "type": "string", "description": "The bundle_id returned by a prior `investigate` call." },
                "token_budget": { "type": "integer", "minimum": 1, "maximum": 16000, "default": 4000, "description": "Approximate token cap for the hydrated bodies (chars/4). Hard-capped at 16000." },
                "root": { "type": "string", "description": "Filesystem root for reading source bodies. Defaults to the server's working directory." }
            },
            "required": ["bundle_id"]
        }
    })
}

fn tool_investigate_hydrate(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // hydrate is the BULK operation — it hydrates every un-hydrated entry in the
    // bundle and takes no per-entry selector. A caller passing `targets`/`target`/
    // `uid`/`uids` has confused it with investigate_expand; those keys were silently
    // ignored (a no-op that reads as "nothing to hydrate"), so reject them with a
    // pointer instead, matching investigate_expand's own strictness.
    for key in ["targets", "target", "uid", "uids"] {
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

/// The nw-103 resolver-staleness disclosure, for tools whose numbers it invalidates.
///
/// The CLI has warned about this at four call sites since nw-103. The MCP
/// surface warned nowhere — so an AGENT asking for hub or bridge rankings got
/// confidently-wrong numbers with nothing to indicate it, while a human running
/// the same query through the CLI was told. The rankings are computed over
/// edges the import-fan-out fix could not repair on disk; upgrading does not
/// correct them, only re-indexing does.
///
/// Kubernetes shipped this exact bug and fixed it the same way in 1.19: the
/// warning moved out of kubectl and into the API layer, because the CLI is not
/// the only caller.
///
/// This belongs in the RESULT, never in `_meta`. `_meta` is client/UI-facing
/// and is typically hidden from the model, so a disclosure placed there would
/// be invisible to precisely the reader that needs it. It is a
/// natural-language sentence because the model reads text.
fn ranking_staleness_note(store: &GraphStore) -> Option<String> {
    let db_path = current_db_path(store).ok()?;
    let repos = store.list_repos(None).ok()?;
    let uids: Vec<String> = repos.into_iter().map(|repo| repo.uid).collect();
    if uids.is_empty() {
        return None;
    }
    nestweaver_engine::resolver_generation::staleness_note(&db_path, &uids)
}

/// Attach a disclosure to a tool result without dropping one already there.
///
/// `note` is this surface's existing human-readable channel and some tools
/// already set it for a different reason (clustering absent). Overwriting it
/// would trade one silent omission for another, so applicable notes accumulate.
fn attach_note(resp: &mut Value, note: String) {
    let merged = match resp.get("note").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{existing} {note}"),
        _ => note,
    };
    resp["note"] = json!(merged);
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
    use nestweaver_schema::{
        Note, NoteKind, Project, Section, Symbol, SymbolKind, Vault, Visibility,
    };

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
            frontmatter_raw: None,
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
    /// nw-305 / F-VAULT-6 — CHARACTERISATION, not a fix.
    ///
    /// `project_context` seeds a PPR walk from the project and its members,
    /// multiplies member relevance by 5, sorts, and then fills the token budget
    /// from whatever the walk reached. Every scope filter — `kinds`, `repos`,
    /// `path_prefix`, `tags`, `exclude_tags`, `since` — is opt-in from `args`
    /// and defaults to off, so NOTHING narrows the result back to the project.
    /// This is a designed absence, not a scoring bug or a broken filter: the
    /// boost is working (the members are ranked first), nothing removes the
    /// rest.
    ///
    /// The test pins that behaviour deliberately rather than asserting "no
    /// foreign notes", which would encode a design decision nobody has made —
    /// and the two candidate fixes (disclose `in_project` / add a
    /// `scope: "strict"` argument) want different assertions here.
    ///
    /// It also runs the measurement the ticket asks for. The ticket's framing
    /// is "degrades when a project has FEW notes". The model that fits the code
    /// is budget-driven: foreign nodes appear iff the budget buys more slots
    /// than the project's own reachable mass fills, which makes the leak
    /// UNIVERSAL rather than small-project-specific — every project hits it
    /// once the budget outgrows it. Raising the budget on a fixed fixture
    /// discriminates the two: under the budget model the foreign count rises,
    /// under a similarity model it does not.
    #[test]
    fn project_context_fills_budget_from_outside_the_project() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_vault(&Vault {
                uid: "vlt:t".into(),
                name: "t".into(),
                root_path: "/v".into(),
                instance_id: "default".into(),
            })
            .unwrap();
        store
            .insert_project(&Project {
                uid: "proj:alpha".into(),
                name: "Alpha".into(),
                summary: None,
                instance_id: "default".into(),
            })
            .unwrap();

        // Alpha has exactly two notes, like Carson Elevator.
        let members = ["note:a1", "note:a2"];
        for (i, uid) in members.iter().enumerate() {
            store
                .insert_note(&mk_note(
                    uid,
                    "vlt:t",
                    &format!("Workspaces/Alpha/doc{i}.md"),
                    &format!("Alpha Doc {i}"),
                ))
                .unwrap();
        }
        store
            .batch_insert_project_note_edges(
                &members
                    .iter()
                    .map(|m| ("proj:alpha", *m))
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        // Ten unrelated notes belonging to nobody, reachable from Alpha's notes
        // through ordinary wikilinks — exactly how the PPR walk leaves a small
        // project's subgraph on the real vault.
        let foreign: Vec<String> = (0..10).map(|i| format!("note:f{i}")).collect();
        for (i, uid) in foreign.iter().enumerate() {
            store
                .insert_note(&mk_note(
                    uid,
                    "vlt:t",
                    &format!("Workspaces/Bravo/other{i}.md"),
                    &format!("Bravo Doc {i}"),
                ))
                .unwrap();
        }
        for (i, member) in members.iter().enumerate() {
            let section_uid = format!("sec:{member}");
            store
                .insert_section(&Section {
                    uid: section_uid.clone(),
                    note_uid: (*member).to_string(),
                    heading_uid: None,
                    start_line: 1,
                    end_line: 2,
                    text_hash: format!("th-{i}"),
                    text_content: "links out".to_string(),
                    word_count: 2,
                    pagerank_score: None,
                })
                .unwrap();
            store
                .batch_insert_note_section_edges(&[(member, section_uid.as_str())])
                .unwrap();
            let edges: Vec<(&str, &str, f32, &str, &str)> = foreign
                .iter()
                .map(|f| (section_uid.as_str(), f.as_str(), 1.0_f32, "link", "link"))
                .collect();
            store.batch_insert_wikilink_to_note_edges(&edges).unwrap();
        }

        let member_set: std::collections::HashSet<&str> = members.iter().copied().collect();
        let foreign_count = |budget: u64| -> usize {
            let resp = tool_project_context(
                &store,
                None,
                json!({
                    "project": "Alpha",
                    "token_budget": budget,
                    "response_format": "detailed"
                }),
                None,
                None,
            )
            .unwrap();
            resp["connected"]
                .as_array()
                .expect("connected array")
                .iter()
                .filter_map(|n| n["uid"].as_str())
                .filter(|uid| !member_set.contains(uid) && uid.starts_with("note:f"))
                .count()
        };

        // The measured sweep on this fixture, which is the ticket's open
        // question answered: foreign notes per token_budget =
        //   100 -> 0, 200 -> 0, 400 -> 6, 800 -> 10, 1600 -> 10, 3200 -> 10,
        //   16000 -> 10
        // Zero while the budget is smaller than the project's own mass, then
        // monotonically rising, then flat once the walk runs out of reachable
        // foreign notes. That is a budget-fill signature, not a similarity one:
        // no PPR-weight change can fix it, and it is not specific to small
        // projects — it is what every project does once `token_budget` exceeds
        // its in-project reachable mass.
        let tight = foreign_count(200);
        let roomy = foreign_count(800);

        assert_eq!(
            tight, 0,
            "characterisation: a budget smaller than the project's own mass \
             leaks nothing — the members fill it first"
        );
        assert!(
            roomy > tight,
            "characterisation: with no scope filter on by default, the surplus \
             budget is filled from whatever the PPR walk reached, including \
             notes belonging to no project (nw-305). Measured {tight} foreign \
             at budget 200 and {roomy} at 800. Flip this assertion once the \
             owner picks between `in_project` disclosure and a `scope` argument."
        );
        assert!(
            foreign_count(16_000) >= roomy,
            "the leak is BUDGET-driven, not similarity-driven: the foreign \
             count must not SHRINK as the budget grows. If this ever inverts, \
             the ticket's 'small projects leak' framing is right after all and \
             the fix belongs in ranking rather than in scoping."
        );
    }

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
    /// Shared with the federated round-trip guard, which needs a real store to
    /// get real tool output.
    pub(super) fn index_on_disk_for_merge_guard() -> (tempfile::TempDir, std::path::PathBuf) {
        index_on_disk()
    }

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

    /// The smallest argument object a schema accepts, so the only thing under
    /// test is what the dispatch seam adds.
    fn smallest_valid_args(schema: &Value) -> Value {
        let mut args = serde_json::Map::new();
        let properties = schema["properties"].as_object();
        for required in schema["required"].as_array().into_iter().flatten() {
            let Some(field) = required.as_str() else {
                continue;
            };
            let declared = properties.and_then(|properties| properties.get(field));
            let kind = declared
                .and_then(|value| value["type"].as_str())
                .unwrap_or("string");
            let value = match kind {
                "array" => json!(["greet"]),
                "integer" | "number" => json!(1),
                "boolean" => json!(true),
                "object" => json!({}),
                _ => declared
                    .and_then(|value| value["enum"].as_array())
                    .and_then(|values| values.first().cloned())
                    .unwrap_or_else(|| json!("greet")),
            };
            args.insert(field.to_string(), value);
        }
        Value::Object(args)
    }

    /// nw-315. `_meta` (scope/sources/stale_repos) was never added on the MCP
    /// route — not dropped, never added. Four provenance authors existed and
    /// the stdio server was not one of them: `src/main.rs` for the CLI direct
    /// route, the federation client for the CLI daemon route, `http.rs` under a
    /// third, namespaced spelling, and stdio nothing — while
    /// `SERVER_INSTRUCTIONS` promises the agent that "Results include
    /// `_meta.sources` indicating which data sources contributed". The server
    /// documented a field it did not send.
    ///
    /// WHERE ELSE DOES THIS PROPERTY NEED TO HOLD? The report named ten tools.
    /// Asserting on those ten would have re-created the defect the moment an
    /// eleventh tool was registered, because the real problem is that
    /// provenance was authored per-command instead of at the seam every route
    /// passes through. So this asserts over the REGISTRY: every registered
    /// read-only tool that answers at all must carry provenance.
    ///
    /// SCOPE, stated because this test's original name over-claimed it: the
    /// registry it iterates is the WIRE SURFACE, but the thing it exercises is
    /// `dispatch` — ONE of the two implementations behind that surface. It said
    /// "every tool that answers"; it proved "every tool that answers *through
    /// the in-process seam*". `dispatch_via_daemon` is a peer of `dispatch`,
    /// not a caller of it, and it stamped nothing — so `brain_search` over MCP
    /// returned no `_meta` while this test was green. The daemon seam cannot be
    /// executed here (a successful response needs a live daemon), so its half of
    /// the property is carried by the type system in `provenance_seam` and its
    /// registry COVERAGE by
    /// `every_registered_tool_routes_to_a_real_arm_on_the_daemon_seam` below.
    #[test]
    fn every_tool_that_answers_stamps_its_provenance() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let schemas = all_tool_schemas();
        let mut answered: Vec<String> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for tool in &schemas {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            // Mutating tools are excluded because exercising them here would
            // write to the fixture, not because provenance is optional for
            // them: they reach the same seam and inherit the same stamp.
            if crate::http::MUTATING_TOOLS.contains(&name) {
                continue;
            }
            let args = smallest_valid_args(&tool["inputSchema"]);
            let Ok(value) = dispatch(&store, None, name, args, None) else {
                // A tool that cannot answer on this two-symbol fixture proves
                // nothing either way; the ones that DO answer are the sample.
                continue;
            };
            // A bare array has nowhere to put a key. Those exist (legacy JSON
            // RPCs) and are a separate shape defect, not this one.
            if !value.is_object() {
                continue;
            }
            answered.push(name.to_string());
            if value["_meta"]["sources"].as_array().is_none() {
                missing.push(name.to_string());
            }
        }

        assert!(
            answered.len() >= 10,
            "only {} tools answered on the fixture — too small a sample to \
             prove anything: {answered:?}",
            answered.len()
        );
        assert!(
            missing.is_empty(),
            "these tools returned an object with no `_meta.sources`, while the \
             server's own `initialize` instructions tell the agent \"Results \
             include _meta.sources indicating which data sources contributed\": \
             {missing:?}"
        );
    }

    /// nw-315 follow-up. The sweep disproved "author result provenance once, at
    /// the tool layer" in both directions: `brain_search` over MCP returned NO
    /// `_meta` while `brain search --json` did, and CLI `hubs --json` had none
    /// while MCP `hub_nodes` did.
    ///
    /// The cause was a SECOND dispatch table — `dispatch_via_daemon`, used
    /// whenever a daemon is running, which is the default — that stamped
    /// nothing. Within it the behaviour split again by RPC shape: a typed proto
    /// (`BrainSearchResponse`, no `_meta` field) forced the client to rebuild a
    /// fresh object and lose the daemon's stamp, while a `result_json`
    /// pass-through (`hub_nodes`, `brain_doc_stats`) carried it through
    /// verbatim. Same table, opposite outcomes, no test on either.
    ///
    /// WHERE ELSE DOES THIS PROPERTY NEED TO HOLD? On every arm of that second
    /// table, for every tool the registry advertises. Two halves, because the
    /// seam cannot be executed without a daemon:
    ///
    /// 1. THAT IT STAMPS — carried by the compiler, not by this test.
    ///    `dispatch_via_daemon_inner` returns `provenance_seam::Unstamped`,
    ///    whose field is private to that module, and `stamp` is the only way to
    ///    get a `Value` back out. Every arm — typed rebuild, hand-built `json!`
    ///    early return, and the generic `result_json` pass-through — is covered
    ///    by construction, and an arm added later cannot forget.
    ///
    /// 2. THAT EVERY REGISTERED TOOL REACHES AN ARM — this test. The second
    ///    table ends in `unknown => Err("unknown tool for daemon dispatch")`,
    ///    so a tool present in the registry but absent from that table answers
    ///    on the direct route and fails on the daemon route. Dialling a dead
    ///    port makes any name that DID route fail with a transport error
    ///    instead, which is what separates "routed" from "not in the table" —
    ///    the same trick `daemon_proxy_enforces_tools_allowlist_and_lite_mode`
    ///    uses. Add tool #43 and forget the daemon table and this goes red.
    #[cfg(feature = "daemon")]
    #[test]
    fn every_registered_tool_routes_to_a_real_arm_on_the_daemon_seam() {
        reset_session();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let channel = {
            let _guard = runtime.enter();
            tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy()
        };
        let mut client = DaemonGrpcClient::new(channel);

        let schemas = all_tool_schemas();
        let mut routed = 0_usize;
        let mut unrouted: Vec<String> = Vec::new();
        for tool in &schemas {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            let args = smallest_valid_args(&tool["inputSchema"]);
            match dispatch_via_daemon(&mut client, &runtime, name, args) {
                // A dead port cannot answer, so a success here would mean the
                // arm never reached the wire. None currently do; if one starts
                // to, it is a locally-fabricated answer and must still stamp —
                // which `Unstamped` guarantees, so accept it as routed.
                Ok(value) => {
                    assert!(
                        value["_meta"]["sources"].as_array().is_some(),
                        "`{name}` answered on the daemon seam without provenance"
                    );
                    routed += 1;
                }
                Err(error) => {
                    let text = error.to_string();
                    if text.contains("unknown tool for daemon dispatch") {
                        unrouted.push(name.to_string());
                    } else {
                        routed += 1;
                    }
                }
            }
        }

        assert!(
            unrouted.is_empty(),
            "these tools are advertised by `tool_list` but fall through to the \
             `unknown` arm of `dispatch_via_daemon`, so they answer on the \
             direct route and fail whenever a daemon is running — which is the \
             default: {unrouted:?}"
        );
        // Vacuity guard: if the registry or the harness ever stops producing
        // calls, `unrouted.is_empty()` passes for the wrong reason.
        assert!(
            routed >= schemas.len(),
            "only {routed} of {} registered tools were exercised",
            schemas.len()
        );
    }

    /// The single arm the sweep caught, pinned end to end through the seam.
    ///
    /// `daemon_brain_search_response_to_json` is the client-side rebuild that
    /// loses the daemon's stamp: `BrainSearchResponse` is a typed proto with no
    /// `_meta` field, so the daemon's `dispatch_tool_json` stamp is discarded at
    /// the proto boundary and this function constructs a fresh object from the
    /// scalars. Asserted in two steps so a regression names its own cause:
    /// the rebuild really does arrive bare, and the seam really does fix it.
    #[cfg(feature = "daemon")]
    #[test]
    fn the_typed_proto_rebuild_arrives_bare_and_leaves_the_seam_stamped() {
        let response = nestweaver_proto::BrainSearchResponse::default();
        let rebuilt = daemon_brain_search_response_to_json(&response, false);
        assert!(
            rebuilt.get("_meta").is_none(),
            "the premise of this test is that the typed rebuild is bare; if it \
             stamps on its own, delete the seam wrapper rather than both"
        );
        let stamped = provenance_seam::stamp(Unstamped::new(rebuilt));
        assert_eq!(
            stamped["_meta"]["sources"],
            serde_json::json!([nestweaver_schema::provenance::SOURCE_LOCAL]),
            "the daemon seam must stamp what the proto boundary dropped"
        );
    }

    /// The `stale_check` half of nw-315: both CLI routes derived
    /// `stale_repos` / `needs_reindex_repos` from `repos` themselves — Route A
    /// by post-processing the daemon payload, Route B in a complete second
    /// implementation of the whole loop — so the agent was told "at least one
    /// repo needs re-indexing" and had to scan a 43-entry array to learn which.
    /// `needs_reindex_repos` is the field 8.0.0 added as a documented breaking
    /// change and it had never reached the MCP surface.
    #[test]
    fn stale_check_reports_the_two_summary_lists_itself() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let value = tool_stale_check(&store).unwrap();
        for field in ["stale_repos", "needs_reindex_repos"] {
            assert!(
                value[field].is_array(),
                "stale_check must author `{field}` itself; deriving it above the \
                 tool is what made it invisible to MCP: {value}"
            );
        }

        // And they must agree with the per-repo flags they summarise — the
        // three prior drifts in this pair were all disagreements of exactly
        // this kind.
        let names = |field: &str, flag: &str| {
            let summary: Vec<&str> = value[field]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            let derived: Vec<&str> = value["repos"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|repo| repo[flag].as_bool().unwrap_or(false))
                .filter_map(|repo| repo["url"].as_str())
                .collect();
            (summary, derived)
        };
        for (field, flag) in [
            ("stale_repos", "is_stale"),
            ("needs_reindex_repos", "needs_reindex"),
        ] {
            let (summary, derived) = names(field, flag);
            assert_eq!(
                summary, derived,
                "`{field}` disagrees with the `{flag}` flags it summarises: {value}"
            );
        }
    }

    /// H1: an upgrade must not serve a pre-upgrade response shape.
    ///
    /// Before the fix this test failed: the entry below satisfies the persisted
    /// `graph_generation`, the scope digest and the 24h TTL, so `brain_search`
    /// returned it verbatim (`hits=1 misses=0`) — without `semantic_applied` or
    /// `degraded_components`, exactly the "no semantic leg vs not implemented"
    /// ambiguity those fields exist to remove.
    #[test]
    fn pre_upgrade_response_shape_is_never_served() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let args = json!({ "query": "greet", "limit": 5 });
        // An entry exactly as the PREVIOUS binary would have left it on disk:
        // same key algorithm, same persisted generation, same scope digest,
        // written < 24h ago — but a response body without the fields the
        // CURRENT binary emits, and stamped with that binary's shape version.
        let key = nestweaver_store::cache::ResponseCache::key("brain_search", &args);
        let scope_digest = whole_db_scope_digest(&db_path);
        let old_shape = br#"{"query":"greet","engine":"bm25","results":[],"total_matches":0}"#;
        let mut cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
            RESPONSE_SHAPE_VERSION ^ 0xD1FF,
        );
        cache.insert(
            key,
            "brain_search",
            old_shape,
            store.graph_generation(),
            scope_digest,
        );
        cache.save();
        reset_session();

        let served = dispatch(&store, None, "brain_search", args, None).unwrap();
        assert_ne!(
            served,
            serde_json::from_slice::<Value>(old_shape).unwrap(),
            "the pre-upgrade entry must not be served verbatim"
        );
        assert_eq!(
            CACHE_HITS.with(|c| c.get()),
            0,
            "a foreign-shape entry must not count as a hit"
        );
        assert_eq!(CACHE_MISSES.with(|c| c.get()), 1);
        assert!(
            served.get("semantic_applied").is_some(),
            "the recomputed response must carry the current shape's fields"
        );
        assert!(served.get("degraded_components").is_some());
    }

    /// The companion to the test above: the guard must not be so blunt that it
    /// breaks caching. Same binary, same graph → still a hit.
    #[test]
    fn matching_shape_version_still_hits() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let args = json!({ "query": "greet", "limit": 5 });
        let _ = dispatch(&store, None, "brain_search", args.clone(), None).unwrap();
        flush_response_cache();
        RESPONSE_CACHE.with(|m| m.borrow_mut().clear());
        let second = dispatch(&store, None, "brain_search", args, None).unwrap();

        assert_eq!(
            CACHE_HITS.with(|c| c.get()),
            1,
            "an entry written by THIS binary must still hit after a reopen"
        );
        assert!(second.get("semantic_applied").is_some());
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
            RESPONSE_SHAPE_VERSION,
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
            RESPONSE_SHAPE_VERSION,
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
        // This test asserts CACHE behaviour with a permanently dirty marker.
        // The nw-C2 bounded wait would otherwise spend its whole budget twice
        // waiting for a publication that never completes; disabling it here
        // preserves the exact pre-nw-C2 dispatch path the test was written for.
        set_index_publication_wait_ms(0);
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        fs::write(
            nestweaver_engine::sidecar_path(&db_path, ".index-dirty"),
            b"dirty",
        )
        .unwrap();
        let store = GraphStore::open(&db_path).unwrap();
        // Dispatch a cacheable NON-ranking tool: hub_nodes (this test's
        // original dispatch) now fails closed during a dirty window under
        // the ranking.rs fail-closed contract, while the response-cache
        // bypass this test exists to pin applies to every cacheable tool.
        let args = json!({ "pattern": "fn" });

        let _ = dispatch(&store, None, "regex_search", args.clone(), None).unwrap();
        let _ = dispatch(&store, None, "regex_search", args, None).unwrap();
        flush_response_cache();

        assert_eq!(CACHE_HITS.with(|c| c.get()), 0);
        assert_eq!(CACHE_MISSES.with(|c| c.get()), 0);
        let cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
            RESPONSE_SHAPE_VERSION,
        );
        assert!(cache.is_empty(), "dirty responses must not be retained");
    }

    // ── code_context bounds: an omitted limit must not mean "everything" ──

    /// The 500-result safeguard was advertised for `code_context` and reachable
    /// by nobody: an omitted `limit` meant `usize::MAX` in the engine, so a
    /// caller escaped the cap by simply not sending the field.
    ///
    /// Tested on the truncation helper rather than through a contrived graph:
    /// what matters is that a capped result says it was capped, and that is a
    /// property of this function, not of any particular fixture's shape.
    #[test]
    fn truncation_is_reported_not_silent() {
        let mut over = vec![1, 2, 3, 4];
        assert!(
            truncate_reporting(&mut over, 3),
            "four rows capped at three IS a truncation"
        );
        assert_eq!(over, vec![1, 2, 3], "and the extra row is dropped");

        // The boundary: exactly at the limit is NOT truncated. Off-by-one here
        // would report every full-but-fitting result as lossy.
        let mut exact = vec![1, 2, 3];
        assert!(!truncate_reporting(&mut exact, 3));
        assert_eq!(exact, vec![1, 2, 3]);

        let mut under = vec![1];
        assert!(!truncate_reporting(&mut under, 3));
        assert_eq!(under, vec![1]);

        let mut empty: Vec<u8> = Vec::new();
        assert!(!truncate_reporting(&mut empty, 0));
    }

    /// The other half: when everything fits, `truncated` must be false. Without
    /// this, a field hardcoded to `true` would pass the test above.
    /// nw-320. `code_context` shipped in 8.0.0 with the defect its own
    /// contract claims to prevent: `connected_count` reports what was
    /// RETURNED, so it agrees with the item list by construction and a capped
    /// answer is indistinguishable from a complete one. The engine HAS the
    /// pre-cap number — its traversal stopped pushing once the cap was reached
    /// and discarded the knowledge in the same expression.
    ///
    /// The convention 8.0.0 corrected `brain_impact` and `brain_search` to is
    /// `returned: N, total: M, truncated: true`.
    #[test]
    fn code_context_reports_the_pre_cap_total() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        // One seed reaching five distinct callees, so a cap of 2 provably
        // drops rows the caller must be told about.
        fs::write(
            src.join("main.js"),
            "function greet(n){return a(n)+b(n)+c(n)+d(n)+e(n);}\n\
             function a(n){return n;}\nfunction b(n){return n;}\n\
             function c(n){return n;}\nfunction d(n){return n;}\n\
             function e(n){return n;}\n",
        )
        .unwrap();
        let db_path = dir.path().join("test.lbug");
        let repo_url = format!("file://{}", src.display());
        nestweaver_engine::index_directory(&src, &db_path, "test", &repo_url, "local").unwrap();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let capped = dispatch(
            &store,
            None,
            "code_context",
            json!({ "seeds": ["greet"], "limit": 2 }),
            None,
        )
        .unwrap();

        let returned = capped["connected"].as_array().unwrap().len();
        assert_eq!(returned, 2, "the cap must bite or this test proves nothing");
        assert_eq!(capped["truncated"], json!(true));
        let total = capped["total"]
            .as_u64()
            .expect("a capped answer must report the PRE-CAP total") as usize;
        assert!(
            total > returned,
            "`total` reports {total} for {returned} returned rows — it is counting what \
             SURVIVED the cap, so a capped answer looks complete"
        );
        assert_eq!(
            capped["returned"].as_u64().unwrap() as usize,
            returned,
            "`returned` must spell the returned count the same way every sibling tool does"
        );
    }

    #[test]
    fn a_result_that_fits_is_not_reported_as_truncated() {
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let full = dispatch(
            &store,
            None,
            "code_context",
            json!({ "seeds": ["greet"] }),
            None,
        )
        .unwrap();

        assert_eq!(full["truncated"], json!(false));
        assert_eq!(
            full["limit"],
            json!(nestweaver_engine::CODE_CONTEXT_DEFAULT_LIMIT),
            "an omitted limit must report the default it actually applied"
        );
        assert_eq!(
            full["connected"].as_array().unwrap().len(),
            full["connected_count"].as_u64().unwrap() as usize
        );
    }

    // ── nw-212: a value that could not be READ is not a value of zero ──
    //
    // These are the sites where a store error was converted into a confident
    // answer — a count of zero, an empty list, a "not found" — so the caller
    // could not tell "we looked and there is nothing" from "we failed to look".

    /// `brain_status` already disclosed unreadable TOTALS via `unavailable` +
    /// nulls. The per-vault note count, twenty lines below that block, still
    /// reported a failed read as a vault holding zero notes. This pins the
    /// disclosure contract both counts now share.
    #[test]
    fn brain_status_reports_complete_counts_on_a_healthy_store() {
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let status = dispatch(&store, None, "brain_status", json!({}), None).unwrap();

        assert_eq!(
            status["counts_complete"],
            json!(true),
            "nothing failed, so nothing may be listed as unavailable: {}",
            status["unavailable"]
        );
        assert_eq!(status["unavailable"], json!([]));
        // The per-vault counts must be REAL numbers on a healthy store, not
        // nulls — the disclosure path must not fire when nothing is wrong.
        for vault in status["vaults"].as_array().unwrap_or(&Vec::new()) {
            assert!(
                vault["note_count"].is_number(),
                "a readable vault must report a number, got {vault}"
            );
        }
    }

    /// `resolve_note_by_title` matched a row and then hydrated it with `.ok()`,
    /// so a failed hydration became "no note found with that title" — a claim
    /// about the vault made on the strength of a failure to read it. A genuine
    /// miss must still be a clean `None`, or the fix would just trade one wrong
    /// answer for a spurious error.
    #[test]
    fn a_title_that_matches_nothing_is_still_a_clean_miss() {
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let resolved = resolve_note_by_title(&store, "no such note exists anywhere")
            .expect("a miss is not an error");

        assert!(resolved.is_none());
    }

    // ── nw-214: ranking staleness must reach the AGENT, not just the human ──

    /// hub_nodes and bridge_nodes rank over edges the nw-103 import-fan-out fix
    /// could not repair on disk. The CLI has warned about that at four call
    /// sites; the MCP surface warned nowhere, so an agent got
    /// confidently-wrong numbers with nothing to indicate it.
    ///
    /// Asserted on the RESULT, which is what becomes structuredContent. A
    /// disclosure in `_meta` would not count: `_meta` is client/UI-facing and
    /// is typically hidden from the model.
    #[test]
    fn stale_rankings_are_disclosed_to_the_agent_in_the_result() {
        for tool in ["hub_nodes", "bridge_nodes"] {
            let (_dir, db_path) = index_on_disk();
            set_current_db_path(db_path.clone());
            let store = GraphStore::open(&db_path).unwrap();

            // Age the fixture: `index_directory` records a CURRENT generation,
            // so the sidecar has to go for the repo to look pre-fix — which is
            // exactly the on-disk state of any graph indexed before nw-103.
            let sidecar = nestweaver_engine::sidecar_path(
                &db_path,
                nestweaver_engine::resolver_generation::RESOLVER_GENERATION_SIDECAR,
            );
            fs::remove_file(&sidecar).unwrap();

            let value = dispatch(&store, None, tool, json!({}), None).unwrap();

            assert_eq!(
                value.get("rankings_stale").and_then(Value::as_bool),
                Some(true),
                "{tool} must flag stale rankings"
            );
            let note = value
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            assert!(
                note.contains("nw-103") && note.contains("index"),
                "{tool} note must say what is wrong AND how to fix it, got: {note}"
            );
        }
    }

    /// The other half, and the half that makes the test above mean something:
    /// once the repos ARE current, the disclosure must go away. Without this,
    /// a helper hardcoded to always warn would pass.
    #[test]
    fn current_rankings_carry_no_staleness_disclosure() {
        for tool in ["hub_nodes", "bridge_nodes"] {
            let (_dir, db_path) = index_on_disk();
            set_current_db_path(db_path.clone());
            let store = GraphStore::open(&db_path).unwrap();

            for repo in store.list_repos(None).unwrap() {
                nestweaver_engine::resolver_generation::record(&db_path, &repo.uid).unwrap();
            }

            let value = dispatch(&store, None, tool, json!({}), None).unwrap();

            assert!(
                value.get("rankings_stale").is_none(),
                "{tool} must not flag staleness once every repo is current"
            );
            let note = value
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                !note.contains("nw-103"),
                "{tool} must not carry the staleness note once current, got: {note}"
            );
        }
    }

    /// `note` is a shared channel — hub_nodes already used it to say clustering
    /// is absent. Adding a second disclosure must not silently drop the first,
    /// which would trade one silent omission for another.
    #[test]
    fn a_second_disclosure_does_not_overwrite_the_first() {
        let mut resp = json!({ "note": "first." });
        attach_note(&mut resp, "second.".to_string());
        assert_eq!(resp["note"], json!("first. second."));

        let mut empty = json!({});
        attach_note(&mut empty, "only.".to_string());
        assert_eq!(empty["note"], json!("only."));
    }

    // ── nw-C2: index-publication wait, classification, and status ───────

    /// A pid guaranteed not to name a live process: spawn a child and reap it.
    ///
    /// nw-138: resolve `true` via PATH. macOS ships it at /usr/bin/true and has
    /// no /bin/true, so hardcoding the path panicked with NotFound and failed
    /// four tests on every macOS machine while passing in Linux CI. This was
    /// the third copy of this helper; the engine and daemon copies were fixed
    /// in fd06ca94 and f3e2529c.
    fn reaped_child_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        pid
    }

    fn write_marker(db_path: &std::path::Path, pid: u32, reason: Option<&str>) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        fs::write(
            nestweaver_engine::sidecar_path(db_path, ".index-dirty"),
            nestweaver_store::index_publication::format_marker_payload(pid, nanos, reason),
        )
        .unwrap();
    }

    #[test]
    fn a_wedged_publication_error_names_the_marker_the_dead_pid_and_the_repair() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let dead = reaped_child_pid();
        write_marker(&db_path, dead as u32, None);
        let store = GraphStore::open(&db_path).unwrap();

        let classified = classify_index_publication_error(
            &store,
            anyhow!("PageRank unavailable during dirty index publication"),
        );
        let message = format!("{classified:#}");
        assert!(message.contains("WEDGED"), "{message}");
        assert!(message.contains(&format!("{dead}")), "{message}");
        assert!(message.contains(".index-dirty"), "{message}");
        assert!(message.contains("nestweaver repair"), "{message}");
        assert!(
            message.contains("not a dirty git working tree"),
            "the message must correct the wrong conclusion the reporter drew: {message}"
        );
    }

    #[test]
    fn a_live_publication_error_reads_as_transient_and_names_no_repair() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        write_marker(&db_path, std::process::id(), None);
        let store = GraphStore::open(&db_path).unwrap();

        let message = format!(
            "{:#}",
            classify_index_publication_error(
                &store,
                anyhow!("PageRank unavailable during dirty index publication"),
            )
        );
        assert!(message.contains("TRANSIENT"), "{message}");
        assert!(!message.contains("nestweaver repair"), "{message}");
        assert!(
            message.contains("not a dirty git working tree"),
            "{message}"
        );
    }

    #[test]
    fn a_wedged_cancelled_publication_also_names_index_force() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        write_marker(&db_path, reaped_child_pid() as u32, Some("cancelled"));
        let store = GraphStore::open(&db_path).unwrap();
        let message = format!(
            "{:#}",
            classify_index_publication_error(
                &store,
                anyhow!("PageRank unavailable during dirty index publication"),
            )
        );
        assert!(message.contains("WEDGED"), "{message}");
        assert!(message.contains("--force"), "{message}");
    }

    #[test]
    fn classification_leaves_unrelated_errors_untouched() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        write_marker(&db_path, reaped_child_pid() as u32, None);
        let store = GraphStore::open(&db_path).unwrap();
        let message = format!(
            "{:#}",
            classify_index_publication_error(&store, anyhow!("no such symbol: foo"))
        );
        assert_eq!(message, "no such symbol: foo");
    }

    #[test]
    fn the_bounded_wait_lets_a_dispatch_through_when_the_marker_clears() {
        reset_session();
        set_index_publication_wait_ms(10_000);
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let marker = nestweaver_engine::sidecar_path(&db_path, ".index-dirty");
        write_marker(&db_path, std::process::id(), None);
        let store = GraphStore::open(&db_path).unwrap();

        // The clearer touches ONLY the marker file — it never acquires or
        // releases the publication lease, so the in-process condvar is never
        // notified. This is what an out-of-process writer looks like from
        // here, and it is why the wait must poll the file.
        let clearer = std::thread::spawn({
            let marker = marker.clone();
            move || {
                std::thread::sleep(std::time::Duration::from_millis(80));
                fs::remove_file(&marker).unwrap();
            }
        });

        let started = std::time::Instant::now();
        let value = dispatch(&store, None, "hub_nodes", json!({ "limit": 5 }), None).unwrap();
        clearer.join().unwrap();

        assert!(value.is_object() || value.is_array(), "{value}");
        assert!(!store.is_index_publication_dirty());
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(50),
            "the dispatch must actually have waited"
        );
        assert_eq!(
            store.index_publication_waiter_count(),
            0,
            "a reader must never acquire or queue on the publication lease"
        );
        set_index_publication_wait_ms(env_index_publication_wait_ms());
    }

    #[test]
    fn brain_status_reports_a_wedged_index_publication_without_a_daemon() {
        reset_session();
        set_index_publication_wait_ms(0);
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let dead = reaped_child_pid();
        write_marker(&db_path, dead as u32, None);
        let store = GraphStore::open(&db_path).unwrap();

        // No daemon anywhere in this test: the field is derived from the
        // marker FILE, which is the whole point — the reporter was on the
        // direct `--no-daemon` path.
        let status = dispatch(&store, None, "brain_status", json!({}), None).unwrap();
        let publication = &status["index_publication"];
        assert_eq!(publication["dirty"], json!(true));
        assert_eq!(publication["determinable"], json!(true));
        assert_eq!(publication["writer_pid"], json!(dead));
        assert_eq!(publication["writer_alive"], json!(false));
        assert_eq!(publication["wedged"], json!(true));
        assert!(
            publication["marker_path"]
                .as_str()
                .unwrap()
                .ends_with(".index-dirty"),
            "{publication}"
        );
        let warnings = status["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w["kind"] == "index_publication_wedged"
                    && w["action"]
                        .as_str()
                        .is_some_and(|a| a.contains("nestweaver repair"))),
            "a wedged publication must carry a `kind`-addressable, actionable warning: {warnings:?}"
        );
        set_index_publication_wait_ms(env_index_publication_wait_ms());
    }

    #[test]
    fn brain_status_reports_a_clean_index_publication() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        let status = dispatch(&store, None, "brain_status", json!({}), None).unwrap();
        assert_eq!(status["index_publication"]["dirty"], json!(false));
        assert_eq!(status["index_publication"]["wedged"], json!(false));
    }

    /// The shared builder serves ONE top-level schema on every path: the
    /// former direct-only keys (`db`, `instance_ids`, `vault_details`) moved
    /// in, and the daemon-runtime fields are always present — explicit nulls
    /// here, live values only on the daemon's gRPC surface.
    #[test]
    fn brain_status_json_carries_one_schema_with_null_daemon_runtime_fields() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let status = brain_status_json(&store, None).unwrap();
        assert_eq!(status["db"], json!(db_path.display().to_string()));
        assert_eq!(status["vault_details"], status["vaults"]);
        assert!(status["instance_ids"].is_array());
        for field in DAEMON_RUNTIME_STATUS_FIELDS {
            assert!(
                status.get(*field).is_some(),
                "{field} must always be present: {status}"
            );
        }
        for field in DAEMON_RUNTIME_STATUS_FIELDS
            .iter()
            .filter(|f| !f.starts_with("tantivy_"))
        {
            assert!(
                status[*field].is_null(),
                "{field} must be an explicit null off the daemon: {status}"
            );
        }
        // The tantivy fields are derived from the builder's own argument.
        assert_eq!(status["tantivy_available"], json!(false));
        assert_eq!(status["tantivy_doc_count"], json!(0));
        assert_eq!(status["degraded_components"], json!([]));
        assert!(
            status.get("_meta").is_none(),
            "provenance is added by the serving layer, not the builder: {status}"
        );
    }

    /// The direct-path marker: every daemon-runtime field becomes an explicit
    /// null and the bypass is disclosed in `degraded_components`, a
    /// synthesized `_meta`, and a `daemon_bypassed` warning carrying the
    /// cause — while file-derived fields (`index_publication`, `warnings`)
    /// survive untouched.
    #[test]
    fn mark_brain_status_daemon_bypassed_discloses_the_degradation() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        let mut status = brain_status_json(&store, None).unwrap();

        mark_brain_status_daemon_bypassed(&mut status, "test bypass cause");

        assert_eq!(status["degraded_components"], json!(["daemon_runtime"]));
        assert_eq!(
            status["_meta"]["degraded_components"],
            json!(["daemon_runtime"])
        );
        assert_eq!(status["_meta"]["sources"], json!(["direct"]));
        for field in DAEMON_RUNTIME_STATUS_FIELDS {
            assert!(
                status[*field].is_null(),
                "{field} must be an explicit null on the direct path: {status}"
            );
        }
        let warnings = status["warnings"].as_array().unwrap();
        let bypass = warnings
            .iter()
            .find(|w| w["kind"] == "daemon_bypassed")
            .expect("a daemon_bypassed warning must carry the disclosure");
        assert_eq!(bypass["cause"], json!("test bypass cause"));
        assert!(status["index_publication"].is_object());
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
            RESPONSE_SHAPE_VERSION,
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

    struct CountingFailingEmbed {
        calls: std::sync::atomic::AtomicUsize,
    }

    struct BlockingFailingCacheEmbed {
        started: std::sync::mpsc::SyncSender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    struct SignallingFixedEmbed {
        called: std::sync::mpsc::SyncSender<()>,
        vector: Vec<f32>,
    }

    impl EmbedQueryFn for CountingFailingEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("deterministic inference failure")
        }
    }

    impl EmbedQueryFn for BlockingFailingCacheEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.started.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            anyhow::bail!("loading epoch inference failed")
        }
    }

    impl EmbedQueryFn for SignallingFixedEmbed {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.called.send(()).unwrap();
            Ok(self.vector.clone())
        }
    }

    #[test]
    fn loading_semantic_result_cannot_mask_ready_model_in_process() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        assert!(store.add_embedding("sym:semantic-cache-probe", vec![1.0, 0.0, 0.0]));
        let args = json!({ "seeds": ["greet"], "token_budget": 2000 });

        let loading = dispatch(&store, None, "brain_context", args.clone(), None).unwrap();
        assert_eq!(loading["semantic_applied"], false);
        assert_eq!(loading["degraded_components"], json!(["semantic"]));

        let ready_model = FixedEmbed(vec![1.0, 0.0, 0.0]);
        let ready = dispatch(&store, None, "brain_context", args, Some(&ready_model)).unwrap();
        assert_eq!(
            ready["semantic_applied"], true,
            "ready model must recompute instead of hitting a loading result"
        );
        assert_eq!(ready["degraded_components"], json!([]));
    }

    #[test]
    fn degraded_semantic_result_is_not_persisted_across_readiness_transition() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        assert!(store.add_embedding("sym:persisted-semantic-probe", vec![1.0, 0.0, 0.0]));
        let args = json!({ "seeds": ["greet"], "token_budget": 2000 });

        let degraded = dispatch(&store, None, "brain_context", args.clone(), None).unwrap();
        assert_eq!(degraded["degraded_components"], json!(["semantic"]));
        flush_response_cache();

        // Simulate a new dispatch session reopening the persisted cache after
        // the daemon's model transitions from loading to ready.
        reset_session();
        set_current_db_path(db_path.clone());
        let ready_model = FixedEmbed(vec![1.0, 0.0, 0.0]);
        let ready = dispatch(&store, None, "brain_context", args, Some(&ready_model)).unwrap();
        assert_eq!(
            ready["semantic_applied"], true,
            "persisted loading response must not survive model readiness"
        );
        assert_eq!(CACHE_HITS.with(|c| c.get()), 0);
    }

    #[test]
    fn inference_failed_semantic_result_is_never_cached() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        assert!(store.add_embedding("sym:failed-semantic-probe", vec![1.0, 0.0, 0.0]));
        let model = CountingFailingEmbed { calls: 0.into() };
        let args = json!({ "seeds": ["greet"], "token_budget": 2000 });

        for _ in 0..2 {
            let result =
                dispatch(&store, None, "brain_context", args.clone(), Some(&model)).unwrap();
            assert_eq!(result["degraded_components"], json!(["semantic"]));
        }

        assert_eq!(
            model.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "failed inference must be retried, never served from cache"
        );
        flush_response_cache();
        let cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
            RESPONSE_SHAPE_VERSION,
        );
        assert!(
            cache.is_empty(),
            "degraded semantic responses must not be persisted"
        );
    }

    #[test]
    fn project_context_inference_failure_is_never_cached() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        let project = nestweaver_schema::Project {
            uid: "proj:semantic-cache".to_string(),
            name: "Semantic Cache Project".to_string(),
            summary: None,
            instance_id: "test".to_string(),
        };
        store.insert_project(&project).unwrap();
        let symbol_uid = store.lookup_symbols_by_name("greet").unwrap()[0]
            .uid
            .clone();
        store
            .batch_insert_project_symbol_edges(&project.uid, std::slice::from_ref(&symbol_uid), 1.0)
            .unwrap();
        assert!(store.add_embedding(&symbol_uid, vec![1.0, 0.0, 0.0]));
        let model = CountingFailingEmbed { calls: 0.into() };
        let args = json!({ "project": "Semantic Cache Project", "token_budget": 2000 });

        for _ in 0..2 {
            let result =
                dispatch(&store, None, "project_context", args.clone(), Some(&model)).unwrap();
            assert_eq!(result["degraded_components"], json!(["semantic"]));
        }

        assert_eq!(
            model.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "project_context inference failure must be retried, never cached"
        );
    }

    #[test]
    fn ready_model_does_not_join_degraded_single_flight() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        let store = std::sync::Arc::new(GraphStore::open(&db_path).unwrap());
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        assert!(store.add_embedding("sym:single-flight-probe", vec![1.0, 0.0, 0.0]));
        let args = json!({ "seeds": ["greet"], "token_budget": 2000 });
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let loading_model = std::sync::Arc::new(BlockingFailingCacheEmbed {
            started: started_tx,
            release: std::sync::Mutex::new(release_rx),
        });

        let loading_store = store.clone();
        let loading_path = db_path.clone();
        let loading_args = args.clone();
        let loading = std::thread::spawn(move || {
            set_current_db_path(loading_path);
            dispatch(
                &loading_store,
                None,
                "brain_context",
                loading_args,
                Some(loading_model.as_ref()),
            )
            .unwrap()
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("loading model must enter inference");

        let (ready_called_tx, ready_called_rx) = std::sync::mpsc::sync_channel(1);
        let ready_model = std::sync::Arc::new(SignallingFixedEmbed {
            called: ready_called_tx,
            vector: vec![1.0, 0.0, 0.0],
        });
        let ready_store = store.clone();
        let ready_path = db_path.clone();
        let ready = std::thread::spawn(move || {
            set_current_db_path(ready_path);
            dispatch(
                &ready_store,
                None,
                "brain_context",
                args,
                Some(ready_model.as_ref()),
            )
            .unwrap()
        });

        ready_called_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("ready model must compute independently of degraded in-flight request");
        let ready_result = ready.join().unwrap();
        assert_eq!(ready_result["semantic_applied"], true);

        release_tx.send(()).unwrap();
        let loading_result = loading.join().unwrap();
        assert_eq!(loading_result["degraded_components"], json!(["semantic"]));
    }

    #[test]
    fn cancellation_during_failed_inference_is_not_published_or_cached() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        let store = std::sync::Arc::new(GraphStore::open(&db_path).unwrap());
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        assert!(store.add_embedding("sym:cancel-error-probe", vec![1.0, 0.0, 0.0]));
        let args = json!({ "seeds": ["greet"], "token_budget": 2000 });
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let model = std::sync::Arc::new(BlockingFailingCacheEmbed {
            started: started_tx,
            release: std::sync::Mutex::new(release_rx),
        });

        let worker_store = store.clone();
        let worker_path = db_path.clone();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            reset_session();
            set_current_db_path(worker_path);
            let result = dispatch_cancellable(
                &worker_store,
                None,
                "brain_context",
                args,
                Some(model.as_ref()),
                Some(&worker_cancel),
                None,
            );
            flush_response_cache();
            result
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("failing inference must start");
        cancel.store(true, std::sync::atomic::Ordering::Release);
        release_tx.send(()).unwrap();

        let err = worker
            .join()
            .unwrap()
            .expect_err("cancelled inference error must not degrade to success");
        assert!(
            err.downcast_ref::<nestweaver_store::StoreError>()
                .is_some_and(nestweaver_store::StoreError::is_cancelled),
            "expected StoreError::Cancelled, got: {err:#}"
        );
        let cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
            RESPONSE_SHAPE_VERSION,
        );
        assert!(
            cache.is_empty(),
            "cancelled inference result must not be published to the persisted cache"
        );
    }

    #[test]
    fn cancelled_follower_rejects_failed_leaders_degraded_result() {
        reset_session();
        let (_dir, db_path) = index_on_disk();
        let store = std::sync::Arc::new(GraphStore::open(&db_path).unwrap());
        store
            .load_pagerank_cache(&nestweaver_engine::sidecar_path(&db_path, ".pagerank.json"))
            .unwrap();
        assert!(store.add_embedding("sym:cancelled-follower-probe", vec![1.0, 0.0, 0.0]));
        let args = json!({ "seeds": ["greet"], "token_budget": 2000 });
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let model = std::sync::Arc::new(BlockingFailingCacheEmbed {
            started: started_tx,
            release: std::sync::Mutex::new(release_rx),
        });

        let flight_key = {
            let key = mix_visibility_cache_key(
                nestweaver_store::cache::ResponseCache::key("brain_context", &args),
                visibility_cache_salt(None),
            );
            let key =
                mix_visibility_cache_key(key, semantic_cache_salt("brain_context", Some(&*model)));
            (
                db_path.clone(),
                key,
                store.graph_generation(),
                whole_db_scope_digest(&db_path),
            )
        };

        let leader_store = store.clone();
        let leader_path = db_path.clone();
        let leader_args = args.clone();
        let leader_model = model.clone();
        let leader = std::thread::spawn(move || {
            reset_session();
            set_current_db_path(leader_path.clone());
            let result = dispatch(
                &leader_store,
                None,
                "brain_context",
                leader_args,
                Some(leader_model.as_ref()),
            );
            let cache_is_empty = RESPONSE_CACHE.with(|caches| {
                caches
                    .borrow()
                    .get(&leader_path)
                    .is_none_or(nestweaver_store::cache::ResponseCache::is_empty)
            });
            flush_response_cache();
            (result, cache_is_empty)
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("leader must block inside failing inference");

        let leader_ref_count = {
            let flights = IN_FLIGHT
                .get()
                .expect("leader must initialize the flight map")
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::sync::Arc::strong_count(
                flights
                    .get(&flight_key)
                    .expect("leader must publish the in-flight slot"),
            )
        };

        let follower_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let follower_store = store.clone();
        let follower_path = db_path.clone();
        let follower_model = model.clone();
        let follower_cancel_worker = follower_cancel.clone();
        let (follower_done_tx, follower_done_rx) = std::sync::mpsc::sync_channel(1);
        let follower = std::thread::spawn(move || {
            reset_session();
            set_current_db_path(follower_path.clone());
            let result = dispatch_cancellable(
                &follower_store,
                None,
                "brain_context",
                args,
                Some(follower_model.as_ref()),
                Some(&follower_cancel_worker),
                None,
            );
            let cache_is_empty = RESPONSE_CACHE.with(|caches| {
                caches
                    .borrow()
                    .get(&follower_path)
                    .is_none_or(nestweaver_store::cache::ResponseCache::is_empty)
            });
            flush_response_cache();
            follower_done_tx.send(()).unwrap();
            (result, cache_is_empty)
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let follower_registered = IN_FLIGHT.get().is_some_and(|flights| {
                let flights = flights.lock().unwrap_or_else(|error| error.into_inner());
                flights
                    .get(&flight_key)
                    .is_some_and(|slot| std::sync::Arc::strong_count(slot) > leader_ref_count)
            });
            if follower_registered {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "follower must join the leader's in-flight slot"
            );
            std::thread::yield_now();
        }

        follower_cancel.store(true, std::sync::atomic::Ordering::Release);
        let follower_stopped_waiting = follower_done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok();
        release_tx.send(()).unwrap();

        let (leader_result, leader_cache_is_empty) = leader.join().unwrap();
        let leader_result = leader_result.unwrap();
        assert_eq!(leader_result["degraded_components"], json!(["semantic"]));
        assert!(
            leader_cache_is_empty,
            "leader's degraded result must not enter its response cache"
        );
        assert!(
            follower_stopped_waiting,
            "cancelled follower must stop waiting before the leader publishes"
        );
        let (follower_result, follower_cache_is_empty) = follower.join().unwrap();
        let follower_error = follower_result
            .expect_err("cancelled follower must reject the leader's degraded success");
        assert!(
            follower_error
                .downcast_ref::<nestweaver_store::StoreError>()
                .is_some_and(nestweaver_store::StoreError::is_cancelled),
            "expected StoreError::Cancelled, got: {follower_error:#}"
        );
        assert!(
            follower_cache_is_empty,
            "cancelled follower must not enter a result into its response cache"
        );
        let persisted_cache = nestweaver_store::cache::ResponseCache::open(
            &db_path,
            nestweaver_store::cache::DEFAULT_MAX_SIZE_MB,
            RESPONSE_SHAPE_VERSION,
        );
        assert!(
            persisted_cache.is_empty(),
            "failed/degraded flight must not create a persisted response-cache entry"
        );
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
        // Seed one embedding so the semantic (vector KNN) leg actually runs:
        // on an un-embedded db that leg is skipped entirely
        // (`store_has_embeddings`) and the pre-cancelled flag would never
        // trip inside it.
        assert!(store.add_embedding("sym:cancel-probe", vec![1.0, 0.0, 0.0]));

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
            RESPONSE_SHAPE_VERSION,
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
                mtime_nanos: 1,
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
                mtime_nanos: 1,
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

    // ── Single-flight coalescing ──────────────────────────────────────────

    /// Poll the IN_FLIGHT map until the slot for `key` reaches the requested
    /// strong-count condition (or fail after a generous deadline).
    fn wait_for_flight_count(key: &InFlightKey, cond: impl Fn(usize) -> bool) -> usize {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let count = IN_FLIGHT.get().and_then(|flights| {
                flights
                    .lock()
                    .ok()
                    .and_then(|f| f.get(key).map(std::sync::Arc::strong_count))
            });
            if let Some(c) = count
                && cond(c)
            {
                return c;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "flight never reached the expected state"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Deterministic overlap: block the leader inside `compute` until the
    /// follower has registered on the same flight (observed via the slot's
    /// Arc strong count), then release it. The computation must run exactly
    /// once and both callers must receive the same value — the stampede fix
    /// for concurrent identical brain_context calls.
    #[test]
    fn single_flight_coalesces_concurrent_identical_calls() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        let key: InFlightKey = (
            std::path::PathBuf::from("/tmp/single-flight-test.lbug"),
            7,
            8,
            9,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Barrier::new(2));

        let leader_key = key.clone();
        let leader_calls = Arc::clone(&calls);
        let leader_release = Arc::clone(&release);
        let leader = std::thread::spawn(move || {
            coalesce_in_flight(leader_key, None, move || {
                leader_calls.fetch_add(1, Ordering::SeqCst);
                // Hold the flight open until the main thread has watched the
                // follower attach, then released us below.
                leader_release.wait();
                Ok(json!({ "answer": 42 }))
            })
        });

        // Wait for the leader to register its flight, then attach a follower.
        let baseline = wait_for_flight_count(&key, |c| c >= 1);
        let follower_key = key.clone();
        let follower = std::thread::spawn(move || {
            coalesce_in_flight(follower_key, None, || {
                panic!("follower must never run the computation")
            })
        });
        wait_for_flight_count(&key, |c| c > baseline);

        release.wait(); // let the leader finish
        let leader_result = leader.join().expect("leader thread panicked");
        let follower_result = follower.join().expect("follower thread panicked");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "identical concurrent calls must share one computation"
        );
        assert_eq!(leader_result.unwrap(), json!({ "answer": 42 }));
        assert_eq!(
            follower_result.unwrap(),
            json!({ "answer": 42 }),
            "follower must receive a clone of the leader's result"
        );
    }

    /// A failed leader cleans up: followers get the error, and a later call
    /// with the same key recomputes instead of poisoning the flight map.
    #[test]
    fn single_flight_error_propagates_and_cleans_up() {
        let key: InFlightKey = (
            std::path::PathBuf::from("/tmp/single-flight-error-test.lbug"),
            1,
            2,
            3,
        );

        let err = coalesce_in_flight(key.clone(), None, || Err(anyhow!("boom"))).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");

        // The flight entry must be gone and the next call must compute fresh.
        let ok = coalesce_in_flight(key.clone(), None, || Ok(json!("recovered"))).unwrap();
        assert_eq!(ok, json!("recovered"));
        let entry_gone = IN_FLIGHT
            .get()
            .map(|f| !f.lock().unwrap().contains_key(&key))
            .unwrap_or(true);
        assert!(
            entry_gone,
            "flight entry must be removed after the leader finishes"
        );
    }

    /// Two files, so a `target` filter can be seen to change the answer.
    fn index_two_files_on_disk() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("alpha.js"),
            "function alphaOne(n){return alphaTwo(n);}\nfunction alphaTwo(n){return n;}\n",
        )
        .unwrap();
        fs::write(
            src.join("beta.js"),
            "function betaOne(n){return betaTwo(n);}\nfunction betaTwo(n){return n;}\n",
        )
        .unwrap();
        let db_path = dir.path().join("test.lbug");
        let repo_url = format!("file://{}", src.display());
        nestweaver_engine::index_directory(&src, &db_path, "test", &repo_url, "local").unwrap();
        (dir, db_path)
    }

    /// nw-321. `summary` handed the HUMAN a structured list with
    /// `returned`/`total` and the AGENT a single "\n"-joined string with
    /// `count`/`total_available` — the inverse of who benefits from structure,
    /// and no caller could be written against both.
    ///
    /// The fourth divergence, which the report missed: the CLI's `total` is
    /// computed AFTER `filter_by_target` and the tool's `total_available` was
    /// captured BEFORE it. They coincide only when no `target` is passed, which
    /// is why both routes read 9657 in the evidence and the rename looked pure.
    /// Pass a `target` and they were different quantities under different names.
    #[test]
    fn get_summary_hands_the_agent_structure_and_a_total_that_respects_the_filter() {
        reset_session();
        let (_dir, db_path) = index_two_files_on_disk();
        set_current_db_path(db_path.clone());
        let store = GraphStore::open(&db_path).unwrap();

        let all = dispatch(
            &store,
            None,
            "get_summary",
            json!({ "level": "file", "no_cache": true }),
            None,
        )
        .unwrap();

        assert!(
            all["summaries"].is_array(),
            "the agent must get records, not prose it has to re-parse: {all}"
        );
        assert!(
            all["summaries_text"].is_string(),
            "the rendered form stays available under its own key: {all}"
        );
        assert_eq!(all["returned"], all["count"], "aliases must agree: {all}");
        assert_eq!(
            all["total"], all["total_available"],
            "aliases must agree: {all}"
        );
        let total_all = all["total"].as_u64().expect("total is a number");
        assert!(
            total_all >= 2,
            "the fixture must summarise both files or the filter below proves \
             nothing: {all}"
        );

        let targeted = dispatch(
            &store,
            None,
            "get_summary",
            json!({ "level": "file", "target": "alpha", "no_cache": true }),
            None,
        )
        .unwrap();
        let returned = targeted["returned"].as_u64().expect("returned is a number");
        let total = targeted["total"].as_u64().expect("total is a number");
        assert!(
            returned >= 1 && returned < total_all,
            "precondition: the target filter must actually bite: {targeted}"
        );
        assert_eq!(
            total, returned,
            "`total` must count what matched the filter the caller ASKED for; \
             counting the whole corpus under the same name makes `returned` vs \
             `total` read as truncation that never happened: {targeted}"
        );
        assert_eq!(
            targeted["truncated"],
            json!(false),
            "nothing was dropped, so nothing may be reported as dropped: {targeted}"
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
        assert_eq!(cfg.limits.default_result_limit, Some(7));
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
        // nw-321: `summaries` is the STRUCTURED list on both routes now, with
        // the rendered prose under `summaries_text`. The agent used to get the
        // string and the human the records, which is backwards.
        assert_eq!(
            via_name["summaries"]
                .as_array()
                .expect("summaries is a list")
                .len(),
            1
        );
        assert!(
            via_name["summaries_text"]
                .as_str()
                .unwrap()
                .contains("greet"),
            "summary should describe the targeted symbol"
        );
        assert_eq!(via_name["returned"], json!(1), "the canonical count name");
        assert_eq!(via_name["total"], json!(1), "the canonical total name");
        // `target` works identically.
        let via_target =
            tool_get_summary(&store, json!({ "level": "symbol", "target": "hello" })).unwrap();
        assert_eq!(via_target["count"], json!(1));
    }

    #[test]
    fn investigate_hydrate_rejects_targeting_keys() {
        // nw-084: hydrate is bulk (no per-entry selector). Passing targets/target/
        // uid/uids was silently ignored (looked like "nothing hydrated"); now it's a
        // clear error pointing to investigate_expand, matching expand's strictness.
        let store = GraphStore::in_memory().unwrap();
        for key in ["targets", "target", "uid", "uids"] {
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

    #[test]
    fn regex_search_and_count_patterns_reject_unknown_kinds() {
        // A bogus kind used to filter out every candidate and return empty
        // results; now it is an error naming the advertised kinds.
        let store = GraphStore::in_memory().unwrap();
        let err = tool_regex_search(
            &store,
            json!({ "pattern": "x", "kinds": ["Function"] }),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("unknown kind 'Function'") && err.contains("Section, Note, Symbol"),
            "{err}"
        );
        let err = tool_count_patterns(&store, json!({ "patterns": ["x"], "kinds": ["Heading"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown kind 'Heading'"), "{err}");
        // Case-insensitive valid kinds pass validation (empty results are fine
        // on an empty store).
        tool_regex_search(
            &store,
            json!({ "pattern": "x", "kinds": ["section", "NOTE"] }),
            None,
        )
        .expect("case-insensitive advertised kinds must be accepted");
    }

    #[test]
    fn brain_context_rejects_unknown_kinds() {
        // A bogus kind used to silently match no nodes and return an empty
        // context; now it is an error naming the advertised kinds (same
        // policy as regex_search/count_patterns).
        let store = GraphStore::in_memory().unwrap();
        let err = tool_brain_context(
            &store,
            None,
            json!({ "seeds": ["x"], "kinds": ["Banana"] }),
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown kind 'banana'"), "{err}");
        // Case-insensitive advertised kinds and Symbol sub-kind prefixes pass
        // (the seed must resolve, so index a note named after it first).
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&nestweaver_schema::Note {
                uid: "note:seed".to_string(),
                vault_uid: "vlt:v".to_string(),
                file_path: "seed.md".to_string(),
                title: "x".to_string(),
                note_kind: nestweaver_schema::NoteKind::General,
                word_count: 1,
                content_hash: "hash-seed".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        tool_brain_context(
            &store,
            None,
            json!({ "seeds": ["x"], "kinds": ["note", "SYMBOL", "Symbol/Function"] }),
            None,
            None,
        )
        .expect("advertised kinds and symbol sub-kinds must be accepted");
    }

    #[test]
    fn regex_search_rejects_empty_pattern() {
        // Parity with count_patterns: an empty pattern matches everything and
        // is a scan-cost lever, not a query.
        let store = GraphStore::in_memory().unwrap();
        for args in [json!({ "pattern": "" }), json!({ "pattern": "   " })] {
            let err = tool_regex_search(&store, args, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("empty pattern"), "{err}");
        }
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
                repo: String::new(),
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

#[cfg(test)]
mod brain_impact_uid_resolution_tests {
    use super::*;
    use nestweaver_schema::{Symbol, SymbolKind, Visibility};

    fn mk_symbol(uid: &str, name: &str) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo:api".to_string(),
            file_path: "src/target.rs".to_string(),
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
        }
    }

    /// A UID-shaped input that does not resolve in this store (garbage,
    /// typo'd, or from another DB) must fail closed with the same `not_found`
    /// contract as the name path — not return status ok with an empty list.
    #[test]
    fn brain_impact_unknown_uid_fails_closed() {
        let store = GraphStore::in_memory().expect("in_memory store");
        store
            .insert_symbol(&mk_symbol("sym:repo:api:target:1", "Target"))
            .unwrap();

        for bogus in [
            "sym:repo:api:nonexistent:99",
            "garbage:uid:from:nowhere",
            "sym:repo:otherdb:target:1",
        ] {
            let result = tool_brain_impact(&store, json!({ "symbol": bogus }), None, None)
                .expect("impact call");
            assert_eq!(
                result["status"], "not_found",
                "unknown UID '{bogus}' must fail closed, got: {result}"
            );
        }
    }

    /// A legit symbol with zero dependents still resolves by UID and
    /// returns status ok with an empty impact list (exit 0 at the CLI).
    #[test]
    fn brain_impact_zero_dependent_uid_still_ok() {
        let store = GraphStore::in_memory().expect("in_memory store");
        store
            .insert_symbol(&mk_symbol("sym:repo:api:lonely:1", "Lonely"))
            .unwrap();

        let result = tool_brain_impact(
            &store,
            json!({ "symbol": "sym:repo:api:lonely:1" }),
            None,
            None,
        )
        .expect("impact call");
        assert_eq!(result["status"], "ok", "{result}");
        assert_eq!(result["total"], 0);
        assert!(result["impact_nodes"].as_array().unwrap().is_empty());
    }
}

#[cfg(test)]
mod honest_count_tests {
    use super::*;

    /// A healthy brain reports numbers, an empty `unavailable`, and
    /// `counts_complete: true`.
    ///
    /// The contract this pins is the DISTINCTION: a number means "counted",
    /// `null` means "could not be read". They used to be the same value —
    /// `unwrap_or(0)` — which is CWE-390, and aggravated because this tool's
    /// own description says "if counts are zero, use brain_add_source to index
    /// content". One failed query advised re-indexing a healthy vault.
    #[test]
    fn a_healthy_brain_reports_counts_and_declares_them_complete() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let status = brain_status_json(&store, None).expect("status");

        assert_eq!(
            status["unavailable"],
            serde_json::json!([]),
            "nothing failed, so nothing is unavailable: {status}"
        );
        assert_eq!(status["counts_complete"], true, "{status}");
        for field in ["notes", "headings", "sections", "tags", "wikilinks"] {
            assert!(
                status[field].is_number(),
                "{field} must be a NUMBER when it was actually counted, never null: {status}"
            );
        }
    }

    /// The description must not tell a caller to treat `null` like `0`.
    ///
    /// The advice is what made the false zero dangerous rather than merely
    /// wrong, so the advice is part of the fix.
    #[test]
    fn the_description_separates_unreadable_from_zero() {
        let schema = tool_schema_brain_status();
        let description = schema["description"].as_str().expect("description");
        assert!(
            description.contains("could NOT BE READ"),
            "the description must distinguish an unreadable count from a zero one"
        );
        assert!(
            description.contains("unavailable"),
            "the description must point the caller at the field that names what failed"
        );
    }
}

#[cfg(test)]
mod stale_check_tool_tests {
    use super::*;

    /// A repo whose local working tree was deleted must be flagged
    /// `status: "missing"` and counted as NEEDING RE-INDEX — never silently
    /// `[ok]`.
    ///
    /// nw-163: it used to be reported as `is_stale: true` as well, which is
    /// false — the repo is not behind HEAD, it has no working tree to compare
    /// against. `needs_reindex` is the flag that means "act on this".
    #[test]
    fn stale_check_flags_deleted_working_tree_as_missing() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let gone = tempfile::tempdir().unwrap();
        let gone_path = gone.path().display().to_string();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:gone".to_string(),
                url: format!("file://{gone_path}"),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: Some(gone_path.clone()),
            })
            .expect("insert repo");
        // Delete the working tree after indexing.
        std::fs::remove_dir_all(gone.path()).unwrap();

        let result = tool_stale_check(&store).expect("stale check");
        assert_eq!(result["any_needs_reindex"], true, "{result}");
        let repo = &result["repos"][0];
        assert_eq!(repo["status"], "missing", "{result}");
        assert_eq!(repo["needs_reindex"], true, "{result}");
        assert_eq!(
            repo["is_stale"], false,
            "a missing working tree is not 'behind HEAD'; conflating them is \
             what made this command contradict itself: {result}"
        );
        assert_eq!(result["any_stale"], false, "{result}");
    }

    /// The same repo, but with a NONZERO stored staleness counter — the case
    /// the test above missed by pinning only `staleness_commits_behind: 0`.
    ///
    /// With the tree deleted there is no HEAD to compare against, so the stored
    /// counter is a leftover from the last successful check. Falling back to it
    /// reported `is_stale: true` — a stale guess presented as a fact — for a
    /// repo whose staleness is simply unknowable.
    #[test]
    fn a_deleted_working_tree_does_not_inherit_its_last_known_staleness() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let gone = tempfile::tempdir().unwrap();
        let gone_path = gone.path().display().to_string();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:gone".to_string(),
                url: format!("file://{gone_path}"),
                indexed_sha: "abc".to_string(),
                // The only difference from the test above.
                staleness_commits_behind: 7,
                instance_id: "test".to_string(),
                name: None,
                root_path: Some(gone_path.clone()),
            })
            .expect("insert repo");
        std::fs::remove_dir_all(gone.path()).unwrap();

        let result = tool_stale_check(&store).expect("stale check");
        let repo = &result["repos"][0];
        assert_eq!(repo["status"], "missing", "{result}");
        assert_eq!(repo["needs_reindex"], true, "{result}");
        assert_eq!(
            repo["is_stale"], false,
            "HEAD is unknowable with the tree deleted, so a stored counter must \
             not be reported as staleness: {result}"
        );
        assert_eq!(result["any_stale"], false, "{result}");
        // The count itself is still reported — it is the last thing known,
        // and hiding it would lose information. Only the VERDICT was wrong.
        assert_eq!(repo["staleness_commits_behind"], 7, "{result}");
    }

    /// A repo whose SHA was committed but whose content never landed
    /// (interrupted index) compares equal to HEAD — it must still be caught by
    /// the CI gate (`status: "incomplete"`, `needs_reindex: true`).
    ///
    /// nw-163: it is NOT stale. It sits exactly at HEAD with zero commits
    /// behind, which is why reporting `is_stale: true` here read as a
    /// contradiction and made a gate keyed on `any_stale` disagree with one
    /// keyed on the exit code.
    #[test]
    fn stale_check_flags_sha_set_but_empty_repo_as_incomplete() {
        let store = GraphStore::in_memory().expect("in_memory store");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:empty".to_string(),
                url: format!("file://{root}"),
                // Not a git working tree → HEAD unreadable, commits_behind 0:
                // the SHA comparison alone would call this repo healthy.
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: Some(root.clone()),
            })
            .expect("insert repo");

        let result = tool_stale_check(&store).expect("stale check");
        assert_eq!(result["any_needs_reindex"], true, "{result}");
        assert_eq!(
            result["any_stale"], false,
            "an incomplete index at HEAD is not staleness: {result}"
        );
        let repo = &result["repos"][0];
        assert_eq!(repo["status"], "incomplete", "{result}");
        assert_eq!(repo["needs_reindex"], true, "{result}");
        assert_eq!(repo["is_stale"], false, "{result}");

        // Once content lands, the same repo must read healthy again. A File
        // node is the content marker: the code index writes one for every
        // parsed file, even files that yield zero symbols.
        store
            .insert_file(&nestweaver_schema::File {
                uid: "file:1".to_string(),
                path: "src/a.rs".to_string(),
                repo_uid: "repo:empty".to_string(),
                content_hash: "h".to_string(),
            })
            .expect("insert file");
        let healed = tool_stale_check(&store).expect("stale check");
        assert_eq!(healed["any_stale"], false, "{healed}");
        assert_eq!(healed["any_needs_reindex"], false, "{healed}");
        assert_eq!(healed["repos"][0]["status"], "ok", "{healed}");
    }

    /// A server-mode vault Repo row carries the SHA while its content lives in
    /// Note nodes off the Vault (whose `name` equals the repo's `url`). Such a
    /// healthy vault must NOT be flagged incomplete forever.
    #[test]
    fn stale_check_does_not_flag_healthy_vault_repo() {
        use nestweaver_schema::{Note, NoteKind, Vault};

        let store = GraphStore::in_memory().expect("in_memory store");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        let url = format!("file://{root}");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:vault".to_string(),
                url: url.clone(),
                // Not a git working tree → SHA comparison alone would call
                // this healthy; the content probe must agree for vaults too.
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: Some(root.clone()),
            })
            .expect("insert repo");
        store
            .upsert_vault(&Vault {
                uid: "vlt:1".to_string(),
                name: url.clone(),
                root_path: root.clone(),
                instance_id: "test".to_string(),
            })
            .expect("upsert vault");
        store
            .insert_note(&Note {
                uid: "note:1".to_string(),
                vault_uid: "vlt:1".to_string(),
                file_path: "a.md".to_string(),
                title: "A".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "h".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .expect("insert note");
        store
            .insert_vault_note_edge("vlt:1", "note:1")
            .expect("insert vault note edge");

        let result = tool_stale_check(&store).expect("stale check");
        assert_eq!(result["any_stale"], false, "{result}");
        assert_eq!(result["repos"][0]["status"], "ok", "{result}");
    }
}

#[cfg(test)]
mod destructive_tool_contract_tests {
    use super::*;

    /// Destructive tools must document the committed/warnings contract.
    ///
    /// nw-091: these mutations commit first and then run fallible post-commit
    /// reconciliation, so a response can carry `committed: true` alongside
    /// warnings. The whole point of that contract is that a caller can tell
    /// "it happened, with bookkeeping warnings" from "nothing happened" — an
    /// agent that only reads the tool description would otherwise see warnings,
    /// conclude the operation failed, and take corrective action against data
    /// that is already gone. That is the exact incident nw-091 came from, so
    /// the description carries the same weight as the response field.
    #[test]
    fn destructive_tools_document_the_committed_contract() {
        const DESTRUCTIVE: &[&str] = &["brain_remove_source", "prune_stale"];

        let schemas = all_tool_schemas();
        for name in DESTRUCTIVE {
            let schema = schemas
                .iter()
                .find(|s| s["name"] == *name)
                .unwrap_or_else(|| panic!("tool `{name}` is missing from all_tool_schemas()"));
            let description = schema["description"]
                .as_str()
                .unwrap_or_else(|| panic!("tool `{name}` has no description"));

            for required in ["committed", "reconciliation_warnings"] {
                assert!(
                    description.contains(required),
                    "`{name}` description does not explain `{required}`. A caller that cannot \
                     distinguish a committed-with-warnings result from a no-op will take \
                     corrective action against data that is already gone (nw-091).\n\n{description}"
                );
            }
        }
    }
}

/// The END-TO-END half of the `cross_repo_links` regression.
///
/// `crates/nestweaver-federation` has the merge tests, but it cannot import
/// `nestweaver-engine`, so it can only assert that a KEY survives. What
/// actually broke `nestweaver context` was the next step: the CLI deserializing
/// the merged envelope into `ContextResult`, where a missing `cross_repo_links`
/// is not a missing key but a hard `Error("missing field")`.
///
/// This crate sees both, so it can assert the thing the user experiences. A
/// federated `context` call has no other test that gets this far — the parity
/// suite configures no upstream, which is precisely why a High-severity break
/// shipped with 24 parity tests green.
#[cfg(all(test, feature = "daemon"))]
mod federated_context_roundtrip_tests {
    use serde_json::json;

    /// A `code_context` reply that has been through a two-tier merge must still
    /// parse as the type the CLI parses it as.
    #[test]
    fn a_merged_code_context_reply_still_deserializes_for_the_cli() {
        let node = |uid: &str| {
            json!({
                "uid": uid, "name": uid, "kind": "Function",
                "file_path": "a.rs", "start_line": 1,
                "signature": "fn", "relevance": 0.5,
            })
        };
        let local = json!({
            "seeds": [node("sym:seed")],
            "connected": [node("sym:local")],
            "cross_repo_links": [{ "package": "serde", "link_type": "dependency", "confidence": 0.5 }],
            "seeds_resolved": 1,
            "connected_count": 1,
        });
        let server = json!({
            "seeds": [],
            "connected": [node("sym:upstream")],
            "cross_repo_links": [{ "package": "tokio", "link_type": "dependency", "confidence": 0.9 }],
            "seeds_resolved": 0,
            "connected_count": 1,
        });

        let merged = nestweaver_federation::results::merge_structured_results(&local, &server);

        // Exactly what `Commands::Context` does with the daemon's reply.
        let parsed: nestweaver_engine::ContextResult = serde_json::from_value(merged.clone())
            .unwrap_or_else(|error| {
                panic!("the CLI could not parse a merged reply: {error}\nmerged = {merged}")
            });

        assert_eq!(
            parsed.cross_repo_links.len(),
            2,
            "both tiers' links must survive into the parsed result"
        );
        assert_eq!(
            parsed.connected.len(),
            2,
            "both tiers' connected symbols must survive"
        );
    }

    /// THE GUARD THAT SHOULD HAVE CAUGHT THIS, and did not.
    ///
    /// The federation crate has a "no field is silently dropped" test, but its
    /// input is a HAND-WRITTEN fixture — so it only protects the keys someone
    /// remembered to list. When `code_context` later grew `limit` and
    /// `truncated`, the fixture did not, and the merger dropped both while that
    /// guard stayed green. A guard against silent drift that is itself
    /// maintained by hand has the same weakness as the thing it guards.
    ///
    /// This one takes its field set from the REAL tool output, so a field added
    /// to `code_context` is covered the moment it exists, with nobody having to
    /// remember anything.
    #[test]
    fn every_field_the_real_tool_emits_survives_the_merge() {
        let (_dir, db_path) = super::cache_dispatch_tests::index_on_disk_for_merge_guard();
        super::set_current_db_path(db_path.clone());
        let store = super::GraphStore::open(&db_path).unwrap();

        let real = super::dispatch(
            &store,
            None,
            "code_context",
            json!({ "seeds": ["greet"] }),
            None,
        )
        .expect("the tool must answer");

        // Merged against itself: the point is which KEYS survive, and a second
        // tier of the same shape is the honest stand-in for an upstream.
        let merged = nestweaver_federation::results::merge_structured_results(&real, &real);

        // Per-tier bookkeeping that is meaningless after a merge.
        const INTENTIONALLY_DROPPED: &[&str] = &["_meta"];

        let dropped: Vec<&String> = real
            .as_object()
            .expect("tool output is an object")
            .keys()
            .filter(|key| !INTENTIONALLY_DROPPED.contains(&key.as_str()))
            .filter(|key| merged.get(key.as_str()).is_none())
            .collect();

        assert!(
            dropped.is_empty(),
            "code_context emits these and the merge loses them: {dropped:?}
             real = {real}
merged = {merged}"
        );
    }

    /// The single-tier path, which is what every existing test exercised — kept
    /// so a fix that only works when both tiers answer cannot pass unnoticed.
    #[test]
    fn an_unmerged_reply_deserializes_too() {
        let local = json!({
            "seeds": [], "connected": [], "cross_repo_links": [],
            "seeds_resolved": 0, "connected_count": 0,
        });

        let parsed: nestweaver_engine::ContextResult =
            serde_json::from_value(local).expect("the un-merged shape must parse");

        assert!(parsed.cross_repo_links.is_empty());
    }
}

/// nw-215: a schema `default` must be what the handler actually applies.
///
/// JSON Schema's `default` is annotation-only — nothing in the MCP stack reads
/// it and substitutes the value. Its ONLY consumer is the model reading
/// `tools/list`. So a `default` that disagrees with the handler is not harmless
/// duplication, it is misinformation delivered straight into the agent's
/// reasoning about how much a call will cost.
///
/// `project_context` advertised `token_budget: 3000` while the handler applied
/// 1000 in its default configuration, because `response_format` defaults to
/// concise and the budget follows it.
#[cfg(test)]
mod schema_default_honesty_tests {
    use super::*;

    /// nw-299(a). `clusters` was the only list-returning tool in the catalogue
    /// with no bounding parameter, and `additionalProperties: false` meant a
    /// caller-supplied `limit` was actively REJECTED — 98.7 MB on the wire with
    /// no way for an MCP client to prevent it.
    #[test]
    fn clusters_declares_a_bound_like_every_other_list_tool() {
        let tool = all_tool_schemas()
            .into_iter()
            .find(|t| t["name"] == "clusters")
            .expect("clusters must be registered");
        let props = &tool["inputSchema"]["properties"];

        assert_eq!(
            props["limit"]["default"],
            json!(50),
            "must match the CLI twin's documented default"
        );
        assert_eq!(
            props["limit"]["maximum"],
            json!(RESULT_LIMIT_MAX),
            "1000 is the ceiling every comparable list tool already uses"
        );
        assert_eq!(props["members"]["default"], json!(20));
        assert_eq!(props["members"]["maximum"], json!(200));
    }

    /// F-MCP-6. `clusters.resolution` advertised 0.5 while the handler applies
    /// 0.3 on any graph over 10K symbols — i.e. on every graph this tool exists
    /// to serve. A conditional default is not expressible in JSON Schema, so
    /// the correct fix is to omit the key (the precedent
    /// `project_context.token_budget` already set), not to state a different
    /// wrong one.
    #[test]
    fn clusters_does_not_advertise_a_resolution_it_may_not_apply() {
        let tool = all_tool_schemas()
            .into_iter()
            .find(|t| t["name"] == "clusters")
            .expect("clusters must be registered");
        let resolution = &tool["inputSchema"]["properties"]["resolution"];
        assert!(
            resolution.get("default").is_none(),
            "the applied resolution depends on symbol count (0.3 above 10K, 0.5 below), \
             so a fixed `default` here is a claim the handler does not honour"
        );
        let description = resolution["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("0.3") && description.contains("0.5"),
            "the description must state BOTH, since the schema cannot: {description}"
        );
    }

    /// nw-317 leg 2. Every `intent` enum in the registry must accept exactly
    /// what `QueryIntent::from_str` accepts. Restating a parser in a schema is
    /// how `blast-radius` came to be documented by `--help`, accepted on the
    /// direct route, and rejected through the daemon with a raw JSON-Schema
    /// error naming an internal MCP tool.
    #[test]
    fn declared_intent_enums_match_the_query_intent_parser() {
        let accepted = nestweaver_store::ranking::QueryIntent::accepted_spellings();
        assert!(
            accepted.contains(&"blast-radius"),
            "fixture drifted from the parser"
        );

        let mut rejected = Vec::new();
        let mut undeclared = Vec::new();
        for tool in all_tool_schemas() {
            let name = tool["name"].as_str().unwrap_or("<unnamed>").to_string();
            let Some(intent) = tool["inputSchema"]["properties"].get("intent") else {
                continue;
            };
            let Some(values) = intent["enum"].as_array() else {
                undeclared.push(name);
                continue;
            };
            let declared: std::collections::BTreeSet<&str> =
                values.iter().filter_map(Value::as_str).collect();
            for spelling in accepted {
                if !declared.contains(spelling) {
                    rejected.push(format!("{name} rejects `{spelling}`"));
                }
            }
            for spelling in &declared {
                assert!(
                    spelling
                        .parse::<nestweaver_store::ranking::QueryIntent>()
                        .is_ok(),
                    "{name} declares `{spelling}`, which the engine's parser rejects"
                );
            }
        }
        assert!(
            rejected.is_empty(),
            "these schemas reject an intent the engine accepts and the CLI `--help` \
             documents: {rejected:?}"
        );
        assert!(
            undeclared.is_empty(),
            "these tools take an `intent` but declare no enum, so the same string is \
             valid on one tool and invalid on its sibling: {undeclared:?}"
        );
    }

    /// nw-304. Where a schema declares `minimum`/`maximum` the validator
    /// enforces it perfectly. The gap is the ten params that declare a
    /// `default` and NO bounds: `as_u64()` returns `None` for a negative, so
    /// `limit: -1` silently became 50 — the caller's explicit request was
    /// discarded and they were told nothing — and `limit: 999999999` returned
    /// the whole dataset.
    ///
    /// Asserted over the REGISTRY so an eleventh unbounded param cannot be
    /// added without failing here.
    #[test]
    fn every_limit_style_param_declares_both_bounds() {
        const LIMIT_KEYS: &[&str] = &["limit", "top_n", "max_suggestions", "top_tags_limit"];
        let mut offenders: Vec<String> = Vec::new();

        for tool in all_tool_schemas() {
            let name = tool["name"].as_str().unwrap_or("?").to_string();
            let Some(props) = tool["inputSchema"]["properties"].as_object() else {
                continue;
            };
            for (field, spec) in props {
                if !LIMIT_KEYS.contains(&field.as_str()) {
                    continue;
                }
                if spec.get("minimum").is_none() {
                    offenders.push(format!(
                        "{name}.{field}: no `minimum` — a negative silently becomes the \
                         default instead of being rejected"
                    ));
                }
                if spec.get("maximum").is_none() {
                    offenders.push(format!(
                        "{name}.{field}: no `maximum` — the tool has no upper bound at all"
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "unbounded limit params: {offenders:#?}"
        );
    }

    /// The bound must be ENFORCED, not merely declared. A present-but-negative
    /// value is a caller bug and must surface as one.
    #[test]
    fn a_negative_limit_is_rejected_rather_than_silently_defaulted() {
        let error = validate_tool_arguments("brain_tag_graph", &json!({ "limit": -1 }))
            .expect_err("limit: -1 must not validate");
        assert!(
            format!("{error}").contains("minimum"),
            "the rejection must name the violated bound, not just fail: {error}"
        );
    }

    /// Every advertised default must be a value the schema itself would accept.
    /// A default outside its own `minimum`/`maximum`, or off an `enum`, is
    /// self-contradictory before any handler is involved.
    #[test]
    fn every_advertised_default_satisfies_its_own_constraints() {
        let mut offenders: Vec<String> = Vec::new();

        for tool in all_tool_schemas() {
            let name = tool["name"].as_str().unwrap_or("?").to_string();
            let Some(props) = tool["inputSchema"]["properties"].as_object() else {
                continue;
            };
            for (field, spec) in props {
                let Some(default) = spec.get("default") else {
                    continue;
                };
                if let Some(values) = spec.get("enum").and_then(|v| v.as_array())
                    && !values.contains(default)
                {
                    offenders.push(format!("{name}.{field}: default {default} not in enum"));
                }
                if let Some(number) = default.as_i64() {
                    if let Some(min) = spec.get("minimum").and_then(serde_json::Value::as_i64)
                        && number < min
                    {
                        offenders.push(format!("{name}.{field}: default {number} < minimum {min}"));
                    }
                    if let Some(max) = spec.get("maximum").and_then(serde_json::Value::as_i64)
                        && number > max
                    {
                        offenders.push(format!("{name}.{field}: default {number} > maximum {max}"));
                    }
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "self-contradictory defaults: {offenders:#?}"
        );
    }

    /// The specific regression: `project_context.token_budget` must not
    /// advertise a fixed default at all, because the value it applies depends
    /// on `response_format`. JSON Schema cannot express a conditional default,
    /// and a wrong one is worse than none — the description carries the truth.
    #[test]
    fn project_context_does_not_advertise_a_budget_it_may_not_apply() {
        let tool = all_tool_schemas()
            .into_iter()
            .find(|t| t["name"] == "project_context")
            .expect("project_context must be registered");
        let budget = &tool["inputSchema"]["properties"]["token_budget"];

        assert!(
            budget.get("default").is_none(),
            "the applied budget follows response_format (1000 concise / 3000 detailed), \
             so a fixed `default` here is a claim the handler does not honour"
        );
        let description = budget["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("1000") && description.contains("3000"),
            "the description must state BOTH budgets, since the schema cannot: {description}"
        );

        // And the format that decides it must advertise its own default, which
        // IS expressible and was missing while the prose claimed one.
        assert_eq!(
            tool["inputSchema"]["properties"]["response_format"]["default"],
            json!("concise")
        );
    }

    /// nw-230. MCP error text told the agent to "run the `nestweaver repair`
    /// command" — but `repair` has no MCP tool, so the agent was instructed to
    /// do something it cannot do from the surface it is on. Repair is
    /// destructive publication recovery, so requiring a human is the right
    /// call; the message just has to SAY so.
    #[test]
    fn repair_guidance_names_who_can_actually_run_it() {
        let has_repair_tool = all_tool_schemas()
            .iter()
            .any(|tool| tool["name"].as_str() == Some("repair"));
        assert!(
            !has_repair_tool,
            "a repair TOOL now exists; this test and the guidance text must change together"
        );

        for tool in all_tool_schemas() {
            let name = tool["name"].as_str().unwrap_or("?").to_string();
            let description = tool["description"].as_str().unwrap_or_default().to_string();
            if !description.contains("nestweaver repair") {
                continue;
            }
            assert!(
                description.contains("ASK THE OPERATOR"),
                "{name} tells the agent to run `nestweaver repair` without saying a \
                 human has to do it: {description}"
            );
        }
    }

    /// `get_summary` must advertise the bound it now applies. nw-182 bounded
    /// this on the CLI *because* it is an MCP tool, then bounded only the CLI.
    #[test]
    fn get_summary_advertises_the_bound_it_applies() {
        let tool = all_tool_schemas()
            .into_iter()
            .find(|t| t["name"] == "get_summary")
            .expect("get_summary must be registered");
        let description = tool["inputSchema"]["properties"]["token_budget"]["description"]
            .as_str()
            .unwrap_or_default();

        assert!(
            !description.contains("Default unlimited"),
            "the handler no longer defaults to unlimited: {description}"
        );
        assert!(
            description.contains(&nestweaver_engine::SUMMARY_DEFAULT_TOKEN_BUDGET.to_string()),
            "the advertised default must be the constant the handler uses: {description}"
        );
    }
}

/// nw-232: the three places that encode "this tool mutates" must agree.
///
/// `MUTATING_TOOLS` calls itself the SINGLE canonical list, and most consumers
/// read it. Two did not: the federation router restated a local-only set, and
/// the hybrid stdio server restated a write-tool set as a literal. Both had
/// drifted — `compact_embeddings` was missing from each, so it was advertised
/// in `tools/list` and failed with "unsupported tool for JSON dispatch" for
/// every caller with an upstream configured.
///
/// The stdio copy is now derived. The router's set cannot simply BE the
/// canonical list — it is a deliberate superset that also covers local-only
/// READS like `query_extensions` and the memory tools — so the invariant is
/// containment, and it is pinned here rather than left to discipline.
#[cfg(all(test, feature = "daemon"))]
mod mutating_tool_routing_invariant_tests {
    /// A mutation routed anywhere but locally would write to someone else's
    /// graph. There is no tool for which that is acceptable, so this is a
    /// containment check over the whole canonical list, not a spot check.
    #[test]
    fn every_mutating_tool_is_local_only() {
        use nestweaver_federation::routing::{ToolRouting, tool_routing};

        let stragglers: Vec<&str> = crate::http::MUTATING_TOOLS
            .iter()
            .copied()
            .filter(|tool| tool_routing(tool) != ToolRouting::LocalOnly)
            .collect();

        assert!(
            stragglers.is_empty(),
            "these mutating tools are not routed LocalOnly: {stragglers:?} — they would \
             be sent upstream, or fall to LocalFirst and fail in dispatch_json_rpc with \
             'unsupported tool for JSON dispatch' while still being advertised. Add them \
             to the Admin/mutation arm in nestweaver-federation's routing.rs"
        );
    }
}

/// nw-299(b), the forwarding half. The CLI declined to send `limit`/`members`
/// to the `clusters` tool because the schema set `additionalProperties: false`
/// and declared neither, so sending either failed the WHOLE call and `clusters`
/// stopped working on the daemon route. That precondition is a property of this
/// crate's schema, so it is asserted here rather than described in a comment in
/// `src/main.rs` — a comment cannot notice when it stops being true.
#[cfg(test)]
mod cluster_flag_forwarding_precondition_tests {
    use super::*;

    #[test]
    fn the_clusters_tool_accepts_the_two_flags_the_cli_needs_to_forward() {
        for args in [
            json!({ "limit": 2 }),
            json!({ "members": 3 }),
            json!({ "limit": 2, "members": 3 }),
            json!({ "limit": 0, "members": 0 }),
            json!({ "limit": 2, "members": 3, "resolution": 0.5 }),
        ] {
            validate_tool_arguments("clusters", &args).unwrap_or_else(|e| {
                panic!(
                    "`clusters` must accept {args}: the CLI forwards exactly these \
                     keys, and under `additionalProperties: false` an undeclared \
                     one fails the entire call rather than being ignored — {e}"
                )
            });
        }
    }

    /// The counterweight. If `additionalProperties` were relaxed instead of the
    /// keys being declared, the test above would pass for the wrong reason and
    /// would keep passing if the tool later ignored both flags.
    #[test]
    fn an_undeclared_key_is_still_rejected_by_the_clusters_tool() {
        assert!(
            validate_tool_arguments("clusters", &json!({ "not_a_real_key": 1 })).is_err(),
            "the schema must still be closed — otherwise the test above proves \
             only that nothing is checked"
        );
    }

    /// Why NOT forwarding was never the neutral option it looked like.
    ///
    /// An omitted `limit` does not mean "unbounded" — it means the tool's own
    /// default of 50. So the pre-forwarding daemon route answered
    /// `clusters --limit 200` with fifty communities, and the CLI-side printer
    /// could not restore the other 150 because they never arrived.
    #[test]
    fn an_omitted_limit_is_the_tools_default_not_the_whole_population() {
        assert_eq!(
            read_limit(&json!({}), "limit", 50, 0, RESULT_LIMIT_MAX).unwrap(),
            50,
            "omitting the key applies 50 — declining to forward a caller's \
             larger --limit therefore SUBSTITUTES a smaller bound rather than \
             leaving the payload unbounded"
        );
        assert_eq!(
            read_limit(&json!({ "limit": 0 }), "limit", 50, 0, RESULT_LIMIT_MAX).unwrap(),
            0,
            "and 0 is the spelling of `all`, which the CLI's `--limit 0` must \
             be able to reach"
        );
    }

    /// The bound the CLI must clamp to. `--limit` is an unbounded `usize` in
    /// clap; these are the tool's ceilings, and forwarding a value above them
    /// fails the whole call rather than being clamped for us.
    #[test]
    fn the_tool_rejects_values_above_its_declared_ceilings() {
        assert!(
            validate_tool_arguments("clusters", &json!({ "limit": 5000 })).is_err(),
            "limit is capped at {RESULT_LIMIT_MAX}; a CLI that forwards \
             `--limit 5000` verbatim breaks the daemon route entirely"
        );
        assert!(
            validate_tool_arguments("clusters", &json!({ "members": 500 })).is_err(),
            "members is capped at 200 — a DIFFERENT ceiling from limit, which \
             is exactly the kind of asymmetry a single clamp constant would miss"
        );
    }
}

#[cfg(test)]
mod broken_links_window_tests {
    use super::*;
    use nestweaver_engine::index_markdown_directory_in_memory;
    use std::fs;

    fn make_vault(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        for (rel, content) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        (dir, root)
    }

    /// nw-341: the ascending-confidence sort puts the same-folder (0.95) and
    /// nearest-ancestor (0.92) tiers at the TAIL -- deliberately, so the most
    /// severe rows come first (nw-297). With a cap and no offset, the tiers a
    /// reviewer must inspect are exactly the ones that fall off the end: a
    /// health tool that cannot be used to verify its own fixes.
    ///
    /// Note what this does NOT claim. The cap is not silent -- `read_limit`
    /// REJECTS an out-of-range limit with an explicit error and `total` is
    /// always in the envelope. The residual defects are the missing window and
    /// the missing `truncated` flag, because `tool_brain_broken_links`
    /// hand-rolls its disclosure instead of using the `Bounded` seam.
    #[test]
    fn the_high_confidence_tail_is_reachable_by_offset() {
        // Three 0.70 alias rows sort ahead of one 0.95 same-folder row.
        let (_dir, root) = make_vault(&[
            (
                "f/a.md",
                "# A\n\nSee [[Sibling]], [[al-one]], [[al-two]], [[al-three]].\n",
            ),
            ("f/Sibling.md", "# Different Title Entirely\n"),
            ("g/one.md", "---\naliases: [al-one]\n---\n# One\n"),
            ("g/two.md", "---\naliases: [al-two]\n---\n# Two\n"),
            ("g/three.md", "---\naliases: [al-three]\n---\n# Three\n"),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let page = tool_brain_broken_links(&store, json!({ "limit": 3 })).unwrap();
        assert_eq!(page["total"], 4, "envelope: {page}");
        assert_eq!(
            page["truncated"],
            json!(true),
            "nw-341: a truncated page must SAY it is truncated, through the same \
             (returned, total, truncated) seam every other bounded list uses: {page}"
        );
        let head: Vec<&str> = page["broken_links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["wikilink_text"].as_str().unwrap())
            .collect();
        assert!(
            !head.contains(&"Sibling"),
            "precondition: the 0.95 row must be the one the cap removes, got {head:?}"
        );

        let tail = tool_brain_broken_links(&store, json!({ "limit": 3, "offset": 3 })).unwrap();
        let tail_texts: Vec<&str> = tail["broken_links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["wikilink_text"].as_str().unwrap())
            .collect();
        assert!(
            tail_texts.contains(&"Sibling"),
            "nw-341: the 0.95 same-folder tier must be REACHABLE -- it is precisely \
             what a reviewer of the wikilink tier ladder has to inspect, got {tail_texts:?}"
        );
        assert_eq!(
            tail["total"], 4,
            "total must stay the PRE-offset population: {tail}"
        );
        assert_eq!(
            tail["offset"], 3,
            "the window's origin must be echoed, or a caller paging through \
             cannot tell which page it is holding: {tail}"
        );
    }

    /// nw-341: an offset past the end is an empty page, not an error and not a
    /// wrapped-around page. It must still report the true population.
    #[test]
    fn an_offset_past_the_population_is_an_honest_empty_page() {
        let (_dir, root) = make_vault(&[
            ("f/a.md", "# A\n\nSee [[Sibling]].\n"),
            ("f/Sibling.md", "# Different Title Entirely\n"),
        ]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let page = tool_brain_broken_links(&store, json!({ "offset": 500 })).unwrap();
        assert_eq!(page["returned"], json!(0));
        assert_eq!(page["total"], json!(1), "population, not remainder: {page}");
        assert_eq!(page["truncated"], json!(true));
        assert_eq!(
            page["unresolved"].as_u64().unwrap() + page["low_confidence"].as_u64().unwrap(),
            1,
            "nw-297: classification is over the POPULATION, so it survives any \
             window: {page}"
        );
    }
}

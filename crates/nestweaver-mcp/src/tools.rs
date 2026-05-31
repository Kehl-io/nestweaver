//! Brain tool implementations.
//!
//! Each public `tool_*` function takes the parsed JSON arguments and the
//! shared `GraphStore`, and returns either a structured `serde_json::Value`
//! (returned to MCP clients inside `tools/call` results) or an error.
//!
//! Tool descriptions are written in the "when to use" style — Claude reads
//! these to pick the right tool. Lead with the trigger, not the mechanism.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, anyhow};
use nestweaver_engine::{
    BrainContextResult, DeadCodeConfidence, HybridSearchConfig, SummaryLevel, affected_tests,
    analyze_blast_radius, attach_cluster_ids, attach_communities, broken_links,
    build_brain_context_hybrid_with_aliases, compute_clusters, detect_changes_impact,
    detect_dead_code, doc_stats, expand_query_with_aliases, filter_by_target, find_bridge_nodes,
    find_hub_nodes, generate_guide, generate_summaries, get_all_properties, get_last_indexed_at,
    index_directory, index_markdown_directory, investigate, investigate_expand,
    investigate_hydrate, load_alias_sidecar, load_clusters, load_extensions, memory_consolidate,
    memory_lint, memory_related, orphan_documents, parse_iso8601_to_epoch, populate_inline_bodies,
    query_by_property, render_text, save_extensions, search_symbols, set_property, tag_graph,
    tag_graph_all, topic_clusters, truncate_to_budget,
};
use nestweaver_store::{GraphStore, TantivyIndex};
use serde_json::{Value, json};

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
pub fn tool_list(lite: bool) -> Value {
    let mut tools = vec![
        tool_schema_brain_context(),
        tool_schema_brain_search(),
        tool_schema_note_get(),
        tool_schema_backlinks(),
        tool_schema_brain_status(),
        tool_schema_brain_add_source(),
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
    ];
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

    // F16: serve cacheable read tools from (or populate) the response cache.
    // Correctness rests on the cache KEY — see `maybe_cached`.
    if is_cacheable_tool(name) && !cache_bypassed(&args) {
        return maybe_cached(store, tantivy, name, args);
    }

    dispatch_uncached(store, tantivy, name, args)
}

/// The actual tool dispatch table, after cache handling.
fn dispatch_uncached(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
) -> Result<Value, anyhow::Error> {
    match name {
        "brain_context" => tool_brain_context(store, tantivy, args),
        "brain_search" => tool_brain_search(store, tantivy, args),
        "note_get" => tool_note_get(store, args),
        "backlinks" => tool_backlinks(store, args),
        "brain_status" => tool_brain_status(store, tantivy),
        "brain_add_source" => tool_brain_add_source(store, args),
        "cross_repo_contracts" => tool_cross_repo_contracts(store, args),
        "brain_impact" => tool_brain_impact(store, args),
        "brain_guide" => tool_brain_guide(store, args),
        "flow_trace" => tool_flow_trace(store, args),
        "detect_changes" => tool_detect_changes(store, args),
        "clusters" => tool_clusters(store, args),
        "stale_check" => tool_stale_check(store),
        "set_extension" => tool_set_extension(args),
        "query_extensions" => tool_query_extensions(args),
        "brain_diff" => tool_brain_diff(store, args),
        "project_context" => tool_project_context(store, tantivy, args),
        "dead_code" => tool_dead_code(store, args),
        "hub_nodes" => tool_hub_nodes(store, args),
        "bridge_nodes" => tool_bridge_nodes(store, args),
        "blast_radius" => tool_blast_radius(store, args),
        "get_summary" => tool_get_summary(store, args),
        "read_symbols" => tool_read_symbols(store, args),
        "regex_search" => tool_regex_search(store, args),
        "count_patterns" => tool_count_patterns(store, args),
        "brain_broken_links" => tool_brain_broken_links(store, args),
        "brain_orphan_documents" => tool_brain_orphan_documents(store, args),
        "brain_topic_clusters" => tool_brain_topic_clusters(store, args),
        "brain_tag_graph" => tool_brain_tag_graph(store, args),
        "brain_doc_stats" => tool_brain_doc_stats(store, args),
        "affected_tests" => tool_affected_tests(store, args),
        "investigate" => tool_investigate(store, tantivy, args),
        "investigate_expand" => tool_investigate_expand(store, args),
        "investigate_hydrate" => tool_investigate_hydrate(store, args),
        "contract_drift" => tool_contract_drift(store, args),
        "brain_memory_lint" => tool_brain_memory_lint(store),
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
///   the cache's own stats, which must not be frozen.
const CACHEABLE_TOOLS: &[&str] = &[
    "brain_context",
    "brain_search",
    "note_get",
    "backlinks",
    "cross_repo_contracts",
    "brain_impact",
    "flow_trace",
    "clusters",
    "query_extensions",
    "brain_diff",
    "project_context",
    "dead_code",
    "hub_nodes",
    "bridge_nodes",
    "blast_radius",
    "get_summary",
    "read_symbols",
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
    let cache = nestweaver_engine::load_filemeta_cache(&filemeta_path);
    nestweaver_store::cache::scope_digest_from_hashes(
        cache
            .iter()
            .map(|(p, m)| (p.as_str(), m.content_hash.as_str())),
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
fn maybe_cached(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
) -> Result<Value, anyhow::Error> {
    let Ok(db_path) = current_db_path(store) else {
        return dispatch_uncached(store, tantivy, name, args);
    };

    let max_mb = CACHE_MAX_SIZE_MB.with(|c| c.get());
    let mut cache = nestweaver_store::cache::ResponseCache::open(&db_path, max_mb);
    let key = nestweaver_store::cache::ResponseCache::key(name, &args);
    let generation = store.graph_generation();
    let scope_digest = whole_db_scope_digest(&db_path);

    if let Some(bytes) = cache.get(key, generation, scope_digest) {
        CACHE_HITS.with(|c| c.set(c.get() + 1));
        cache.save(); // persist updated LRU last_access
        let value: Value =
            serde_json::from_slice(&bytes).with_context(|| "decode cached response")?;
        return Ok(value);
    }

    CACHE_MISSES.with(|c| c.set(c.get() + 1));
    let result = dispatch_uncached(store, tantivy, name, args)?;
    if let Ok(bytes) = serde_json::to_vec(&result) {
        cache.insert(key, name, &bytes, generation, scope_digest);
        cache.save();
    }
    Ok(result)
}

/// Session cache stats `(size_bytes, entries, hit_rate)` for `brain_status`.
/// `hit_rate` is `hits / (hits + misses)` over this process's lifetime;
/// it is `None` when no cacheable calls have been made yet. Honest framing:
/// this hit-rate is unproven and should be measured in real usage.
fn cache_stats(db_path: &Path) -> (u64, usize, Option<f64>) {
    let max_mb = CACHE_MAX_SIZE_MB.with(|c| c.get());
    let cache = nestweaver_store::cache::ResponseCache::open(db_path, max_mb);
    let hits = CACHE_HITS.with(|c| c.get());
    let misses = CACHE_MISSES.with(|c| c.get());
    let total = hits + misses;
    let hit_rate = if total > 0 {
        Some(hits as f64 / total as f64)
    } else {
        None
    };
    (cache.size_bytes(), cache.len(), hit_rate)
}

/// F5: read a symbol's source span (not the whole file). Resolves UIDs/names/
/// FQNs, optionally includes adjacent symbols, and respects a token budget.
fn tool_read_symbols(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let targets: Vec<String> = args
        .get("targets")
        .or_else(|| args.get("uids_or_fqns"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
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
    let res = nestweaver_engine::read_symbols::read_symbols(
        store,
        &targets,
        &root,
        neighbors,
        token_budget,
    );
    Ok(serde_json::to_value(res)?)
}

fn tool_schema_read_symbols() -> Value {
    json!({
        "name": "read_symbols",
        "description": "Use when you need to READ a symbol's source — return just that symbol's span (start_line..end_line), not the whole file. Far cheaper in tokens than reading entire files. Accepts UIDs (sym:...), bare names, or FQNs; an ambiguous name returns candidate UIDs to disambiguate. Use include_neighbors to also return adjacent symbols in the same file, and token_budget to cap output. `root` is the repository root used to resolve file paths (defaults to the server's working directory).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "targets": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Symbol UIDs (sym:...), names, or FQNs to read."
                },
                "include_neighbors": {
                    "type": "integer",
                    "description": "Include N adjacent symbols in the same file (default 0)."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Approximate token cap for the combined output."
                },
                "root": {
                    "type": "string",
                    "description": "Repository root for resolving file paths (default: server working directory)."
                }
            },
            "required": ["targets"]
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
    Ok(serde_json::to_value(res)?)
}

fn tool_schema_regex_search() -> Value {
    json!({
        "name": "regex_search",
        "description": "Use when you need to find text by REGEX across indexed content (markdown section bodies, note titles, code symbol signatures) — a first-party replacement for shelling out to rg/grep. Runs a real Rust `regex` against the indexed text, accelerated by a trigram pre-filter when one was built (`nestweaver index --with-trigrams`). When no trigram index exists, or the pattern has no usable literals (e.g. `.{4,}`), it falls back to scanning all candidate text — still correct, just slower, and `scanned_fallback` is set.\n\nDo NOT use for fuzzy/semantic lookup — use brain_search. Returns `{results:[{uid,kind,title,location,line,snippet}], truncated, scanned_fallback}`. `truncated` is set when the candidate cap (5000) or time budget was hit.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust regex pattern. Example: \"fn\\\\s+authenticate\" or \"(?i)todo\"." },
                "path_prefix": { "type": "string", "description": "Restrict to nodes whose file path starts with this prefix." },
                "kinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict to these node kinds: Section, Note, Symbol (case-insensitive)."
                },
                "limit": { "type": "integer", "description": "Maximum results to return. Default: unlimited (capped by the candidate budget)." },
                "max_millis": { "type": "integer", "description": "Wall-clock time budget in milliseconds. Default 2000." }
            },
            "required": ["pattern"]
        }
    })
}

/// F4: counts-only companion to regex_search. Counts matches per pattern across
/// indexed text and reports the busiest files.
fn tool_count_patterns(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let patterns = parse_string_array(&args, "patterns")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow!("'patterns' must be a non-empty array of regex strings"))?;
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
        "description": "Use when you only need COUNTS of regex matches across indexed text, not the matches themselves — e.g. \"how many sections mention TODO?\" Counts one match per node and reports, per pattern, `{pattern, total_matches, files_matched, top_files:[{path,count}]}`. Reuses the same trigram pre-filter as regex_search and the same full-scan fallback when no literals/index are available.\n\nDo NOT use when you need the matching text — use regex_search. Pass multiple patterns to compare counts in one call.",
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
    let links = broken_links(store, max_suggestions)?;
    Ok(json!({ "broken_links": serde_json::to_value(&links)?, "total": links.len() }))
}

fn tool_schema_brain_broken_links() -> Value {
    json!({
        "name": "brain_broken_links",
        "description": "Use when auditing a markdown vault for wikilinks that did not resolve cleanly — links whose target is ambiguous or low-confidence (confidence < 1.0). For each, returns the source note, the link text, and suggested target note UIDs (fuzzy title match) so you can repair the link. Returns empty when there is no vault. Output: `{broken_links:[{source_uid, source_path, wikilink_text, confidence, suggested_target_uids:[...]}], total}`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "max_suggestions": {
                    "type": "integer",
                    "description": "Max suggested target UIDs per broken link (default 5).",
                    "default": 5
                }
            }
        }
    })
}

fn tool_brain_orphan_documents(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let vault = args.get("vault").and_then(|v| v.as_str());
    let path_prefix = args.get("path_prefix").and_then(|v| v.as_str());
    let allowlist = parse_string_array(&args, "allowlist").unwrap_or_default();
    let orphans = orphan_documents(store, vault, path_prefix, &allowlist)?;
    Ok(json!({ "orphans": serde_json::to_value(&orphans)?, "total": orphans.len() }))
}

fn tool_schema_brain_orphan_documents() -> Value {
    json!({
        "name": "brain_orphan_documents",
        "description": "Use to find notes that are disconnected from the knowledge graph — notes with ZERO inbound and ZERO outbound wikilinks. These are candidates to link up or archive. Index/MOC notes are excluded via a configurable allowlist (default includes Projects.md, index.md, README.md, _brain/index.md, and any note whose path/title contains \"MOC\"). Optional `vault` and `path_prefix` filters. Returns empty when there is no vault. Output: `{orphans:[{uid, title, file_path}], total}`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "vault": { "type": "string", "description": "Restrict to this vault UID." },
                "path_prefix": { "type": "string", "description": "Restrict to notes whose file path starts with this prefix." },
                "allowlist": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Note paths/titles to exclude (overrides the default index/MOC allowlist when provided)."
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
    let clusters = topic_clusters(store, resolution)?;
    Ok(json!({ "clusters": serde_json::to_value(&clusters)?, "total": clusters.len() }))
}

fn tool_schema_brain_topic_clusters() -> Value {
    json!({
        "name": "brain_topic_clusters",
        "description": "Use to discover the thematic structure of a markdown vault: runs Leiden community detection over the note-to-note wikilink graph and groups notes into topics. Each cluster is labelled by its most central member (highest PageRank, then highest link degree). Returns empty when there is no vault. Output: `{clusters:[{cluster_id, members:[note_uid], label}], total}`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "resolution": {
                    "type": "number",
                    "description": "Leiden resolution — higher yields more, smaller clusters (default 0.5).",
                    "default": 0.5
                }
            }
        }
    })
}

fn tool_brain_tag_graph(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // `tag` is optional. When present we accept only a string (reject other
    // JSON types); when absent we return the whole tag co-occurrence graph.
    match args.get("tag") {
        Some(Value::Null) | None => {
            let tags = tag_graph_all(store)?;
            Ok(json!({ "tags": serde_json::to_value(&tags)? }))
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
        "description": "Use to understand how tags relate to each other in a markdown vault. Two modes. (1) With `tag`: returns that focus tag's note count plus the tags that co-occur with it (appear on the same notes), ranked by shared-note count. Output: `{tag, count, co_occurring:[{tag, count}]}`. (2) Without `tag`: returns the WHOLE tag co-occurrence graph — one entry per distinct tag, sorted by note count descending then name — for taxonomy-drift detection. Output: `{tags:[{tag, count, co_occurring:[{tag, count}]}]}`. The `tag` argument may include or omit a leading `#`. Returns count 0 / empty when the tag or vault is absent.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tag": { "type": "string", "description": "Optional focus tag (with or without leading #). When omitted, returns the full tag co-occurrence graph for all tags." }
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
        "description": "Use for a one-shot health summary of a markdown vault's document graph. Composes the other brain document tools plus counts. Returns all seven keys even on an empty vault (zeros / empty collections). Output: `{total_notes, total_wikilinks, broken_wikilinks, orphans, avg_outdegree, top_tags:[{tag,count}], notes_by_year:{year:count}}`.",
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

fn tool_brain_memory_lint(store: &GraphStore) -> Result<Value, anyhow::Error> {
    let report = memory_lint(store, now_epoch_secs())?;
    Ok(serde_json::to_value(&report)?)
}

fn tool_schema_brain_memory_lint() -> Value {
    json!({
        "name": "brain_memory_lint",
        "description": "Use to audit a markdown 'memory bank' vault for health problems. Runs SEVEN checks and returns them keyed: `stale` (notes marked status:active but unmodified for >90 days), `contradictions` (Supersedes cycles like A→B→A), `orphans` (notes with no inbound/outbound wikilinks), `broken_wikilinks` (ambiguous/low-confidence links), `supersession_chains` (a superseded note still actively linked), `schema_drift` (note frontmatter keys missing vs the _templates/<kind>.md template), `dangling_relationships` (a typed relationship whose target note does not exist). All keys always present; empty on a no-vault DB. Output: `{stale:[...], contradictions:[...], orphans:[...], broken_wikilinks:[...], supersession_chains:[...], schema_drift:[...], dangling_relationships:[...]}`.",
        "inputSchema": { "type": "object", "properties": {} }
    })
}

fn tool_brain_memory_consolidate(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let apply = args.get("apply").and_then(|v| v.as_bool()).unwrap_or(false);
    let manifest = memory_consolidate(store, apply, now_epoch_secs())?;
    Ok(serde_json::to_value(&manifest)?)
}

fn tool_schema_brain_memory_consolidate() -> Value {
    json!({
        "name": "brain_memory_consolidate",
        "description": "Use to propose promotions of vault notes UP the memory tiers (daily logs → ideas → project files). DRY-RUN BY DEFAULT — it never mutates files. Proposes: (1) a daily log (under _logs/) wikilinked from >=3 distinct idea notes and older than 14 days → _ideas candidate; (2) an idea (under _ideas/) referenced from BOTH a project's sync.md and status.md → project-file candidate. Set `apply:true` to opt into write-mode; today that is an explicit no-op stub that records a warning and still mutates nothing. Output: `{dry_run, applied, proposals:[{source_uid, source_title, source_path, promote_to, rationale, evidence:[...]}], warnings:[...]}`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "apply": {
                    "type": "boolean",
                    "description": "Opt into write-mode (currently a no-op stub that warns; default false = safe dry-run).",
                    "default": false
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
        "description": "Use to walk the TYPED relationship graph from a note — Supersedes / DependsOn / CausedBy / RelatesTo — without the noise of generic wikilinks. Breadth-first from `uid` over the chosen `edge_types` (default all four) to `depth` hops (default 2). Returns only the typed neighbours. Empty on unknown node / no-vault DB. Output: `{related:[{uid, title, file_path, depth, via_edge}], total}`.",
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
        "description": "Use FIRST when you need codebase or knowledge-base context around a symbol, note, tag, or topic. Runs Personalized PageRank over the unified code + notes graph from the given seeds and returns ranked, mixed-kind results (Symbol, Note, Section, Tag, Heading) within a token budget. This is cheaper than reading files — get the structural picture before opening anything.\n\nDo NOT use for simple text search — use brain_search instead. Do NOT use when you already have a specific note UID and want its full body — use note_get instead.\n\nThe `seeds` parameter accepts note titles (e.g. \"Architecture\"), tag names (\"#status/active\"), symbol names (\"greet\"), free-text terms, or UIDs (sym:, note:, head:, sec:, tag:). Example: seeds=[\"AuthService\", \"#security\"] returns the authentication service symbol and all security-tagged notes, plus their graph neighbors ranked by relevance. Use `response_format` \"concise\" for a quick overview (names and relationships only) or \"detailed\" (default) for full metadata including file paths and relevance scores.",
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
    let defaults = HybridSearchConfig::default();
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
        // The MCP server has no instance-config handle here, so use the
        // built-in [response] defaults (threshold 0.75, cap 800 tokens).
        let response_config = nestweaver_engine::ResponseConfig::default();
        let root = args
            .get("root")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        populate_inline_bodies(
            store,
            &mut result.connected,
            &root,
            response_config.inline_body_threshold,
            response_config.inline_max_body_tokens,
            Some(token_budget),
        );
    }

    let (cut, used_tokens) = budgeted_cut(&result.connected, token_budget);

    let concise = is_concise(&args);

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

fn budgeted_cut(nodes: &[nestweaver_engine::BrainNode], budget: usize) -> (usize, usize) {
    let mut used = 0usize;
    let mut taken = 0usize;
    for n in nodes {
        let cost = render_cost(n);
        if used + cost > budget {
            break;
        }
        used += cost;
        taken += 1;
    }
    (taken, used)
}

fn render_cost(n: &nestweaver_engine::BrainNode) -> usize {
    // UID + title + kind + location + relevance (~10 chars) + JSON overhead
    (n.uid.len() + n.title.len() + n.kind.len() + n.location.len() + 10 + 80).div_ceil(4)
}

// ── 2. brain_search ─────────────────────────────────────────────────────────

fn tool_schema_brain_search() -> Value {
    json!({
        "name": "brain_search",
        "description": "Use when you need to find specific notes, headings, sections, tags, or code symbols by keyword or phrase. Performs BM25 full-text search across note titles, heading text, section bodies, and tag names, plus substring search across code symbol names, returning ranked hits (best match first) with a kind discriminator so you can tell note/symbol hits apart.\n\nDo NOT use for structural context (\"what's connected to X\" or \"what calls Y\") — use brain_context instead. Do NOT use to read a full note body — use note_get after finding the note here.\n\nThe `query` parameter accepts natural language (e.g. \"authentication flow\") or exact terms (e.g. \"AuthService\"). Results include UIDs you can pass directly to note_get or brain_context as seeds. Use `response_format` \"concise\" to get just titles and kinds (good for scanning many results), or \"detailed\" (default) to include scores and location details.",
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
                    "default": 20
                },
                "response_format": {
                    "type": "string",
                    "enum": ["concise", "detailed"],
                    "default": "detailed",
                    "description": "\"concise\" returns note titles and kinds only; \"detailed\" (default) adds section text excerpts, BM25 scores, and vault UIDs."
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
        .unwrap_or(20);
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
    let (mut note_results, engine) = if let Some(idx) = tantivy {
        // Tantivy BM25 path (preferred).
        let raw_limit = limit * 5;
        let hits = if prf {
            let (hits, terms) = idx
                .search_prf(
                    &query,
                    raw_limit,
                    nestweaver_engine::query::nestweaver_store_stoplist(),
                )
                .map_err(|e| anyhow!("tantivy prf search: {e}"))?;
            expansion_terms = terms;
            hits
        } else {
            idx.search(&query, raw_limit)
                .map_err(|e| anyhow!("tantivy search: {e}"))?
        };
        let results = group_search_hits_by_note(store, &hits, limit, concise);
        (results, "bm25")
    } else {
        // Substring fallback: search note titles, heading text, and section bodies.
        let needle = query.to_lowercase();
        let raw_limit = limit * 5;

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
            if n.title.to_lowercase().contains(&needle) && raw_hits.len() < raw_limit {
                raw_hits.push(RawHit {
                    kind: "note".to_string(),
                    title: n.title.clone(),
                    note_uid: n.uid.clone(),
                    score: 1.0,
                });
            }
        }

        // Heading text matches.
        if raw_hits.len() < raw_limit {
            let headings = store.list_all_headings().context("list_all_headings")?;
            for h in &headings {
                if h.text.to_lowercase().contains(&needle) && raw_hits.len() < raw_limit {
                    raw_hits.push(RawHit {
                        kind: "heading".to_string(),
                        title: h.text.clone(),
                        note_uid: h.note_uid.clone(),
                        score: 0.8,
                    });
                }
            }
        }

        // Section body matches.
        if raw_hits.len() < raw_limit {
            let sections = store.list_all_sections().context("list_all_sections")?;
            for s in &sections {
                if s.text_content.to_lowercase().contains(&needle) && raw_hits.len() < raw_limit {
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
                    .unwrap_or_else(|_| group.note_uid.clone());
            }
        }

        note_order.sort_by(|a, b| {
            let sa = groups.get(a).map(|g| g.best_score).unwrap_or(0.0);
            let sb = groups.get(b).map(|g| g.best_score).unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let results: Vec<Value> = note_order
            .iter()
            .take(limit)
            .filter_map(|nuid| groups.get(nuid))
            .map(|g| {
                if concise {
                    json!({
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

        (results, "substring")
    };

    // ── Code symbol results ─────────────────────────────────────────────

    // Collect note titles (lowercased) for dedup against code symbols.
    let seen_titles: HashSet<String> = note_results
        .iter()
        .filter_map(|v| v.get("title").and_then(|t| t.as_str()))
        .map(|t| t.to_lowercase())
        .collect();

    // Search code symbols and merge into results, skipping duplicates.
    if let Ok(code_hits) = search_symbols(store, &query, limit) {
        for sym in &code_hits {
            if seen_titles.contains(&sym.name.to_lowercase()) {
                continue;
            }
            let location = format!("{}:{}", sym.file_path, sym.start_line);
            let kind = format!("Symbol/{}", sym.kind);
            if concise {
                note_results.push(json!({
                    "kind": kind,
                    "title": sym.name,
                    "location": location,
                }));
            } else {
                note_results.push(json!({
                    "uid": sym.uid,
                    "kind": kind,
                    "title": sym.name,
                    "score": 0.5,
                    "location": location,
                }));
            }
        }
    }

    // Stable sort by score descending so notes and symbols interleave by relevance.
    note_results.sort_by(|a, b| {
        let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Feature F17: rerank the top-N before truncation. OFF by default →
    // byte-identical output. Detailed mode only (concise rows carry no UID to
    // key the reorder on). The default scorer is a transparent monotonic
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

    note_results.truncate(limit);

    // Feature F8: embed high-relevance bodies inline when opted in. Off by
    // default. Concise mode carries no UID/score, so inline bodies are skipped
    // there. Bodies are computed via the shared engine helper for parity with
    // brain_context (normalized-relevance threshold + per-body truncation).
    let include_bodies = args
        .get("include_bodies")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if include_bodies && !concise {
        let response_config = nestweaver_engine::ResponseConfig::default();
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
                })
            })
            .collect();
        populate_inline_bodies(
            store,
            &mut nodes,
            &root,
            response_config.inline_body_threshold,
            response_config.inline_max_body_tokens,
            None,
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

    let total = note_results.len();
    let mut response = json!({
        "query": query,
        "engine": engine,
        "results": note_results,
        "total_matches": total,
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
fn group_search_hits_by_note(
    store: &GraphStore,
    hits: &[nestweaver_store::SearchHit],
    limit: usize,
    concise: bool,
) -> Vec<Value> {
    use std::collections::HashMap;

    struct NoteGroup {
        note_uid: String,
        best_score: f32,
        best_title: String,
        vault_uid: String,
        matched_headings: Vec<String>,
    }

    let mut groups: HashMap<String, NoteGroup> = HashMap::new();
    let mut note_order: Vec<String> = Vec::new();

    for h in hits {
        // Determine the parent note UID based on the hit kind.
        let parent_note_uid = match h.kind.as_str() {
            "note" => h.uid.clone(),
            "heading" => store
                .lookup_heading(&h.uid)
                .map(|hd| hd.note_uid)
                .unwrap_or_else(|_| h.uid.clone()),
            "section" => store
                .lookup_section(&h.uid)
                .map(|s| s.note_uid)
                .unwrap_or_else(|_| h.uid.clone()),
            _ => h.uid.clone(),
        };

        let group = groups.entry(parent_note_uid.clone()).or_insert_with(|| {
            note_order.push(parent_note_uid.clone());
            NoteGroup {
                note_uid: parent_note_uid.clone(),
                best_score: 0.0,
                best_title: String::new(),
                vault_uid: h.vault_uid.clone(),
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
    for group in groups.values_mut() {
        if group.best_title.is_empty() {
            group.best_title = store
                .lookup_note(&group.note_uid)
                .map(|n| n.title)
                .unwrap_or_else(|_| group.note_uid.clone());
        }
    }

    // Sort by best_score descending, then take `limit`.
    note_order.sort_by(|a, b| {
        let sa = groups.get(a).map(|g| g.best_score).unwrap_or(0.0);
        let sb = groups.get(b).map(|g| g.best_score).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    note_order
        .iter()
        .take(limit)
        .filter_map(|nuid| groups.get(nuid))
        .map(|g| {
            if concise {
                json!({
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
                    "vault_uid": g.vault_uid,
                    "matched_headings": g.matched_headings,
                })
            }
        })
        .collect()
}

// ── 3. note_get ─────────────────────────────────────────────────────────────

fn tool_schema_note_get() -> Value {
    json!({
        "name": "note_get",
        "description": "Use after brain_context or brain_search indicates a specific note is relevant and you need its full markdown body or specific sections. Loads the note content from disk plus structural metadata (frontmatter, heading outline, tags, outgoing wikilink count).\n\nDo NOT use to discover notes — use brain_search or brain_context first, then call note_get with the UID or title from those results. Do NOT use for code symbols — this is for markdown notes only.\n\nPass either `uid` (e.g. \"note:vlt:MyVault:abc123\") or `title` (case-insensitive, returns first match). Use the `sections` parameter to retrieve only specific named sections instead of the full body — this is much more token-efficient for large notes. Example: sections=[\"Architecture\", \"API Design\"] returns only those two heading sections.",
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
        store.lookup_note(uid).context("lookup_note")?
    } else if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        let matches = store
            .lookup_notes_by_title(title)
            .context("lookup_notes_by_title")?;
        match matches.into_iter().next() {
            Some(n) => n,
            None => return Err(anyhow!("no note found with title '{title}'")),
        }
    } else {
        return Err(anyhow!("provide either 'uid' or 'title'"));
    };

    // Load all headings and sections (needed for both outline and section filter).
    let headings_raw = store.headings_in_note(&note.uid).unwrap_or_default();
    let sections_raw = store.sections_in_note(&note.uid).unwrap_or_default();

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
        "description": "Use to find every note that wiki-links TO a specific target note. Returns each source note with the linking section, confidence score, and display text. This reveals the reverse link graph — which notes reference the target.\n\nDo NOT use for forward links (what a note links to) — read the note body with note_get instead. Do NOT use for code symbol dependencies — use brain_impact or flow_trace instead.\n\nPass either `uid` (e.g. \"note:vlt:MyVault:abc123\") or `title` (case-insensitive, first match). Example: backlinks for \"API Design\" returns all notes that contain [[API Design]] wikilinks, along with the source note path and the confidence of each link resolution.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Note UID (e.g. note:vlt:MyVault:abc123). Preferred for unambiguous lookup." },
                "title": { "type": "string", "description": "Note title (case-insensitive match). Returns backlinks for the first matching note." }
            }
        }
    })
}

fn tool_backlinks(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let target_uid = if let Some(uid) = args.get("uid").and_then(|v| v.as_str()) {
        uid.to_string()
    } else if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        let matches = store.lookup_notes_by_title(title)?;
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
        "description": "Use at the start of a session to see what knowledge sources are indexed and available. Returns counts for vaults (with per-vault note counts and last-indexed timestamps), notes, headings, sections, tags, wikilinks, and code repos. When interaction tracking is enabled (--track-interactions), also reports interaction memory status including query count and memory age. This is a cheap metadata-only call with no parameters.\n\nDo NOT use to search for content — use brain_search. Do NOT use to check if the index is stale — use stale_check instead.\n\nCall this first to verify that the expected vaults and repos are loaded before issuing queries. If counts are zero, the user may need to run brain_add_source to index their content.",
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
    let vaults = store.list_vaults(None).unwrap_or_default();
    let notes = store.count_notes().unwrap_or(0);
    let headings = store.count_headings().unwrap_or(0);
    let sections = store.count_sections().unwrap_or(0);
    let tags = store.count_tags().unwrap_or(0);
    let wikilinks = store.count_wikilink_edges().unwrap_or(0);
    let repos = store.list_repos(None).unwrap_or_default();

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
                "name": v.name,
                "root_path": v.root_path,
                "note_count": note_count,
                "last_indexed": last_indexed,
                "last_indexed_source": last_indexed_source,
            })
        })
        .collect();
    let repos_json: Vec<Value> = repos
        .iter()
        .map(|r| json!({ "url": r.url, "sha": r.indexed_sha }))
        .collect();

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
    }))
}

// ── 6. brain_add_source ─────────────────────────────────────────────────────

fn tool_schema_brain_add_source() -> Value {
    json!({
        "name": "brain_add_source",
        "description": "Use when the user mentions notes, vaults, or repos that are not yet indexed, or when brain_status shows missing sources. Auto-detects the source type: Obsidian vault (if .obsidian/ is present), code repo (if .git/ is present), or plain markdown folder, and indexes it into the brain graph.\n\nDo NOT use if the source is already indexed — check brain_status first. This tool requires the MCP server to be started with --allow-mcp-add-sources; it will return an error if that flag was not set.\n\nThe `path` parameter must be an absolute path or start with ~/ (tilde is expanded to $HOME). Example: path=\"~/Documents/Obsidian/MyVault\" indexes the vault and returns counts for notes, headings, sections, tags, and wikilinks created. The optional `name` parameter sets a friendly display name for vaults (defaults to the directory name).",
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
    if !ALLOW_ADD_SOURCES.with(|c| c.get()) {
        return Err(anyhow!(
            "brain_add_source is disabled. Start the MCP server with \
             --allow-mcp-add-sources to enable runtime source indexing."
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
    let has_git = path.join(".git").is_dir();
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
        let result =
            index_markdown_directory(path, &db_path, "default", &name).context("index vault")?;
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
        let url = format!("file://{}", path.display());
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
}

// ── 7. cross_repo_contracts ─────────────────────────────────────────────────

fn tool_schema_cross_repo_contracts() -> Value {
    json!({
        "name": "cross_repo_contracts",
        "description": "Use when modifying a symbol that may be shared across multiple repositories to understand cross-repo blast radius. Returns other repos that reference or define the same symbol name, with confidence scores and link types (e.g. imports, re-exports, API contracts).\n\nDo NOT use for single-repo impact analysis — use brain_impact instead. Do NOT use for general search — use brain_search. This tool is only useful when multiple repos are indexed in the same brain.\n\nPass either `uid` (e.g. \"sym:repo:...:hash:42\") or `name` (e.g. \"UserService\"). Example: cross_repo_contracts for \"PaymentAPI\" returns all repos that import or implement that symbol, with confidence scores indicating match quality.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "uid": { "type": "string", "description": "Symbol UID (e.g. sym:repo:...:hash:42). Preferred for unambiguous lookup." },
                "name": { "type": "string", "description": "Symbol name (e.g. \"UserService\"). Uses first match if multiple symbols share the name." }
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

    Ok(json!({
        "uid": uid,
        "count": rows.len(),
        "note": "Links are hypotheses, not ground truth — check confidence. \
                 link_type \"contract\" denotes an implemented API contract.",
        "contracts": rows,
    }))
}

// ── 35. contract_drift ──────────────────────────────────────────────────────

fn tool_schema_contract_drift() -> Value {
    json!({
        "name": "contract_drift",
        "description": "Use to audit API contract drift in the indexed code graph: routes/methods/operations DECLARED in a spec file (OpenAPI/Swagger, .proto, GraphQL) but not implemented by any handler, and routes IMPLEMENTED by a Spring/NestJS handler but declared in no spec.\n\nContract links are HYPOTHESES, not ground truth — derived from spec parsing and framework handler heuristics (same-repo only). Use this to spot missing endpoints or undocumented APIs.\n\nOptional `repo` filters to a single repo by UID. Returns two buckets: declared_not_implemented and implemented_not_declared, each a list of contract UIDs (e.g. \"contract:http:POST:/v1/approvals\").",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Optional repo UID to scope the analysis to a single repository." }
            }
        }
    })
}

fn tool_contract_drift(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let repo = args.get("repo").and_then(|v| v.as_str());
    let report = nestweaver_engine::contracts::drift_for_store(store, repo)
        .map_err(|e| anyhow!("drift_for_store: {e}"))?;
    Ok(json!({
        "note": "Contract links are hypotheses, not ground truth.",
        "declared_not_implemented": report.declared_not_implemented,
        "implemented_not_declared": report.implemented_not_declared,
        "clean": report.is_clean(),
    }))
}

// ── 8. brain_impact ─────────────────────────────────────────────────────────

fn tool_schema_brain_impact() -> Value {
    json!({
        "name": "brain_impact",
        "description": "Use BEFORE modifying a function, class, or interface to understand what might break. Performs reverse-dependency traversal from the target symbol and returns all symbols that directly or transitively call, import, or extend it, grouped by depth level.\n\nDo NOT use for forward call chains (what does this function call?) — use flow_trace instead. Do NOT use for cross-repo impact — use cross_repo_contracts. Do NOT use for file-level change impact — use detect_changes instead.\n\nThe `symbol` parameter accepts a symbol name (e.g. \"validateUser\") or a full UID (e.g. \"sym:repo:...:hash:42\"). The `depth` parameter controls how many hops to traverse (default 3). Use `response_format` \"concise\" to get just affected symbol names, or \"detailed\" (default) to include file paths, edge types (calls/imports/extends), and confidence scores.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name (e.g. \"validateUser\") or full UID (e.g. \"sym:repo:...:hash:42\"). Names are resolved via first-match lookup." },
                "depth": { "type": "integer", "description": "Max traversal depth. Higher values find more transitive dependents but take longer. Default 3.", "default": 3 },
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

fn tool_brain_impact(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'symbol' is required"))?;
    let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
    let concise = is_concise(&args);

    let uid = resolve_symbol_uid(store, symbol)?;

    let nodes = store.impact(&uid, depth, 0.0)?;

    let rows: Vec<Value> = nodes
        .iter()
        .map(|n| {
            if concise {
                json!({
                    "name": n.name,
                    "depth": n.depth,
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
                })
            }
        })
        .collect();

    Ok(json!({
        "target": uid,
        "impact_nodes": rows,
        "total": rows.len(),
    }))
}

// ── 9. brain_guide ──────────────────────────────────────────────────────────

fn tool_schema_brain_guide() -> Value {
    json!({
        "name": "brain_guide",
        "description": "Use at the very start of a session to get a comprehensive overview of the indexed codebase and knowledge base. Returns an auto-generated intelligence guide covering all indexed repos (with language breakdowns and key entry points), vaults (with note counts and topics), cross-repo relationships, and a summary of available brain tools with usage tips.\n\nDo NOT use for specific queries — use brain_context or brain_search instead. This is a read-once orientation tool, not a query tool. No parameters are required.\n\nThe guide is regenerated from the current graph state on each call, so it always reflects the latest indexed content. Call this before brain_context to understand what seeds are available.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

fn tool_brain_guide(store: &GraphStore, _args: Value) -> Result<Value, anyhow::Error> {
    // The MCP server does not hold an InstanceConfig at runtime; cross-repo
    // edges from the graph are still included via the store query.
    let guide = generate_guide(store, None)?;
    Ok(json!({ "guide": guide }))
}

// ── 10. flow_trace ─────────────────────────────────────────────────────────

fn tool_schema_flow_trace() -> Value {
    json!({
        "name": "flow_trace",
        "description": "Use when you need to understand execution flow: what functions a symbol calls, what those call, and so on. Returns a tree of callees rooted at the given symbol, following call edges forward through the graph. Best for tracing from entry points (e.g. main, request handlers) to understand the full execution path.\n\nDo NOT use for reverse dependencies (\"what calls this?\") — use brain_impact instead. Do NOT use for general context around a symbol — use brain_context instead.\n\nThe `symbol` parameter accepts a symbol name (e.g. \"handleRequest\") or a full UID. The `max_depth` parameter caps tree depth (default 10). Cycles are detected and pruned. Use `response_format` \"concise\" for a function-name-only chain, or \"detailed\" (default) for full metadata including file paths and UIDs at each node.",
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

fn tool_flow_trace(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
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

    let opts = FlowTraceOpts { max_depth, concise };
    let tree = build_flow_tree(
        store,
        &root.uid,
        &root.name,
        &root.file_path,
        0,
        &mut visited,
        &opts,
    );

    Ok(json!({
        "root_uid": root.uid,
        "root_name": root.name,
        "max_depth": max_depth,
        "tree": tree,
    }))
}

/// Configuration for `build_flow_tree` to keep the argument count under
/// the clippy `too_many_arguments` threshold.
struct FlowTraceOpts {
    max_depth: usize,
    concise: bool,
}

fn build_flow_tree(
    store: &GraphStore,
    uid: &str,
    name: &str,
    file_path: &str,
    depth: usize,
    visited: &mut HashSet<String>,
    opts: &FlowTraceOpts,
) -> Value {
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
            );
            children.push(child);
        }
    }

    if opts.concise {
        json!({
            "name": name,
            "children": children,
        })
    } else {
        json!({
            "uid": uid,
            "name": name,
            "file_path": file_path,
            "depth": depth,
            "children": children,
        })
    }
}

// ── 11. detect_changes ─────────────────────────────────────────────────────

fn tool_schema_detect_changes() -> Value {
    json!({
        "name": "detect_changes",
        "description": "Use BEFORE committing or reviewing changes to understand their blast radius at the file level. Takes a list of changed file paths, maps them to all symbols defined in those files, traces their transitive dependents, and returns a risk assessment (low/medium/high) with affected execution flows.\n\nDo NOT use for single-symbol impact — use brain_impact instead. Do NOT use for cross-repo impact — use cross_repo_contracts. Do NOT use for git diff details — use brain_diff instead.\n\nThe `files` parameter accepts repo-relative paths (e.g. [\"src/auth/login.ts\", \"src/utils/validate.ts\"]). Returns affected symbols, affected processes/flows, and an overall risk level. Example: passing 3 changed files might return risk=\"high\" with 12 affected symbols across 2 execution flows.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of changed file paths (repo-relative). Example: [\"src/auth/login.ts\", \"src/utils/validate.ts\"]."
                }
            },
            "required": ["files"]
        }
    })
}

fn tool_detect_changes(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let files: Vec<String> = args
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("'files' must be an array of strings"))?
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
        "description": "Use to prioritize which test files an MR/PR should run for a set of code changes. Maps changed files to the symbols they define, reverse-traverses the call/import graph, and returns the test files that (transitively) depend on the changed code, bucketed into priority tiers: tier_1 = tests that directly reference a changed symbol, tier_2 = tests of a direct caller, tier_3 = transitively reachable tests.\n\nIMPORTANT — this is STATIC, call-graph-based regression test selection. It is a prioritized signal, NOT a provably-safe subset. It misses tests reached via reflection, dependency injection, codegen/macros, and data-driven or integration/e2e tests. \"No tests found\" does NOT mean it is safe to skip testing — keep a periodic full test run in CI. Treat the output as a ranked starting point, not a guarantee.\n\nDo NOT use for symbol-level blast radius — use brain_impact. Do NOT use for risk scoring of a change — use detect_changes.\n\nProvide either `changed_files` (repo-relative paths) or `base_ref` (a git ref such as \"main\"; runs `git diff --name-only base...HEAD` against the locally-indexed repo). Example: affected_tests(base_ref=\"main\") → {tier_1: [...], tier_2: [...], tier_3: [...], summary: \"3 tier-1, 2 tier-2, 0 tier-3 tests affected\"}.",
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

fn tool_affected_tests(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    // Resolve the set of changed files: explicit list takes precedence over base_ref.
    let mut changed_files: Vec<String> = args
        .get("changed_files")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if changed_files.is_empty()
        && let Some(base_ref) = args.get("base_ref").and_then(|v| v.as_str())
    {
        let repo_path = first_local_repo_path(store).unwrap_or_else(|| ".".to_string());
        let files =
            nestweaver_engine::changed_files_from_git(Path::new(&repo_path), Some(base_ref))
                .context("git diff for base_ref")?;
        changed_files = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
    }

    if changed_files.is_empty() {
        return Err(anyhow!(
            "provide either 'changed_files' (non-empty) or 'base_ref'"
        ));
    }

    let result = affected_tests(store, &changed_files).context("affected_tests")?;
    Ok(serde_json::to_value(&result)?)
}

/// Return the filesystem path of the first locally-indexed repo (file:// URL).
fn first_local_repo_path(store: &GraphStore) -> Option<String> {
    let repos = store.list_repos(None).unwrap_or_default();
    repos
        .iter()
        .find_map(|r| r.url.strip_prefix("file://").map(|p| p.to_string()))
}

// ── 12. clusters ───────────────────────────────────────────────────────────

fn tool_schema_clusters() -> Value {
    json!({
        "name": "clusters",
        "description": "Use to understand the high-level architecture of the codebase by viewing functional communities detected via the Leiden clustering algorithm. Each cluster groups tightly-connected symbols (functions, classes, modules) that form a cohesive unit, with a generated name, cohesion score, and key files.\n\nDo NOT use for specific symbol lookup — use brain_search or brain_context. Do NOT use for dependency analysis — use brain_impact or flow_trace. This is an exploratory tool for understanding overall code organization.\n\nThe optional `resolution` parameter controls cluster granularity: higher values produce more, smaller clusters; lower values produce fewer, larger clusters (default 1.0). Returns up to 20 member symbols per cluster. Example: resolution=0.5 might yield 3 broad architectural layers, while resolution=2.0 might yield 15 fine-grained feature modules.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "resolution": {
                    "type": "number",
                    "description": "Leiden resolution parameter. Higher = more, smaller clusters; lower = fewer, larger clusters. Default 0.5 (0.3 for large graphs >10K symbols). Try 2.0 for fine-grained modules.",
                    "default": 0.5
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

    let clusters_json: Vec<Value> = output
        .communities
        .iter()
        .map(|c| {
            let members: Vec<Value> = c
                .members
                .iter()
                .take(20)
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
        "description": "Use at the start of a session or after the user makes code changes to verify the graph index is current. Compares each repo's indexed git SHA against the current HEAD and reports whether the index is stale. No parameters required.\n\nDo NOT use to see what changed — use brain_diff for that. Do NOT use for vault/note freshness — the brain auto-detects note modifications on query.\n\nReturns per-repo staleness status with indexed SHA, current HEAD SHA, and a boolean `any_stale` flag. If stale, suggest the user re-index with brain_add_source or the CLI `nestweaver index` command to update the graph.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    })
}

fn tool_stale_check(store: &GraphStore) -> Result<Value, anyhow::Error> {
    let repos = store.list_repos(None).unwrap_or_default();

    let mut results = Vec::new();
    let mut any_stale = false;

    for repo in &repos {
        // Try to determine current HEAD from the repo's URL.
        let current_head = if let Some(path) = repo.url.strip_prefix("file://") {
            get_git_head(path)
        } else {
            get_remote_head(&repo.url)
        };

        let is_stale = match &current_head {
            Some(head) => head != &repo.indexed_sha,
            None => repo.staleness_commits_behind > 0,
        };

        if is_stale {
            any_stale = true;
        }

        results.push(json!({
            "url": repo.url,
            "indexed_sha": repo.indexed_sha,
            "current_head": current_head,
            "is_stale": is_stale,
            "staleness_commits_behind": repo.staleness_commits_behind,
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
        "description": "Use to attach custom metadata to any node (symbol, note, section, tag) in the brain. Stores key-value properties in a JSON sidecar file alongside the database. Use this for information not in the core schema, such as team ownership, deprecation status, review flags, or custom taxonomies.\n\nDo NOT use for querying existing properties — use query_extensions instead. Properties persist across sessions and are queryable immediately after being set.\n\nThe `uid` parameter is the node's full UID (e.g. \"sym:repo:...:hash:42\" for symbols, \"note:vlt:...:hash\" for notes). The `key` is a property name (e.g. \"team_owner\", \"deprecated\", \"priority\"). The `value` accepts any JSON value: strings, numbers, booleans, arrays, or objects. Example: set_extension(uid=\"sym:...\", key=\"team_owner\", value=\"platform-team\").",
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

    let db_path = CURRENT_DB_PATH
        .with(|c| c.borrow().clone())
        .ok_or_else(|| anyhow!("database path not set on server"))?;

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

// ── 15. query_extensions ───────────────────────────────────────────────────

fn tool_schema_query_extensions() -> Value {
    json!({
        "name": "query_extensions",
        "description": "Use to find nodes by custom metadata or to inspect all properties on a specific node. Queries the extension sidecar (set via set_extension) and returns matching UIDs with their full property maps.\n\nDo NOT use to set properties — use set_extension. Do NOT use for core graph queries (symbols, notes, edges) — use brain_search or brain_context.\n\nTwo modes: (1) Pass `uid` alone to get all custom properties for that specific node. (2) Pass `key` + `value` to find all nodes matching that property (e.g. key=\"team_owner\", value=\"platform-team\" returns every node owned by that team). Example: query_extensions(key=\"deprecated\", value=true) returns all nodes marked as deprecated.",
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
        "description": "Use before a code review or after pulling new changes to see what changed since the graph was last indexed. Returns files added, modified, and deleted between a base SHA and the current HEAD, plus all symbols defined in the changed files. Only works with locally-indexed repositories (file:// URLs).\n\nDo NOT use for impact analysis of hypothetical changes — use detect_changes instead. Do NOT use to check if the index is stale — use stale_check (faster, no git diff). Do NOT use for cross-repo change tracking.\n\nThe `repo` parameter is a repo name or substring of its URL (e.g. \"nestweaver\" or \"github.com/org/repo\"). The optional `since_sha` overrides the base SHA (defaults to the repo's last indexed SHA). Example: brain_diff(repo=\"my-app\") shows all files and symbols changed since the last index.",
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

    if !repo.url.starts_with("file://") {
        anyhow::bail!(
            "brain_diff only works with locally-indexed repositories (file:// URLs); \
             '{}' is not a local repo",
            repo.url
        );
    }
    let repo_path = repo.url.strip_prefix("file://").unwrap_or(&repo.url);

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
        "affected_symbol_count": affected_symbols.len(),
    }))
}

// ── 17. project_context ────────────────────────────────────────────────────

fn tool_schema_project_context() -> Value {
    json!({
        "name": "project_context",
        "description": "Use when you need the full context for a specific named project. Returns all Notes, Symbols, and Sections associated with the project, ranked by Personalized PageRank within the project's subgraph and bounded by a token budget. For composite projects, optionally includes content from component sub-projects.\n\nDo NOT use for ad-hoc topic queries — use brain_context with seed terms instead. Do NOT use if you don't know the project name — use brain_search to find it first. This tool requires projects to be defined in the graph (via vault taxonomy or instance config).\n\nThe `project` parameter accepts a project name (e.g. \"AuthService\"), alias, or UID. Use `kinds` to filter by node type (e.g. [\"Symbol\"] for code only, [\"Note\", \"Section\"] for docs only). Use `since` and `recency_weight` to prioritize recent content. Example: project_context(project=\"payments\", token_budget=5000, kinds=[\"Symbol\"]) returns the top code symbols in the payments project.",
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
) -> Result<Value, anyhow::Error> {
    let project_str = args
        .get("project")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("'project' must be a string"))?;
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(3000);
    let include_components = args
        .get("include_components")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
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
        let comp_notes = store.list_project_note_uids(comp_uid).unwrap_or_default();
        member_note_uids.extend(comp_notes.iter().cloned());
        member_uids.extend(comp_notes);
        let comp_syms = store.list_project_symbol_uids(comp_uid).unwrap_or_default();
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
    let mut ppr_seeds: Vec<String> = vec![project.uid.clone()];
    ppr_seeds.extend(component_uids);
    ppr_seeds.extend(member_note_uids.iter().cloned());

    let intent: nestweaver_store::QueryIntent = args
        .get("intent")
        .and_then(|v| v.as_str())
        .map(|s| s.parse::<nestweaver_store::QueryIntent>())
        .transpose()
        .map_err(|e| anyhow!("invalid intent: {e}"))?
        .unwrap_or(nestweaver_store::QueryIntent::ProjectContext);

    let db_path = current_db_path(store).unwrap_or_default();
    let aliases = load_alias_sidecar(&db_path);
    let config = HybridSearchConfig::default();
    let mut result = build_brain_context_hybrid_with_aliases(
        store,
        &ppr_seeds,
        tantivy,
        &config,
        &aliases,
        Some(&db_path),
        Some(intent),
    )?;

    // 4b. Surface the project's curated member notes into `connected`. They
    //     were seeded above, so they live in `result.seeds` — which is
    //     disjoint from `connected` and not rendered. For project orientation
    //     the curated notes are the answer, so promote them (Bug #12).
    nestweaver_engine::promote_member_notes_into_connected(&mut result, &member_note_uids);

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

    // 6. Apply token budget: account for seed cost, allocate remainder to connected.
    let seed_tokens: usize = result.seeds.iter().map(render_cost).sum();
    let remaining_budget = token_budget.saturating_sub(seed_tokens);
    let (cut, connected_tokens) = budgeted_cut(&result.connected, remaining_budget);
    let used_tokens = seed_tokens + connected_tokens;

    // 7. Load external_refs from extension sidecar.
    let ext_store = load_extensions(&db_path);
    let external_refs = get_all_properties(&ext_store, &project.uid)
        .get("external_refs")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let mut connected_json: Vec<Value> = result
        .connected
        .iter()
        .take(cut)
        .map(|n| {
            json!({
                "uid": n.uid,
                "kind": n.kind,
                "title": n.title,
                "location": n.location,
                "relevance": n.relevance,
            })
        })
        .collect();

    let include_seeds = args
        .get("include_seeds")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let seeds_json: Option<Vec<Value>> = if include_seeds {
        Some(
            result
                .seeds
                .iter()
                .map(|n| {
                    json!({
                        "uid": n.uid,
                        "kind": n.kind,
                        "title": n.title,
                        "location": n.location,
                        "relevance": n.relevance,
                    })
                })
                .collect(),
        )
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
        "description": "Use when you want to find cleanup opportunities or understand code coverage gaps. Walks forward from every entry point (main functions, HTTP handlers, event listeners, test runners) following CALLS, IMPORTS, EXTENDS, IMPLEMENTS, and MEMBER_OF edges. Symbols not reached are flagged as potentially dead, with confidence scoring based on visibility: High (private/internal — very likely dead), Medium (inferred visibility), Low (public — may be a library API consumed externally).\n\nDo NOT use for understanding what depends on a specific symbol — use brain_impact instead. Do NOT use for finding hub nodes or architectural chokepoints — use hub_nodes or bridge_nodes instead.\n\nThe `min_confidence` parameter filters results (default 'low' = show all). Use `response_format` \"concise\" to get only names and confidence levels (good for quick scan), or \"detailed\" (default) for full metadata including UIDs, file paths, kinds, and visibility.",
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
                }
            }
        }
    })
}

fn tool_dead_code(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
    let min_conf_str = args
        .get("min_confidence")
        .and_then(|v| v.as_str())
        .unwrap_or("low");
    let min_conf =
        DeadCodeConfidence::from_str_loose(min_conf_str).unwrap_or(DeadCodeConfidence::Low);
    let concise = is_concise(&args);

    let result = detect_dead_code(store).context("detect_dead_code")?;

    let filtered: Vec<Value> = result
        .unreachable_symbols
        .iter()
        .filter(|s| s.confidence >= min_conf)
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
        "unreachable_count": filtered.len(),
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
        "description": "Use when you need to identify the most connected symbols in the codebase — the central abstractions that many other parts depend on. Returns nodes ranked by total degree (incoming + outgoing edges), with optional cluster membership. Hub nodes are the architectural core: changing them affects the most code paths.\n\nDo NOT use for finding chokepoints between communities — use bridge_nodes instead. Do NOT use for understanding a specific symbol's dependencies — use brain_impact or flow_trace instead.\n\nThe `top_n` parameter controls how many hubs are returned (default 10). Use `response_format` \"concise\" to get only names and degree counts (good for quick orientation), or \"detailed\" (default) for full metadata including UIDs, file paths, PageRank scores, and cluster IDs.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "top_n": {
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
        .get("top_n")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);
    let concise = is_concise(&args);

    let mut hubs = find_hub_nodes(store, top_n).context("find_hub_nodes")?;

    // Attach cluster IDs if clustering sidecar exists.
    let db_path = current_db_path(store).unwrap_or_default();
    if let Ok(Some(clustering)) = load_clusters(&db_path) {
        attach_cluster_ids(&mut hubs, &clustering);
    }

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

    Ok(json!({
        "top_n": top_n,
        "count": nodes_json.len(),
        "hubs": nodes_json,
    }))
}

// ── 20. bridge_nodes ──────────────────────────────────────────────────────

fn tool_schema_bridge_nodes() -> Value {
    json!({
        "name": "bridge_nodes",
        "description": "Use when you need to find architectural chokepoints — symbols that sit on many shortest paths between other nodes and have outsized blast radius if changed. Returns nodes ranked by betweenness centrality (Brandes' algorithm with sampling), plus which community clusters each bridge connects.\n\nDo NOT use for finding the most-connected nodes — use hub_nodes instead (degree centrality). Do NOT use for single-symbol impact analysis — use brain_impact instead.\n\nThe `top_n` parameter controls how many bridges are returned (default 10). Use `response_format` \"concise\" to get only names and betweenness scores (good for quick triage), or \"detailed\" (default) for full metadata including UIDs, file paths, and connected community IDs.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "top_n": {
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
        .get("top_n")
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
        "description": "Use BEFORE merging a PR or after staging changes to understand the full blast radius across the code graph. Takes a list of changed file paths, maps them to all symbols defined in those files, runs transitive reverse-dependency analysis up to a configurable depth, groups affected symbols by cluster/community, and returns a risk assessment (Low/Medium/High) based on affected symbol count, PageRank centrality of changed symbols, and number of clusters touched.\n\nDo NOT use for single-symbol impact — use brain_impact instead. Do NOT use for cross-repo impact — use cross_repo_contracts. This tool gives a holistic PR-level view including cluster grouping and risk scoring.\n\nThe `changed_files` parameter accepts repo-relative paths (e.g. [\"src/auth/login.ts\", \"src/utils/validate.ts\"]). The optional `max_depth` parameter controls transitive traversal depth (default 3). Returns changed symbols (with PageRank scores), affected symbols (with depth and edge type), affected clusters, risk level, and a human-readable summary.",
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
                }
            },
            "required": ["changed_files"]
        }
    })
}

fn tool_blast_radius(store: &GraphStore, args: Value) -> Result<Value, anyhow::Error> {
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

    let db_path = current_db_path(store).ok();
    let result = analyze_blast_radius(store, &files, max_depth, db_path.as_deref())
        .context("analyze_blast_radius")?;

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

    Ok(json!({
        "changed_files": files.iter().map(|f| f.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "max_depth": max_depth,
        "risk": risk_str,
        "summary": result.summary,
        "changed_symbols": changed_json,
        "changed_symbol_count": changed_json.len(),
        "affected_symbols": affected_json,
        "affected_symbol_count": affected_json.len(),
        "affected_clusters": clusters_json,
        "affected_cluster_count": clusters_json.len(),
    }))
}

// ── 22. get_summary ──────────────────────────────────────────────────────

fn tool_schema_get_summary() -> Value {
    json!({
        "name": "get_summary",
        "description": "Use when you need a compact architectural overview without reading raw code files. Returns hierarchical, deterministic summaries at three granularity levels: symbol (function/class with callers/callees and file location), file (exports and import sources per file), or cluster (community architecture with key types and cross-cluster dependencies). No LLM needed — summaries are derived entirely from graph data and are highly token-efficient.\n\nDo NOT use for specific symbol lookup — use brain_search or brain_context instead. Do NOT use for understanding a single symbol's call chain — use flow_trace or brain_impact instead.\n\nThe `level` parameter selects granularity: 'symbol' for per-function detail, 'file' for per-file exports, 'cluster' for community-level architecture. Use `target` to filter to a specific file path, symbol name, or cluster name. Use `token_budget` to cap output size for context windows.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "level": {
                    "type": "string",
                    "enum": ["symbol", "file", "cluster"],
                    "description": "Summary granularity. 'symbol' = per-function/class, 'file' = per-file exports, 'cluster' = per-community architecture.",
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
    use nestweaver_engine::{load_summaries, save_summaries};

    let level_str = args.get("level").and_then(|v| v.as_str()).unwrap_or("file");
    let level: SummaryLevel = level_str.parse().map_err(|e: String| anyhow!("{e}"))?;
    let target = args.get("target").and_then(|v| v.as_str());
    let token_budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    // Try loading cached summaries from the sidecar first; only use the
    // cache when it contains entries at the requested level.
    let db_path = match current_db_path(store) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("get_summary: db_path unavailable ({e}), sidecar cache disabled");
            None
        }
    };
    let (summaries, from_cache) = if let Some(ref db) = db_path
        && let Ok(Some(cached)) = load_summaries(db)
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

    // Persist freshly generated summaries so subsequent calls hit the cache.
    if !from_cache && let Some(ref db) = db_path {
        // Merge with any existing cached summaries at other levels.
        let mut all = if let Ok(Some(existing)) = load_summaries(db) {
            existing
                .into_iter()
                .filter(|s| s.level != level)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        all.extend(summaries.iter().cloned());
        // Best-effort save; don't fail the tool call on I/O error.
        if let Err(e) = save_summaries(db, &all) {
            tracing::warn!("failed to save summaries sidecar: {e}");
        }
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
    static ALLOW_ADD_SOURCES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static LITE_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ALLOWED_TOOLS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
    static TRACK_INTERACTIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // F16 response cache: size cap (MiB) and per-session hit/miss counters.
    static CACHE_MAX_SIZE_MB: std::cell::Cell<u64> =
        const { std::cell::Cell::new(nestweaver_store::cache::DEFAULT_MAX_SIZE_MB) };
    static CACHE_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CACHE_MISSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub fn set_current_db_path(path: std::path::PathBuf) {
    CURRENT_DB_PATH.with(|c| *c.borrow_mut() = Some(path));
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

// ── Daemon proxy dispatch ─────────────────────────────────────────────────

/// The tonic-generated gRPC client type for the `NestWeaverDaemon` service.
/// Re-exported so callers (e.g. `main.rs`) can construct it and pass it in
/// without depending on `nestweaver-proto` directly.
#[cfg(feature = "daemon")]
pub type DaemonGrpcClient =
    nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient<tonic::transport::Channel>;

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

    let args_json = serde_json::to_string(&args)?;

    // brain_add_source is special: it maps to IndexRepo or IndexVault
    // (streaming RPCs) depending on the path content.
    if name == "brain_add_source" {
        return dispatch_add_source_via_daemon(client, rt, args);
    }

    let result_json = rt.block_on(async {
        let req = tonic::Request::new(JsonRequest {
            args_json: args_json.clone(),
        });

        let resp = match name {
            "brain_search" => client.search(req).await,
            "brain_context" => client.get_context(req).await,
            "project_context" => client.get_project_context(req).await,
            "note_get" => client.get_note(req).await,
            "backlinks" => client.get_backlinks(req).await,
            "brain_status" => client.brain_status(req).await,
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
            "hub_nodes" => client.hub_nodes(req).await,
            "bridge_nodes" => client.bridge_nodes(req).await,
            "get_summary" => client.get_summary(req).await,
            "investigate" => client.investigate(req).await,
            "investigate_expand" => client.investigate_expand(req).await,
            "investigate_hydrate" => client.investigate_hydrate(req).await,
            "set_extension" => client.set_extension(req).await,
            "query_extensions" => client.query_extensions(req).await,
            other => {
                return Err(anyhow::anyhow!("unknown tool for daemon dispatch: {other}"));
            }
        };

        let resp = resp.map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?;
        Ok(resp.into_inner().result_json)
    })?;

    serde_json::from_str(&result_json).map_err(Into::into)
}

/// Handle `brain_add_source` by routing to `IndexRepo` or `IndexVault`
/// streaming RPCs. Detection order matches the non-daemon path:
/// 1. `.git/` present → code repo (IndexRepo)
/// 2. `.obsidian/` present OR contains `.md` files → vault/markdown (IndexVault)
#[cfg(feature = "daemon")]
fn dispatch_add_source_via_daemon(
    client: &mut DaemonGrpcClient,
    rt: &tokio::runtime::Runtime,
    args: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    use tokio_stream::StreamExt;

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
    let is_repo = resolved.join(".git").exists();
    let is_vault = !is_repo
        && (resolved.join(".obsidian").exists()
            || std::fs::read_dir(resolved)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .any(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                })
                .unwrap_or(false));

    rt.block_on(async {
        if is_vault || !is_repo {
            let req = tonic::Request::new(nestweaver_proto::IndexVaultRequest {
                vault_path: path.clone(),
                vault_name: name,
                extra_ignore_patterns: vec![],
            });
            let mut stream = client.index_vault(req).await
                .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?
                .into_inner();

            let mut last_msg = String::new();
            while let Some(progress) = stream.next().await {
                let progress = progress.map_err(|s| anyhow::anyhow!("stream error: {}", s.message()))?;
                last_msg = progress.message;
            }
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
            });
            let mut stream = client.index_repo(req).await
                .map_err(|s| anyhow::anyhow!("gRPC error: {}", s.message()))?
                .into_inner();

            let mut last_msg = String::new();
            while let Some(progress) = stream.next().await {
                let progress = progress.map_err(|s| anyhow::anyhow!("stream error: {}", s.message()))?;
                last_msg = progress.message;
            }
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
        "description": "Use to orient yourself on an unfamiliar topic, feature, or subsystem in ONE call instead of a chain of searches. Runs hybrid PPR + BM25 retrieval (with pseudo-relevance feedback) for your query, groups the results into architectural domains (code directories + notes), inlines a few high-confidence source bodies, and returns a token-budgeted map plus a `bundle_id`. Drill into specific entries afterwards with `investigate_expand` (by asset_id) or fill in all remaining bodies with `investigate_hydrate`.\n\nScope: \"project:<slug>\" restricts seeds to a project's members, \"repo:<name>\" restricts results to a repo, \"vault\"/\"all\"/omitted = no restriction. Returns `{bundle_id, domains:[{label, entry_point, members}], entries:[{asset_id, uid, kind, title, location, summary, inline_body?, relevance}], more_available}`. `more_available` counts entries dropped by the token budget — raise `token_budget` to see them.",
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
    )?;
    Ok(serde_json::to_value(result)?)
}

fn tool_schema_investigate_expand() -> Value {
    json!({
        "name": "investigate_expand",
        "description": "Use after `investigate` to drill into specific map entries. Given a `bundle_id` and one or more targets (each an `asset_id` from the map, or a raw node uid), returns each entry's full source body plus its immediate neighbors (callers/callees for symbols, wikilink sources for notes) and marks the entries expanded. Returns `{bundle_id, expanded:[entry], neighbors:[{of, uid, kind, title, relation}], unresolved:[target]}`. Bundles expire 24h after creation.",
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
        "description": "Use after `investigate` to fill in source bodies/summaries for every map entry that doesn't yet have one — the bulk version of `investigate_expand`, budget-bounded. Given a `bundle_id`, reads bodies for all un-hydrated entries up to the token budget. Returns `{bundle_id, hydrated, entries}`. Bundles expire 24h after creation.",
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
            json!({ "project": "Parallel Paths", "token_budget": 5000 }),
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
        let first = dispatch(&store, None, "hub_nodes", args.clone()).unwrap();
        let second = dispatch(&store, None, "hub_nodes", args.clone()).unwrap();

        assert_eq!(
            first, second,
            "2nd call must return byte-identical response"
        );
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
        let _ = dispatch(&store, None, "hub_nodes", args.clone()).unwrap();
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
        let _ = dispatch(&store2, None, "hub_nodes", args).unwrap();
        // The old entry's generation no longer matches → MISS (recomputed).
        assert_eq!(CACHE_MISSES.with(|c| c.get()), 1, "stale entry must miss");
        assert_eq!(CACHE_HITS.with(|c| c.get()), 0);
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
        let _ = dispatch(&store, None, "hub_nodes", json!({ "limit": 5 })).unwrap();
        reset_session();
        // cache:"bypass" skips the cache entirely (no hit recorded).
        let _ = dispatch(
            &store,
            None,
            "hub_nodes",
            json!({ "limit": 5, "cache": "bypass" }),
        )
        .unwrap();
        // no_cache:true likewise.
        let _ = dispatch(
            &store,
            None,
            "hub_nodes",
            json!({ "limit": 5, "no_cache": true }),
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
        // And a representative read tool IS cacheable.
        assert!(is_cacheable_tool("hub_nodes"));

        let store = GraphStore::open(&db_path).unwrap();
        // set_extension is a write tool; dispatching it must not create a cache.
        let _ = dispatch(
            &store,
            None,
            "set_extension",
            json!({ "uid": "sym:x", "key": "k", "value": "v" }),
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
}

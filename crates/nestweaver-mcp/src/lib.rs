//! MCP server for the NestWeaver brain.
//!
//! Implements the three methods Claude Code's MCP client uses against a
//! stdio server (`initialize`, `tools/list`, `tools/call`) plus the
//! `notifications/initialized` notification. Wire format is line-delimited
//! JSON-RPC 2.0. `tracing` is configured by the caller to write to stderr
//! (CRITICAL — anything on stdout must be a valid MCP frame).

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::Context;
use nestweaver_store::{GraphStore, TantivyIndex};
use serde_json::{Value, json};

pub mod http;
pub mod protocol;
pub mod tools;

use protocol::{ErrorResponse, PROTOCOL_VERSION, Response, error, error_code, success};

const SERVER_NAME: &str = "nestweaver-brain";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

const SERVER_INSTRUCTIONS: &str = "\
NestWeaver is a code intelligence knowledge graph. Use it instead of grep/find for indexed repos.

## Quick Reference
- Explore a topic: brain_context (seed with symbol/file name, filter by repo/tags)
- Find a symbol: brain_search (keyword search across code + notes)
- Check impact before changing code: brain_impact or blast_radius
- Trace execution flow: flow_trace (forward call graph)
- Read a symbol's source: read_symbols (cheaper than reading whole files)
- Read a vault note: note_get (by UID or title)
- Investigate unfamiliar code: investigate → investigate_expand → investigate_hydrate

## Query Efficiency
- Always query the graph before reading source files
- Filter with repos, tags, path_prefix, kinds for precision
- Use response_format 'concise' unless you need full bodies
- In subagents/scripts: use CLI (nestweaver search --json) instead of MCP for fewer tokens

## Do NOT
- grep or find in indexed repos — use brain_search or regex_search
- Read entire files — use read_symbols for specific symbol spans
- Re-index manually — use stale_check to verify, the daemon handles re-indexing";

/// Canonical sidecar location for the Tantivy index. Lives next to the
/// LadybugDB file: `<db>.tantivy/`.
pub fn tantivy_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    nestweaver_engine::sidecar_path(db_path, ".tantivy")
}

/// Run the brain server on stdio until the client closes stdin or sends
/// no more lines. Returns Ok on clean shutdown; errors only on truly
/// unrecoverable conditions (the database failing to open, etc.). Per-call
/// errors are wrapped as MCP tool errors and the loop continues.
pub fn run_stdio_server(
    db_path: &Path,
    allow_add_sources: bool,
    lite: bool,
    track_interactions: bool,
    config_path: Option<&Path>,
) -> Result<(), anyhow::Error> {
    let store = GraphStore::open_or_readonly(db_path)
        .with_context(|| format!("open GraphStore at {}", db_path.display()))?;
    // Pre-load the PageRank sidecar if present — same behaviour as the CLI.
    nestweaver_engine::migrate_sidecar(db_path, "pagerank.json", ".pagerank.json");
    let pr_path = nestweaver_engine::sidecar_path(db_path, ".pagerank.json");
    let _ = store.load_pagerank_cache(&pr_path);

    // Load interaction memory scores so PPR can apply a small bias toward
    // frequently-accessed nodes.
    if let Some(scores) = nestweaver_engine::load_interaction_scores(db_path) {
        store.load_interaction_cache(scores);
    }

    // Open the Tantivy index sidecar in read-only mode. The MCP server
    // only searches — it never writes to the index. Reader-only mode
    // avoids contending for the writer lock with a running brain watcher.
    let tantivy_path = tantivy_sidecar_path(db_path);
    let tantivy = match TantivyIndex::open_reader_only(&tantivy_path) {
        Ok(idx) => {
            tracing::info!(
                docs = idx.doc_count(),
                path = %tantivy_path.display(),
                "Tantivy index open (reader-only)"
            );
            Some(idx)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not open Tantivy index — brain_search will use substring fallback"
            );
            None
        }
    };

    // Make the DB path available to tool handlers that need to spawn
    // additional indexer invocations against the same DB.  Canonicalize
    // so that sidecar lookups (extensions, summaries, clusters) always
    // resolve to the correct location regardless of CWD changes.
    let canonical_db = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
    tools::set_current_db_path(canonical_db);
    tools::set_allow_add_sources(allow_add_sources);
    tools::set_lite_mode(lite);

    // Load instance config so tools can read [limits], [response], [ranking], etc.
    // Try explicit --config path first, then auto-discover instance.toml next to the DB.
    let (instance_cfg, cfg_source) = if let Some(p) = config_path {
        match nestweaver_engine::InstanceConfig::from_file(p) {
            Ok(c) => (Some(c), Some(p.display().to_string())),
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "failed to load --config");
                (None, None)
            }
        }
    } else {
        let sibling = db_path.parent().map(|d| d.join("instance.toml"));
        match sibling.as_deref().and_then(|s| {
            nestweaver_engine::InstanceConfig::from_file(s)
                .ok()
                .map(|c| (c, s.display().to_string()))
        }) {
            Some((c, path)) => (Some(c), Some(path)),
            None => (None, None),
        }
    };
    if let Some(cfg) = instance_cfg {
        tracing::info!(
            config = cfg_source.as_deref().unwrap_or("?"),
            limits.default_result_limit = cfg.limits.default_result_limit,
            "loaded instance config"
        );
        if cfg.cache.max_size_mb > 0 {
            tools::set_cache_max_size_mb(cfg.cache.max_size_mb);
        }
        tools::set_current_instance_config(Some(std::sync::Arc::new(cfg)));
    }

    let tracker: Option<nestweaver_engine::InteractionTracker> = if track_interactions {
        Some(nestweaver_engine::InteractionTracker::new(db_path))
    } else {
        None
    };

    tracing::info!(
        path = %db_path.display(),
        track_interactions,
        "brain MCP server ready on stdio"
    );

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut line = String::new();
    let mut reader = stdin.lock();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF from the client — clean shutdown.
            if let Some(tracker) = &tracker {
                // Best-effort terminal-success heuristic: if the agent's last
                // tool call was NOT another search (i.e. it stopped looking),
                // the context it had at that point was "good enough". Record a
                // TerminalSuccess for the UIDs most recently surfaced this
                // session so that positive outcome reinforces those nodes.
                // This is purely heuristic and must never break shutdown.
                maybe_record_terminal_success(tracker);
                if let Err(e) = tracker.flush() {
                    tracing::warn!("failed to flush interaction tracker: {e}");
                }
            }
            // Flush any in-process response cache entries that haven't been
            // written to disk yet (periodic flush threshold may not have been hit).
            crate::tools::flush_response_cache();
            tracing::info!("client closed stdin; shutting down");
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse as serde_json::Value first to detect batch (array) vs
        // single (object) requests per JSON-RPC 2.0 §6.
        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = error(
                    Value::Null,
                    error_code::PARSE_ERROR,
                    format!("invalid JSON: {e}"),
                );
                write_response(&mut stdout, &Frame::Error(resp))?;
                continue;
            }
        };

        if let Value::Array(arr) = parsed {
            // Batch request.
            if arr.is_empty() {
                let resp = error(
                    Value::Null,
                    error_code::INVALID_REQUEST,
                    "empty batch array",
                );
                write_response(&mut stdout, &Frame::Error(resp))?;
                continue;
            }
            let mut responses: Vec<Value> = Vec::new();
            for item in arr {
                let req: protocol::Request = match serde_json::from_value(item) {
                    Ok(r) => r,
                    Err(e) => {
                        responses.push(serde_json::to_value(error(
                            Value::Null,
                            error_code::INVALID_REQUEST,
                            format!("invalid request in batch: {e}"),
                        ))?);
                        continue;
                    }
                };
                let is_notification = req.id.is_none();
                let outcome = dispatch_method(&store, tantivy.as_ref(), &req, tracker.as_ref());
                if is_notification {
                    if let Frame::Error(e) = outcome {
                        tracing::warn!(
                            "batch notification {} produced error: {}",
                            req.method,
                            e.error.message
                        );
                    }
                    continue;
                }
                let val = match outcome {
                    Frame::Success(r) => serde_json::to_value(r)?,
                    Frame::Error(e) => serde_json::to_value(e)?,
                };
                responses.push(val);
            }
            if !responses.is_empty() {
                let serialized = serde_json::to_string(&responses)?;
                stdout.write_all(serialized.as_bytes())?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        } else {
            // Single request.
            let req: protocol::Request = match serde_json::from_value(parsed) {
                Ok(r) => r,
                Err(e) => {
                    let resp = error(
                        Value::Null,
                        error_code::INVALID_REQUEST,
                        format!("invalid request: {e}"),
                    );
                    write_response(&mut stdout, &Frame::Error(resp))?;
                    continue;
                }
            };
            let is_notification = req.id.is_none();
            let outcome = dispatch_method(&store, tantivy.as_ref(), &req, tracker.as_ref());
            if is_notification {
                if let Frame::Error(e) = outcome {
                    tracing::warn!(
                        "notification {} produced error: {}",
                        req.method,
                        e.error.message
                    );
                }
                continue;
            }
            write_response(&mut stdout, &outcome)?;
        }
    }
}

/// Run the brain server in daemon proxy mode: instead of opening the DB
/// directly, all tool calls are forwarded to the daemon via gRPC. The
/// `tools/list`, `initialize`, and `ping` methods are handled locally.
///
/// The caller must provide a connected `DaemonGrpcClient` (the inner tonic
/// client from `nestweaver_client::DaemonClient::inner_mut()`) and a tokio
/// `Runtime`. This avoids a dependency cycle: the MCP crate does not depend
/// on `nestweaver-client` (which depends on `nestweaver-daemon` which
/// depends on this crate).
#[cfg(feature = "daemon")]
pub fn run_stdio_server_daemon(
    mut grpc_client: tools::DaemonGrpcClient,
    rt: tokio::runtime::Runtime,
    lite: bool,
    track_interactions: bool,
    db_path: &std::path::Path,
) -> Result<(), anyhow::Error> {
    tools::set_lite_mode(lite);

    let tracker: Option<nestweaver_engine::InteractionTracker> = if track_interactions {
        Some(nestweaver_engine::InteractionTracker::new(db_path))
    } else {
        None
    };

    tracing::info!(
        track_interactions,
        "brain MCP server ready on stdio (daemon proxy mode)"
    );

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut line = String::new();
    let mut reader = stdin.lock();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            if let Some(tracker) = &tracker {
                maybe_record_terminal_success(tracker);
                if let Err(e) = tracker.flush() {
                    tracing::warn!("failed to flush interaction tracker: {e}");
                }
            }
            tracing::info!("client closed stdin; shutting down (daemon proxy)");
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = error(
                    Value::Null,
                    error_code::PARSE_ERROR,
                    format!("invalid JSON: {e}"),
                );
                write_response(&mut stdout, &Frame::Error(resp))?;
                continue;
            }
        };

        if let Value::Array(arr) = parsed {
            if arr.is_empty() {
                let resp = error(
                    Value::Null,
                    error_code::INVALID_REQUEST,
                    "empty batch array",
                );
                write_response(&mut stdout, &Frame::Error(resp))?;
                continue;
            }
            let mut responses: Vec<Value> = Vec::new();
            for item in arr {
                let req: protocol::Request = match serde_json::from_value(item) {
                    Ok(r) => r,
                    Err(e) => {
                        responses.push(serde_json::to_value(error(
                            Value::Null,
                            error_code::INVALID_REQUEST,
                            format!("invalid request in batch: {e}"),
                        ))?);
                        continue;
                    }
                };
                let is_notification = req.id.is_none();
                let outcome = dispatch_method_daemon(&mut grpc_client, &rt, &req, tracker.as_ref());
                if is_notification {
                    if let Frame::Error(e) = outcome {
                        tracing::warn!(
                            "batch notification {} produced error: {}",
                            req.method,
                            e.error.message
                        );
                    }
                    continue;
                }
                let val = match outcome {
                    Frame::Success(r) => serde_json::to_value(r)?,
                    Frame::Error(e) => serde_json::to_value(e)?,
                };
                responses.push(val);
            }
            if !responses.is_empty() {
                let serialized = serde_json::to_string(&responses)?;
                stdout.write_all(serialized.as_bytes())?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        } else {
            let req: protocol::Request = match serde_json::from_value(parsed) {
                Ok(r) => r,
                Err(e) => {
                    let resp = error(
                        Value::Null,
                        error_code::INVALID_REQUEST,
                        format!("invalid request: {e}"),
                    );
                    write_response(&mut stdout, &Frame::Error(resp))?;
                    continue;
                }
            };
            let is_notification = req.id.is_none();
            let outcome = dispatch_method_daemon(&mut grpc_client, &rt, &req, tracker.as_ref());
            if is_notification {
                if let Frame::Error(e) = outcome {
                    tracing::warn!(
                        "notification {} produced error: {}",
                        req.method,
                        e.error.message
                    );
                }
                continue;
            }
            write_response(&mut stdout, &outcome)?;
        }
    }
}

#[cfg(feature = "daemon")]
fn dispatch_method_daemon(
    client: &mut tools::DaemonGrpcClient,
    rt: &tokio::runtime::Runtime,
    req: &protocol::Request,
    tracker: Option<&nestweaver_engine::InteractionTracker>,
) -> Frame {
    let id = req.id.clone().unwrap_or(Value::Null);

    match req.method.as_str() {
        "initialize" => Frame::Success(success(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": SERVER_VERSION,
                },
                "instructions": SERVER_INSTRUCTIONS
            }),
        )),

        "notifications/initialized" | "initialized" => Frame::Success(success(id, Value::Null)),

        "tools/list" => {
            let lite = tools::is_lite_mode();
            Frame::Success(success(id, tools::tool_list(lite)))
        }

        "tools/call" => {
            let params = req.params.clone().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(Value::Object(serde_json::Map::new()));

            let Some(name) = name else {
                return Frame::Error(error(
                    id,
                    error_code::INVALID_PARAMS,
                    "tools/call: 'name' is required",
                ));
            };

            match tools::dispatch_via_daemon(client, rt, &name, arguments.clone()) {
                Ok(result) => {
                    if let Some(tracker) = tracker {
                        record_interaction(tracker, &name, &arguments, &result);
                    }
                    Frame::Success(success(id, tools::wrap_tool_result(result)))
                }
                Err(e) => Frame::Success(success(id, tools::wrap_tool_error(&e.to_string()))),
            }
        }

        "ping" => Frame::Success(success(id, json!({}))),

        other => Frame::Error(error(
            id,
            error_code::METHOD_NOT_FOUND,
            format!("method not implemented: {other}"),
        )),
    }
}

enum Frame {
    Success(Response),
    Error(ErrorResponse),
}

fn write_response(out: &mut std::io::StdoutLock<'_>, frame: &Frame) -> Result<(), anyhow::Error> {
    let serialized = match frame {
        Frame::Success(r) => serde_json::to_string(r)?,
        Frame::Error(e) => serde_json::to_string(e)?,
    };
    out.write_all(serialized.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn dispatch_method(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    req: &protocol::Request,
    tracker: Option<&nestweaver_engine::InteractionTracker>,
) -> Frame {
    let id = req.id.clone().unwrap_or(Value::Null);

    match req.method.as_str() {
        "initialize" => Frame::Success(success(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": SERVER_VERSION,
                },
                "instructions": SERVER_INSTRUCTIONS
            }),
        )),

        "notifications/initialized" | "initialized" => {
            // Notification — never reaches a sent frame, but we still need
            // to return SOMETHING. Caller filters notifications above.
            Frame::Success(success(id, Value::Null))
        }

        "tools/list" => {
            let lite = tools::is_lite_mode();
            Frame::Success(success(id, tools::tool_list(lite)))
        }

        "tools/call" => {
            let params = req.params.clone().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(Value::Object(serde_json::Map::new()));

            let Some(name) = name else {
                return Frame::Error(error(
                    id,
                    error_code::INVALID_PARAMS,
                    "tools/call: 'name' is required",
                ));
            };

            match tools::dispatch(store, tantivy, &name, arguments.clone(), None) {
                Ok(result) => {
                    if let Some(tracker) = tracker {
                        record_interaction(tracker, &name, &arguments, &result);
                    }
                    Frame::Success(success(id, tools::wrap_tool_result(result)))
                }
                Err(e) => {
                    // Tool errors come back inside the result envelope with
                    // isError=true — not as JSON-RPC errors — so the client
                    // can surface them to Claude rather than aborting the
                    // call sequence.
                    Frame::Success(success(id, tools::wrap_tool_error(&e.to_string())))
                }
            }
        }

        "ping" => Frame::Success(success(id, json!({}))),

        // Methods we don't implement (e.g. resources/list, prompts/list)
        // return method-not-found per JSON-RPC convention.
        other => Frame::Error(error(
            id,
            error_code::METHOD_NOT_FOUND,
            format!("method not implemented: {other}"),
        )),
    }
}

// ── interaction telemetry helpers ──────────────────────────────────────────

/// Tool names that count as "the agent is still searching". If the last
/// recorded tool call was one of these, we do NOT record a terminal-success
/// signal at shutdown — the agent was still looking when the session ended.
const SEARCH_TOOLS: &[&str] = &["brain_search", "brain_context", "project_context"];

/// Best-effort, shutdown-time heuristic: record a [`TerminalSuccess`] event
/// for the UIDs most recently surfaced this session, *unless* the agent's
/// last tool call was itself a search (which means it had not yet found what
/// it needed). Called at clean stdin-EOF shutdown.
///
/// This is intentionally simple and conservative:
/// - No last tool recorded → nothing happened, skip.
/// - Last tool was a search → agent was still looking, skip.
/// - Otherwise → reinforce the last surfaced UIDs (the retrieval the agent
///   acted on and then stopped searching).
///
/// `record_terminal_success` no-ops on an empty UID list, so this is safe
/// even when nothing was ever surfaced.
///
/// [`TerminalSuccess`]: nestweaver_engine::EventType::TerminalSuccess
fn maybe_record_terminal_success(tracker: &nestweaver_engine::InteractionTracker) {
    let last_tool = match tracker.last_tool_name() {
        Some(t) => t,
        None => return, // nothing happened this session
    };
    if SEARCH_TOOLS.contains(&last_tool.as_str()) {
        // Agent's last action was a search — it was still looking, so we do
        // not treat the session as a successful terminal retrieval.
        return;
    }
    let surfaced = tracker.last_surfaced_uids();
    tracker.record_terminal_success(&surfaced);
}

/// Classify the tool call and record an appropriate interaction event.
fn record_interaction(
    tracker: &nestweaver_engine::InteractionTracker,
    name: &str,
    arguments: &Value,
    result: &Value,
) {
    match name {
        "brain_context"
        | "brain_search"
        | "project_context"
        | "investigate"
        | "investigate_expand"
        | "investigate_hydrate" => {
            let seeds = extract_string_array(arguments, "seeds");
            let results = extract_result_uids(result);
            tracker.record_query(name, &seeds, &results);
        }
        "note_get" | "backlinks" | "get_summary" | "read_symbols" | "hub_nodes"
        | "bridge_nodes" | "clusters" => {
            if let Some(uid) = arguments.get("uid").and_then(|v| v.as_str()) {
                tracker.record_access(name, uid);
            }
        }
        "brain_impact" | "blast_radius" | "affected_tests" | "dead_code" | "flow_trace" => {
            let seeds = extract_string_array(arguments, "seeds");
            tracker.record_impact(name, &seeds);
        }
        _ => {}
    }
}

/// Extract a string array from a JSON object by key.
fn extract_string_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Try to extract UIDs from a tool result value.
///
/// The result is the raw structured JSON returned by `dispatch()` (before
/// `wrap_tool_result` wraps it). Different tools use different field names:
/// - `brain_context` / `project_context` → `connected[].uid`
/// - `brain_search` → `results[].uid`
fn extract_result_uids(result: &Value) -> Vec<String> {
    // brain_context / project_context: connected[].uid
    if let Some(connected) = result.get("connected").and_then(|c| c.as_array()) {
        let uids: Vec<String> = connected
            .iter()
            .filter_map(|n| n.get("uid").and_then(|u| u.as_str()).map(String::from))
            .collect();
        if !uids.is_empty() {
            return uids;
        }
    }
    // brain_search: results[].uid
    if let Some(results) = result.get("results").and_then(|r| r.as_array()) {
        return results
            .iter()
            .filter_map(|n| n.get("uid").and_then(|u| u.as_str()).map(String::from))
            .collect();
    }
    vec![]
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(method: &str, id: u64, params: Value) -> protocol::Request {
        protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(id)),
            method: method.to_string(),
            params: Some(params),
        }
    }

    #[test]
    fn initialize_returns_capabilities_and_server_info() {
        let store = GraphStore::in_memory().unwrap();
        let req = make_request("initialize", 1, json!({}));
        let frame = dispatch_method(&store, None, &req, None);
        match frame {
            Frame::Success(resp) => {
                assert_eq!(resp.id, json!(1));
                assert_eq!(resp.result["protocolVersion"], PROTOCOL_VERSION);
                assert!(resp.result["capabilities"]["tools"].is_object());
                assert_eq!(resp.result["serverInfo"]["name"], SERVER_NAME);
                assert!(resp.result["serverInfo"]["version"].is_string());
            }
            Frame::Error(e) => panic!("initialize should succeed: {}", e.error.message),
        }
    }

    #[test]
    fn tools_list_returns_all_tools() {
        let store = GraphStore::in_memory().unwrap();
        let req = make_request("tools/list", 2, json!({}));
        let frame = dispatch_method(&store, None, &req, None);
        match frame {
            Frame::Success(resp) => {
                let tools = resp.result["tools"].as_array().expect("tools array");
                let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
                for expected in [
                    "brain_context",
                    "brain_search",
                    "note_get",
                    "backlinks",
                    "brain_status",
                    "brain_add_source",
                    "cross_repo_contracts",
                    "brain_impact",
                    "brain_guide",
                    "flow_trace",
                    "detect_changes",
                    "clusters",
                    "stale_check",
                    "set_extension",
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
                    "investigate",
                    "investigate_expand",
                    "investigate_hydrate",
                    "contract_drift",
                    "brain_memory_lint",
                    "brain_memory_consolidate",
                    "brain_memory_related",
                ] {
                    assert!(names.contains(&expected), "missing tool: {expected}");
                }
                assert_eq!(tools.len(), 40, "expected 40 tools, got {}", tools.len());
                // Every tool has a description leading with usage guidance.
                for tool in tools {
                    let desc = tool["description"].as_str().expect("description");
                    assert!(!desc.is_empty(), "empty description");
                }
            }
            Frame::Error(e) => panic!("tools/list should succeed: {}", e.error.message),
        }
    }

    #[test]
    fn tools_call_with_unknown_tool_returns_error_envelope() {
        let store = GraphStore::in_memory().unwrap();
        let req = make_request(
            "tools/call",
            3,
            json!({ "name": "no_such_tool", "arguments": {} }),
        );
        let frame = dispatch_method(&store, None, &req, None);
        // Tool errors come back as success frames with isError=true (the
        // intentional design — Claude sees the error in-band).
        match frame {
            Frame::Success(resp) => {
                assert_eq!(resp.result["isError"], json!(true));
            }
            Frame::Error(e) => panic!("expected in-band error envelope, got {}", e.error.message),
        }
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let store = GraphStore::in_memory().unwrap();
        let req = make_request("not_a_real_method", 4, json!({}));
        let frame = dispatch_method(&store, None, &req, None);
        match frame {
            Frame::Error(e) => {
                assert_eq!(e.error.code, error_code::METHOD_NOT_FOUND);
            }
            Frame::Success(_) => panic!("expected method-not-found error"),
        }
    }

    #[test]
    fn brain_status_works_on_empty_store() {
        let store = GraphStore::in_memory().unwrap();
        let req = make_request(
            "tools/call",
            5,
            json!({ "name": "brain_status", "arguments": {} }),
        );
        let frame = dispatch_method(&store, None, &req, None);
        match frame {
            Frame::Success(resp) => {
                let structured = &resp.result["structuredContent"];
                assert_eq!(structured["notes"], json!(0));
                assert_eq!(structured["vault_count"], json!(0));
                assert_eq!(resp.result["isError"], json!(false));
                // Tantivy fields: no index passed → unavailable.
                assert_eq!(structured["tantivy_available"], json!(false));
                assert_eq!(structured["tantivy_doc_count"], json!(0));
            }
            Frame::Error(e) => panic!("brain_status should succeed: {}", e.error.message),
        }
    }

    #[test]
    fn batch_request_dispatch_round_trip() {
        // JSON-RPC 2.0 §6: a batch is an array of request objects. We
        // verify the same parse + dispatch pattern that run_stdio_server's
        // batch path uses — single line in, N responses out.
        let store = GraphStore::in_memory().unwrap();
        let batch_json = serde_json::json!([
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05" }
            },
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }
        ]);
        let arr = batch_json.as_array().expect("batch is an array");
        assert_eq!(arr.len(), 2, "batch should contain 2 requests");

        let mut responses: Vec<Value> = Vec::new();
        for item in arr {
            let req: protocol::Request = serde_json::from_value(item.clone()).unwrap();
            let outcome = dispatch_method(&store, None, &req, None);
            let val = match outcome {
                Frame::Success(r) => serde_json::to_value(r).unwrap(),
                Frame::Error(e) => serde_json::to_value(e).unwrap(),
            };
            responses.push(val);
        }
        assert_eq!(responses.len(), 2, "batch produces 2 responses");
        assert_eq!(responses[0]["id"], json!(1));
        assert_eq!(responses[1]["id"], json!(2));
        // Spot-check both are well-formed JSON-RPC responses.
        assert_eq!(responses[0]["jsonrpc"], json!("2.0"));
        assert!(responses[0].get("result").is_some());
        assert!(responses[1]["result"]["tools"].is_array());
    }

    #[test]
    fn batch_with_notification_omits_its_response() {
        // Per JSON-RPC 2.0 §6 / §4.1: notifications inside a batch do NOT
        // contribute responses. Our loop's notification short-circuit
        // implements this.
        let store = GraphStore::in_memory().unwrap();
        let mixed = serde_json::json!([
            {"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {}},
            {"jsonrpc": "2.0",          "method": "notifications/initialized"}
        ]);
        let arr = mixed.as_array().unwrap();
        let mut responses: Vec<Value> = Vec::new();
        for item in arr {
            let req: protocol::Request = serde_json::from_value(item.clone()).unwrap();
            let is_notification = req.id.is_none();
            let outcome = dispatch_method(&store, None, &req, None);
            if is_notification {
                continue;
            }
            let val = match outcome {
                Frame::Success(r) => serde_json::to_value(r).unwrap(),
                Frame::Error(e) => serde_json::to_value(e).unwrap(),
            };
            responses.push(val);
        }
        assert_eq!(responses.len(), 1, "notification produces no response");
        assert_eq!(responses[0]["id"], json!(7));
    }

    #[test]
    fn brain_search_returns_empty_for_no_match() {
        let store = GraphStore::in_memory().unwrap();
        let req = make_request(
            "tools/call",
            6,
            json!({ "name": "brain_search", "arguments": { "query": "nope" } }),
        );
        let frame = dispatch_method(&store, None, &req, None);
        match frame {
            Frame::Success(resp) => {
                assert_eq!(resp.result["isError"], json!(false));
                assert_eq!(resp.result["structuredContent"]["total_matches"], json!(0));
            }
            Frame::Error(e) => panic!("brain_search should succeed: {}", e.error.message),
        }
    }

    // ── interaction telemetry helper tests ────────────────────────────────

    #[test]
    fn extract_string_array_returns_strings() {
        let args = json!({ "seeds": ["a", "b", "c"] });
        let result = extract_string_array(&args, "seeds");
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn extract_string_array_handles_missing_key() {
        let args = json!({});
        let result = extract_string_array(&args, "seeds");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_string_array_handles_non_array() {
        let args = json!({ "seeds": "not_an_array" });
        let result = extract_string_array(&args, "seeds");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_string_array_filters_non_strings() {
        let args = json!({ "seeds": ["a", 42, "b", null] });
        let result = extract_string_array(&args, "seeds");
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn extract_result_uids_from_brain_context_format() {
        let result = json!({
            "seeds_expanded": 1,
            "connected": [
                { "uid": "sym:repo:a:hash:1", "kind": "Symbol", "title": "foo" },
                { "uid": "note:vlt:b:hash:2", "kind": "Note", "title": "bar" },
            ],
            "tokens_used": 100,
        });
        let uids = extract_result_uids(&result);
        assert_eq!(uids, vec!["sym:repo:a:hash:1", "note:vlt:b:hash:2"]);
    }

    #[test]
    fn extract_result_uids_from_brain_search_format() {
        let result = json!({
            "query": "test",
            "engine": "bm25",
            "results": [
                { "uid": "note:vlt:x:hash:1", "kind": "note", "title": "Test" },
                { "uid": "note:vlt:x:hash:2", "kind": "note", "title": "Test2" },
            ],
            "total_matches": 2,
        });
        let uids = extract_result_uids(&result);
        assert_eq!(uids, vec!["note:vlt:x:hash:1", "note:vlt:x:hash:2"]);
    }

    #[test]
    fn extract_result_uids_returns_empty_for_unknown_format() {
        let result = json!({ "something_else": true });
        let uids = extract_result_uids(&result);
        assert!(uids.is_empty());
    }

    #[test]
    fn record_interaction_records_query_for_brain_context() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);

        let args = json!({ "seeds": ["AuthService"] });
        let result = json!({
            "connected": [
                { "uid": "sym:a", "kind": "Symbol", "title": "AuthService" }
            ]
        });

        record_interaction(&tracker, "brain_context", &args, &result);
        assert_eq!(tracker.pending_count(), 1);
    }

    #[test]
    fn record_interaction_records_access_for_note_get() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);

        let args = json!({ "uid": "note:vlt:x:hash:1" });
        let result = json!({ "uid": "note:vlt:x:hash:1", "title": "Test" });

        record_interaction(&tracker, "note_get", &args, &result);
        assert_eq!(tracker.pending_count(), 1);
    }

    #[test]
    fn record_interaction_records_impact_for_brain_impact() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);

        let args = json!({ "seeds": ["sym:xyz"] });
        let result = json!({ "target": "sym:xyz", "impact_nodes": [] });

        record_interaction(&tracker, "brain_impact", &args, &result);
        assert_eq!(tracker.pending_count(), 1);
    }

    #[test]
    fn record_interaction_ignores_untracked_tools() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);

        let args = json!({});
        let result = json!({});

        record_interaction(&tracker, "brain_status", &args, &result);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn record_interaction_skips_access_when_no_uid() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);

        // note_get called with title instead of uid — no uid to record.
        let args = json!({ "title": "My Note" });
        let result = json!({ "uid": "note:abc", "title": "My Note" });

        record_interaction(&tracker, "note_get", &args, &result);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn terminal_success_recorded_when_last_call_was_not_search() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);

        // Query surfaces a result, then the agent reads it (a non-search
        // tool) and stops — a successful terminal retrieval.
        record_interaction(
            &tracker,
            "brain_context",
            &json!({ "seeds": ["AuthService"] }),
            &json!({ "connected": [{ "uid": "sym:a", "kind": "Symbol" }] }),
        );
        record_interaction(
            &tracker,
            "note_get",
            &json!({ "uid": "sym:a" }),
            &json!({ "uid": "sym:a" }),
        );

        let before = tracker.pending_count();
        maybe_record_terminal_success(&tracker);
        assert_eq!(
            tracker.pending_count(),
            before + 1,
            "should record a terminal-success event"
        );
    }

    #[test]
    fn terminal_success_skipped_when_last_call_was_search() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);

        // Agent's last action was a search — it was still looking.
        record_interaction(
            &tracker,
            "brain_search",
            &json!({ "query": "auth" }),
            &json!({ "results": [{ "uid": "sym:a", "kind": "Symbol" }] }),
        );

        let before = tracker.pending_count();
        maybe_record_terminal_success(&tracker);
        assert_eq!(
            tracker.pending_count(),
            before,
            "should NOT record terminal success after a search"
        );
    }

    #[test]
    fn terminal_success_skipped_when_no_activity() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let tracker = nestweaver_engine::InteractionTracker::new(&db_path);
        maybe_record_terminal_success(&tracker);
        assert_eq!(tracker.pending_count(), 0);
    }
}

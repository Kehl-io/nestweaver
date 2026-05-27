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

pub mod protocol;
pub mod tools;

use protocol::{ErrorResponse, PROTOCOL_VERSION, Response, error, error_code, success};

const SERVER_NAME: &str = "nestweaver-brain";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Canonical sidecar location for the Tantivy index. Lives next to the
/// LadybugDB file, mirroring the `.pagerank.json` convention.
pub fn tantivy_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".tantivy");
    std::path::PathBuf::from(s)
}

/// Run the brain server on stdio until the client closes stdin or sends
/// no more lines. Returns Ok on clean shutdown; errors only on truly
/// unrecoverable conditions (the database failing to open, etc.). Per-call
/// errors are wrapped as MCP tool errors and the loop continues.
pub fn run_stdio_server(
    db_path: &Path,
    allow_add_sources: bool,
    lite: bool,
) -> Result<(), anyhow::Error> {
    let store = GraphStore::open_or_readonly(db_path)
        .with_context(|| format!("open GraphStore at {}", db_path.display()))?;
    // Pre-load the PageRank sidecar if present — same behaviour as the CLI.
    let pr_path = db_path.with_extension("pagerank.json");
    let _ = store.load_pagerank_cache(&pr_path);

    // Open the Tantivy index sidecar. Best-effort: if it can't open
    // (corrupt segments, version skew, etc.) we log and fall back to
    // pure-PPR retrieval + substring search.
    let tantivy_path = tantivy_sidecar_path(db_path);
    let tantivy = match TantivyIndex::open_or_create(&tantivy_path) {
        Ok(idx) => {
            tracing::info!(
                docs = idx.doc_count(),
                path = %tantivy_path.display(),
                "Tantivy index open"
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
    // additional indexer invocations against the same DB.
    tools::set_current_db_path(db_path.to_path_buf());
    tools::set_allow_add_sources(allow_add_sources);
    tools::set_lite_mode(lite);

    tracing::info!(
        path = %db_path.display(),
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
                let outcome = dispatch_method(&store, tantivy.as_ref(), &req);
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
            let outcome = dispatch_method(&store, tantivy.as_ref(), &req);
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
                }
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

            match tools::dispatch(store, tantivy, &name, arguments) {
                Ok(result) => Frame::Success(success(id, tools::wrap_tool_result(result))),
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
        let frame = dispatch_method(&store, None, &req);
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
        let frame = dispatch_method(&store, None, &req);
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
                ] {
                    assert!(names.contains(&expected), "missing tool: {expected}");
                }
                assert_eq!(tools.len(), 17, "expected 17 tools, got {}", tools.len());
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
        let frame = dispatch_method(&store, None, &req);
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
        let frame = dispatch_method(&store, None, &req);
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
        let frame = dispatch_method(&store, None, &req);
        match frame {
            Frame::Success(resp) => {
                let structured = &resp.result["structuredContent"];
                assert_eq!(structured["notes"], json!(0));
                assert_eq!(structured["vault_count"], json!(0));
                assert_eq!(resp.result["isError"], json!(false));
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
            let outcome = dispatch_method(&store, None, &req);
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
            let outcome = dispatch_method(&store, None, &req);
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
        let frame = dispatch_method(&store, None, &req);
        match frame {
            Frame::Success(resp) => {
                assert_eq!(resp.result["isError"], json!(false));
                assert_eq!(resp.result["structuredContent"]["total_matches"], json!(0));
            }
            Frame::Error(e) => panic!("brain_search should succeed: {}", e.error.message),
        }
    }
}

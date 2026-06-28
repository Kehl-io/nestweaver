//! MCP-over-HTTP endpoint.
//!
//! Provides a minimal axum HTTP server that accepts `POST /mcp` with
//! JSON-RPC 2.0 bodies.  Handles `initialize`, `tools/list`, and
//! `tools/call` — the latter delegates to the same `tools::dispatch`
//! function used by the stdio server.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use axum::{Json, Router, extract::State, routing::post};
use dashmap::DashMap;
use nestweaver_store::{GraphStore, TantivyIndex};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::protocol::{PROTOCOL_VERSION, error_code};
use crate::tools;

const SERVER_NAME: &str = "nestweaver-brain";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a session can be idle before the sweeper removes it.
const SESSION_TTL_SECS: u64 = 3600; // 1 hour

/// How often the background sweeper runs.
const SWEEP_INTERVAL_SECS: u64 = 300; // 5 minutes

/// Default per-tool timeout for MCP HTTP requests.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 30;

/// Hard cap on graph traversal depth parameters.
const MAX_DEPTH: u64 = 15;

/// Hard cap on result count parameters (limit / max_results).
const MAX_RESULTS: u64 = 5_000;

/// Requests allowed per session per minute before rate limiting kicks in.
const RATE_LIMIT_PER_MIN: u64 = 120;

/// Per-client MCP session metadata.
#[derive(Debug)]
pub struct McpSession {
    pub id: String,
    pub created_at: Instant,
    pub last_active: Instant,
    pub request_count: u64,
}

/// Shared state for the MCP HTTP handler.
///
/// Holds references to the graph store and search index so `tools/call`
/// can dispatch through the same path as the stdio server.
pub struct McpHttpState {
    pub lite: bool,
    pub store: Arc<GraphStore>,
    pub tantivy: Option<Arc<TantivyIndex>>,
    pub db_path: PathBuf,
    pub instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
    pub sessions: Arc<DashMap<String, McpSession>>,
    /// Whether the daemon is running in server mode. Threaded into the tool
    /// dispatch thread-local so server-only code paths (e.g. `read_symbols`
    /// reading content via `git show` from blobless bare clones, `brain_status`
    /// reporting) behave correctly over HTTP, matching the gRPC handler.
    pub server_mode: bool,
    /// Optional bearer token for MCP-over-HTTP authentication. When set,
    /// requests must include `Authorization: Bearer <token>` or receive 401.
    pub auth_token: Option<String>,
}

impl McpHttpState {
    /// Create a new state with an empty session registry and no auth.
    pub fn new(
        lite: bool,
        store: Arc<GraphStore>,
        tantivy: Option<Arc<TantivyIndex>>,
        db_path: PathBuf,
        instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
        server_mode: bool,
    ) -> Self {
        Self {
            lite,
            store,
            tantivy,
            db_path,
            instance_cfg,
            sessions: Arc::new(DashMap::new()),
            server_mode,
            auth_token: None,
        }
    }

    /// Create a new state with bearer token authentication enabled.
    pub fn with_auth(
        lite: bool,
        store: Arc<GraphStore>,
        tantivy: Option<Arc<TantivyIndex>>,
        db_path: PathBuf,
        instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
        server_mode: bool,
        auth_token: String,
    ) -> Self {
        Self {
            lite,
            store,
            tantivy,
            db_path,
            instance_cfg,
            sessions: Arc::new(DashMap::new()),
            server_mode,
            auth_token: Some(auth_token),
        }
    }
}

/// Spawn a background task that removes sessions idle longer than `SESSION_TTL_SECS`.
pub fn spawn_session_sweeper(sessions: Arc<DashMap<String, McpSession>>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(SWEEP_INTERVAL_SECS);
        let ttl = std::time::Duration::from_secs(SESSION_TTL_SECS);
        loop {
            tokio::time::sleep(interval).await;
            let now = Instant::now();
            sessions.retain(|_id, session| now.duration_since(session.last_active) < ttl);
        }
    });
}

/// Clamp depth and result-count parameters in tool arguments to server caps.
///
/// Mutates the `arguments` JSON object in place, capping `depth` to
/// [`MAX_DEPTH`] and `limit` / `max_results` to [`MAX_RESULTS`].
fn clamp_safeguard_params(arguments: &mut Value) {
    if let Some(obj) = arguments.as_object_mut() {
        for key in &["depth"] {
            if let Some(val) = obj.get_mut(*key) {
                if let Some(n) = val.as_u64() {
                    if n > MAX_DEPTH {
                        tracing::warn!(param = *key, requested = n, capped = MAX_DEPTH, "clamped parameter");
                        *val = Value::Number(serde_json::Number::from(MAX_DEPTH));
                    }
                }
            }
        }
        for key in &["limit", "max_results"] {
            if let Some(val) = obj.get_mut(*key) {
                if let Some(n) = val.as_u64() {
                    if n > MAX_RESULTS {
                        tracing::warn!(param = *key, requested = n, capped = MAX_RESULTS, "clamped parameter");
                        *val = Value::Number(serde_json::Number::from(MAX_RESULTS));
                    }
                }
            }
        }
    }
}

/// Check per-session rate limit. Returns `true` if the request is allowed.
/// Uses a simple sliding-window approximation: if the session has made more
/// than `RATE_LIMIT_PER_MIN` requests and the last reset was less than 60s
/// ago, reject.
fn check_session_rate_limit(sessions: &DashMap<String, McpSession>, session_id: &str) -> bool {
    if let Some(mut entry) = sessions.get_mut(session_id) {
        let elapsed = entry.last_active.elapsed();
        // Reset window every 60 seconds.
        if elapsed >= Duration::from_secs(60) {
            entry.request_count = 1;
            entry.last_active = Instant::now();
            return true;
        }
        if entry.request_count > RATE_LIMIT_PER_MIN {
            return false;
        }
        entry.request_count += 1;
        entry.last_active = Instant::now();
        true
    } else {
        // Unknown session — allow (session tracking will create one on initialize).
        true
    }
}

/// Build an axum [`Router`] that serves `POST /mcp`.
pub fn router(state: Arc<McpHttpState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .with_state(state)
}

/// JSON-RPC request as received over HTTP (same shape as the stdio wire
/// format but parsed from the request body instead of a line).
#[derive(serde::Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[allow(dead_code)]
    #[serde(default)]
    params: Option<Value>,
}

async fn handle_mcp(
    State(state): State<Arc<McpHttpState>>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> (axum::http::StatusCode, HeaderMap, Json<Value>) {
    // Validate bearer token when auth is configured.
    if let Some(ref expected) = state.auth_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match provided {
            Some(t) if t == expected => {}
            _ => {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    HeaderMap::new(),
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": {
                            "code": error_code::INVALID_REQUEST,
                            "message": "unauthorized: valid Bearer token required",
                        }
                    })),
                );
            }
        }
    }

    let id = req.id.clone().unwrap_or(Value::Null);

    // Track the session: look up an existing one or note that we need a new one.
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    // Per-session rate limiting (server mode only).
    if state.server_mode {
        if let Some(ref sid) = session_id {
            if !check_session_rate_limit(&state.sessions, sid) {
                return (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    HeaderMap::new(),
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": {
                            "code": error_code::INVALID_REQUEST,
                            "message": "rate limit exceeded: too many requests per minute",
                        }
                    })),
                );
            }
        }
    } else {
        // Non-server mode: just update last_active / request_count.
        if let Some(ref sid) = session_id {
            if let Some(mut entry) = state.sessions.get_mut(sid) {
                entry.last_active = Instant::now();
                entry.request_count += 1;
            }
        }
    }

    let response = match req.method.as_str() {
        "initialize" => {
            // Always create a fresh session on initialize.
            let new_id = Uuid::new_v4().to_string();
            state.sessions.insert(
                new_id.clone(),
                McpSession {
                    id: new_id.clone(),
                    created_at: Instant::now(),
                    last_active: Instant::now(),
                    request_count: 1,
                },
            );

            let mut resp_headers = HeaderMap::new();
            resp_headers.insert("mcp-session-id", new_id.parse().unwrap());

            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "version": SERVER_VERSION,
                    },
                    "instructions": crate::SERVER_INSTRUCTIONS,
                }
            });

            return (axum::http::StatusCode::OK, resp_headers, Json(body));
        }

        "notifications/initialized" | "initialized" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": null,
        }),

        "tools/list" => {
            let tool_list = tools::tool_list(state.lite);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": tool_list,
            })
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
                return (
                    axum::http::StatusCode::OK,
                    HeaderMap::new(),
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": error_code::INVALID_PARAMS,
                            "message": "tools/call: 'name' is required",
                        }
                    })),
                );
            };

            let store = state.store.clone();
            let tantivy = state.tantivy.clone();
            let db_path = state.db_path.clone();
            let instance_cfg = state.instance_cfg.clone();
            let lite = state.lite;
            let server_mode = state.server_mode;

            // Clamp depth / result-count parameters to server caps so MCP
            // clients cannot request unbounded traversals or result sets.
            let mut arguments = arguments;
            if server_mode {
                clamp_safeguard_params(&mut arguments);
            }

            // Run tool dispatch on a blocking thread — graph queries are
            // CPU-bound and must not starve the tokio runtime.
            // Wrap in a timeout to match the gRPC safeguard behaviour.
            let timeout = Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS);
            let tool_name = name.clone();
            let result = tokio::time::timeout(
                timeout,
                tokio::task::spawn_blocking(move || {
                    tools::set_current_db_path(db_path);
                    tools::set_lite_mode(lite);
                    tools::set_current_instance_config(instance_cfg);
                    // Match the gRPC handler: server-only code paths (read_symbols
                    // via git, brain_status) key off this thread-local. Without it,
                    // HTTP requests in server mode read from an empty filesystem and
                    // return empty bodies.
                    tools::set_server_mode(server_mode);

                    tools::dispatch(&store, tantivy.as_deref(), &tool_name, arguments, None)
                }),
            )
            .await;

            match result {
                Ok(Ok(Ok(value))) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_result(value),
                }),
                Ok(Ok(Err(e))) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_error(&e.to_string()),
                }),
                Ok(Err(e)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_error(&format!("dispatch panicked: {e}")),
                }),
                Err(_elapsed) => {
                    tracing::warn!(tool = %name, timeout_secs = DEFAULT_TOOL_TIMEOUT_SECS, "MCP tool dispatch timed out");
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": tools::wrap_tool_error(&format!(
                            "{name} query exceeded {DEFAULT_TOOL_TIMEOUT_SECS}s timeout"
                        )),
                    })
                }
            }
        }

        "ping" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {},
        }),

        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": error_code::METHOD_NOT_FOUND,
                "message": format!("method not implemented: {other}"),
            }
        }),
    };

    (axum::http::StatusCode::OK, HeaderMap::new(), Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_app() -> Router {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let state = Arc::new(McpHttpState::new(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            false,
        ));
        router(state)
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn tools_list_returns_tools() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 2);
        let tools = json["result"]["tools"].as_array().expect("tools array");
        assert!(tools.len() >= 30, "expected 30+ tools, got {}", tools.len());
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "no/such/method",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 3);
        assert_eq!(json["error"]["code"], error_code::METHOD_NOT_FOUND);
    }
}

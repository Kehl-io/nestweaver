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
    pub rate_window_start: Instant,
}

#[derive(Debug)]
struct HttpTokenBucket {
    tokens: f64,
    last_refill: Instant,
}

/// Simple per-client token bucket for stateless MCP-over-HTTP requests.
pub struct HttpRateLimiter {
    buckets: DashMap<String, HttpTokenBucket>,
    capacity: f64,
    refill_per_sec: f64,
    /// Source of the current time. Injectable so tests can freeze the clock
    /// and exercise refill behavior deterministically; production uses
    /// `Instant::now`.
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl std::fmt::Debug for HttpRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRateLimiter")
            .field("buckets", &self.buckets)
            .field("capacity", &self.capacity)
            .field("refill_per_sec", &self.refill_per_sec)
            .finish_non_exhaustive()
    }
}

impl HttpRateLimiter {
    fn new(requests_per_min: u64) -> Self {
        Self::new_with_clock(requests_per_min, Arc::new(Instant::now))
    }

    fn new_with_clock(
        requests_per_min: u64,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            buckets: DashMap::new(),
            capacity: requests_per_min as f64,
            refill_per_sec: requests_per_min as f64 / 60.0,
            clock,
        }
    }

    fn check(&self, client_key: &str) -> bool {
        let now = (self.clock)();
        let mut bucket = self
            .buckets
            .entry(client_key.to_string())
            .or_insert_with(|| HttpTokenBucket {
                tokens: self.capacity,
                last_refill: now,
            });

        let elapsed_secs = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed_secs * self.refill_per_sec).min(self.capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
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
    /// Optional admin bearer token. When query auth is enabled, this token is
    /// also accepted so MCP and gRPC query auth share the same semantics.
    pub admin_token: Option<String>,
    pub client_rate_limiter: Arc<HttpRateLimiter>,
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
            admin_token: None,
            client_rate_limiter: Arc::new(HttpRateLimiter::new(RATE_LIMIT_PER_MIN)),
        }
    }

    /// Create a new state with bearer token authentication enabled.
    #[allow(clippy::too_many_arguments)]
    pub fn with_auth(
        lite: bool,
        store: Arc<GraphStore>,
        tantivy: Option<Arc<TantivyIndex>>,
        db_path: PathBuf,
        instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
        server_mode: bool,
        auth_token: String,
        admin_token: Option<String>,
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
            admin_token,
            client_rate_limiter: Arc::new(HttpRateLimiter::new(RATE_LIMIT_PER_MIN)),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedLimit {
    param: &'static str,
    requested: u64,
    applied: u64,
}

/// Validate depth parameters and cap result-count parameters in tool arguments.
///
/// Depth above [`MAX_DEPTH`] is rejected because traversals become ambiguous
/// and expensive. Result counts above [`MAX_RESULTS`] are capped and disclosed
/// on the MCP response metadata so callers know the returned page was bounded.
fn apply_safeguard_params(arguments: &mut Value) -> Result<Vec<AppliedLimit>, String> {
    let mut limits = Vec::new();
    if let Some(obj) = arguments.as_object_mut() {
        for key in &["depth", "max_depth"] {
            if let Some(val) = obj.get_mut(*key)
                && let Some(n) = val.as_u64()
                && n > MAX_DEPTH
            {
                return Err(format!(
                    "invalid-argument: parameter '{key}' requested depth {n}, maximum allowed depth is {MAX_DEPTH}"
                ));
            }
        }
        for key in &["limit", "max_results"] {
            if let Some(val) = obj.get_mut(*key)
                && let Some(n) = val.as_u64()
                && n > MAX_RESULTS
            {
                tracing::warn!(
                    param = key,
                    requested = n,
                    capped = MAX_RESULTS,
                    "clamped parameter"
                );
                limits.push(AppliedLimit {
                    param: key,
                    requested: n,
                    applied: MAX_RESULTS,
                });
                *val = Value::Number(serde_json::Number::from(MAX_RESULTS));
            }
        }
    }
    Ok(limits)
}

fn add_limit_metadata(mut result: Value, limits: &[AppliedLimit]) -> Value {
    if limits.is_empty() {
        return result;
    }

    if let Some(obj) = result.as_object_mut() {
        let meta = obj.entry("_meta").or_insert_with(|| json!({}));
        if let Some(meta_obj) = meta.as_object_mut() {
            meta_obj.insert(
                "limits".to_string(),
                Value::Array(
                    limits
                        .iter()
                        .map(|limit| {
                            json!({
                                "param": limit.param,
                                "requested": limit.requested,
                                "applied": limit.applied,
                                "reason": "server_cap",
                            })
                        })
                        .collect(),
                ),
            );
        }
    }

    result
}

/// Check per-session rate limit. Returns `true` if the request is allowed.
fn check_session_rate_limit(sessions: &DashMap<String, McpSession>, session_id: &str) -> bool {
    if let Some(mut entry) = sessions.get_mut(session_id) {
        let now = Instant::now();
        let elapsed = now.duration_since(entry.rate_window_start);
        if elapsed >= Duration::from_secs(60) {
            entry.request_count = 1;
            entry.rate_window_start = now;
            entry.last_active = now;
            return true;
        }
        if entry.request_count >= RATE_LIMIT_PER_MIN {
            return false;
        }
        entry.request_count += 1;
        entry.last_active = now;
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
    let provided_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let mut admin_bypass_rate_limit = false;

    // Validate bearer token when auth is configured.
    if let Some(ref expected) = state.auth_token {
        match provided_bearer {
            Some(t)
                if {
                    use subtle::ConstantTimeEq;
                    let query_match = bool::from(t.as_bytes().ct_eq(expected.as_bytes()));
                    let admin_match = state
                        .admin_token
                        .as_ref()
                        .map(|admin| bool::from(t.as_bytes().ct_eq(admin.as_bytes())))
                        .unwrap_or(false);
                    admin_bypass_rate_limit = admin_match;
                    query_match || admin_match
                } => {}
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

    // Reject unknown session IDs (except for `initialize` which creates one).
    // If a client sends a session ID that isn't in our DashMap, it likely
    // expired or belongs to a previous server instance — ask it to re-init.
    if let Some(ref sid) = session_id
        && req.method != "initialize"
        && !state.sessions.contains_key(sid)
    {
        return (
            axum::http::StatusCode::OK,
            HeaderMap::new(),
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": error_code::INVALID_REQUEST,
                    "message": "unknown or expired session ID — please re-initialize",
                }
            })),
        );
    }

    // Per-session rate limiting (server mode only).
    if state.server_mode {
        let client_key = if provided_bearer.is_some() {
            if admin_bypass_rate_limit {
                "bearer:admin".to_string()
            } else {
                "bearer:query".to_string()
            }
        } else if let Some(ref sid) = session_id {
            format!("session:{sid}")
        } else {
            "anonymous-stateless".to_string()
        };

        if !admin_bypass_rate_limit && !state.client_rate_limiter.check(&client_key) {
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

        if let Some(ref sid) = session_id {
            let _ = check_session_rate_limit(&state.sessions, sid);
        }
    } else {
        // Non-server mode: just update last_active / request_count.
        if let Some(ref sid) = session_id
            && let Some(mut entry) = state.sessions.get_mut(sid)
        {
            entry.last_active = Instant::now();
            entry.request_count += 1;
        }
    }

    let response = match req.method.as_str() {
        "initialize" => {
            // Always create a fresh session on initialize.
            let new_id = Uuid::new_v4().to_string();
            let now = Instant::now();
            state.sessions.insert(
                new_id.clone(),
                McpSession {
                    id: new_id.clone(),
                    created_at: now,
                    last_active: now,
                    request_count: 1,
                    rate_window_start: now,
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

            // Validate depth and cap result-count parameters to server caps so
            // MCP clients cannot request unbounded traversals or result sets.
            let mut arguments = arguments;
            let mut applied_limits = Vec::new();
            if server_mode {
                match apply_safeguard_params(&mut arguments) {
                    Ok(limits) => applied_limits = limits,
                    Err(message) => {
                        return (
                            axum::http::StatusCode::OK,
                            HeaderMap::new(),
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": error_code::INVALID_PARAMS,
                                    "message": message,
                                }
                            })),
                        );
                    }
                }
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
                Ok(Ok(Ok(value))) => {
                    let result =
                        add_limit_metadata(tools::wrap_tool_result(value), &applied_limits);
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    })
                }
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

    fn test_auth_app() -> Router {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let state = Arc::new(McpHttpState::with_auth(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            false,
            "query-token".to_string(),
            Some("admin-token".to_string()),
        ));
        router(state)
    }

    #[test]
    fn safeguard_rejects_over_depth_instead_of_clamping() {
        let mut args = json!({
            "depth": MAX_DEPTH + 1,
            "limit": 10,
        });

        let err = apply_safeguard_params(&mut args).unwrap_err();

        assert!(err.contains("invalid-argument"));
        assert!(err.contains("maximum allowed depth"));
        assert_eq!(args["depth"], MAX_DEPTH + 1);
    }

    #[test]
    fn safeguard_caps_result_limit_and_records_metadata() {
        let mut args = json!({
            "limit": MAX_RESULTS + 25,
        });

        let limits = apply_safeguard_params(&mut args).unwrap();
        let result = add_limit_metadata(tools::wrap_tool_result(json!({"ok": true})), &limits);

        assert_eq!(args["limit"], MAX_RESULTS);
        assert_eq!(result["_meta"]["limits"][0]["param"], "limit");
        assert_eq!(result["_meta"]["limits"][0]["requested"], MAX_RESULTS + 25);
        assert_eq!(result["_meta"]["limits"][0]["applied"], MAX_RESULTS);
        assert_eq!(result["structuredContent"]["ok"], true);
    }

    #[test]
    fn rate_limit_rejects_at_limit_within_window() {
        let sessions = DashMap::new();
        let now = Instant::now();
        sessions.insert(
            "sid".to_string(),
            McpSession {
                id: "sid".to_string(),
                created_at: now,
                last_active: now,
                request_count: RATE_LIMIT_PER_MIN,
                rate_window_start: now,
            },
        );

        assert!(!check_session_rate_limit(&sessions, "sid"));
    }

    #[test]
    fn rate_limit_resets_by_window_start_not_last_activity() {
        let sessions = DashMap::new();
        let now = Instant::now();
        sessions.insert(
            "sid".to_string(),
            McpSession {
                id: "sid".to_string(),
                created_at: now - Duration::from_secs(120),
                last_active: now,
                request_count: RATE_LIMIT_PER_MIN,
                rate_window_start: now - Duration::from_secs(61),
            },
        );

        assert!(check_session_rate_limit(&sessions, "sid"));
        let session = sessions.get("sid").unwrap();
        assert_eq!(session.request_count, 1);
    }

    #[test]
    fn http_rate_limiter_rejects_after_capacity_with_frozen_clock() {
        // Freeze the clock so the bucket never refills, making the rate-limit
        // boundary exact and independent of wall-clock timing.
        let frozen = Instant::now();
        let limiter = HttpRateLimiter::new_with_clock(RATE_LIMIT_PER_MIN, Arc::new(move || frozen));

        // The first RATE_LIMIT_PER_MIN requests for one client consume the
        // full bucket and succeed.
        for i in 0..RATE_LIMIT_PER_MIN {
            assert!(
                limiter.check("client-a"),
                "request {i} should be allowed within capacity"
            );
        }

        // The next request has no tokens left and is rejected (the 429 path).
        assert!(
            !limiter.check("client-a"),
            "request beyond capacity must be rate limited with a frozen clock"
        );

        // Rate limiting is per-client: a different key starts with a full bucket.
        assert!(
            limiter.check("client-b"),
            "a distinct client must not be limited by another client's usage"
        );
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
    async fn admin_token_is_accepted_for_mcp_http_query_auth() {
        for token in ["query-token", "admin-token"] {
            let app = test_auth_app();
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": token,
                "method": "tools/list",
            });
            let req = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(json["result"]["tools"].is_array(), "{json}");
        }
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

    #[tokio::test]
    async fn unknown_session_id_rejected() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/list",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("mcp-session-id", "nonexistent-session-id")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 4);
        assert_eq!(json["error"]["code"], error_code::INVALID_REQUEST);
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("re-initialize"),
        );
    }
}

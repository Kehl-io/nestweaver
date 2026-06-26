//! MCP-over-HTTP endpoint.
//!
//! Provides a minimal axum HTTP server that accepts `POST /mcp` with
//! JSON-RPC 2.0 bodies.  Handles `initialize`, `tools/list`, and
//! `tools/call` — the latter delegates to the same `tools::dispatch`
//! function used by the stdio server.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::post};
use nestweaver_store::{GraphStore, TantivyIndex};
use serde_json::{Value, json};

use crate::protocol::{PROTOCOL_VERSION, error_code};
use crate::tools;

const SERVER_NAME: &str = "nestweaver-brain";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    Json(req): Json<JsonRpcRequest>,
) -> Json<Value> {
    let id = req.id.clone().unwrap_or(Value::Null);

    let response = match req.method.as_str() {
        "initialize" => json!({
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
        }),

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
                return Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": error_code::INVALID_PARAMS,
                        "message": "tools/call: 'name' is required",
                    }
                }));
            };

            let store = state.store.clone();
            let tantivy = state.tantivy.clone();
            let db_path = state.db_path.clone();
            let instance_cfg = state.instance_cfg.clone();
            let lite = state.lite;

            // Run tool dispatch on a blocking thread — graph queries are
            // CPU-bound and must not starve the tokio runtime.
            let result = tokio::task::spawn_blocking(move || {
                tools::set_current_db_path(db_path);
                tools::set_lite_mode(lite);
                tools::set_current_instance_config(instance_cfg);

                tools::dispatch(&store, tantivy.as_deref(), &name, arguments, None)
            })
            .await;

            match result {
                Ok(Ok(value)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_result(value),
                }),
                Ok(Err(e)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_error(&e.to_string()),
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_error(&format!("dispatch panicked: {e}")),
                }),
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

    Json(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_app() -> Router {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let state = Arc::new(McpHttpState {
            lite: false,
            store,
            tantivy: None,
            db_path: PathBuf::from("/tmp/test.lbug"),
            instance_cfg: None,
        });
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

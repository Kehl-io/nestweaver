//! MCP-over-HTTP endpoint.
//!
//! Provides a minimal axum HTTP server that accepts `POST /mcp` with
//! JSON-RPC 2.0 bodies.  Currently handles `initialize` and `tools/list`;
//! full tool dispatch (`tools/call`) is wired up separately.

use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};

use crate::protocol::{PROTOCOL_VERSION, error_code};
use crate::tools;

const SERVER_NAME: &str = "nestweaver-brain";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared state for the MCP HTTP handler.
///
/// For now only `lite` mode matters (controls which tools `tools/list`
/// returns).  Task 10 will add `DaemonState` fields needed for
/// `tools/call` dispatch.
pub struct McpHttpState {
    pub lite: bool,
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
        let state = Arc::new(McpHttpState { lite: false });
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

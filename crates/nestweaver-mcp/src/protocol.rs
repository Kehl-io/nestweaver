//! JSON-RPC 2.0 + MCP wire types.
//!
//! Hand-rolled rather than pulling in a third-party SDK because the surface
//! the brain server needs is tiny: three methods (`initialize`, `tools/list`,
//! `tools/call`) plus the `notifications/initialized` notification, all
//! line-delimited JSON over stdio. Keeping it local also means dependency
//! drift in upstream MCP crates never breaks brain integration.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version we advertise. Matches the version Claude Code's MCP
/// client and Claude Desktop currently speak.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC 2.0 request. `id` is absent for notifications.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 success response.
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub result: Value,
}

/// JSON-RPC 2.0 error response.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub error: RpcError,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC error codes used by the brain server.
pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

pub fn success(id: Value, result: Value) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result,
    }
}

pub fn error(id: Value, code: i32, message: impl Into<String>) -> ErrorResponse {
    ErrorResponse {
        jsonrpc: "2.0",
        id,
        error: RpcError {
            code,
            message: message.into(),
            data: None,
        },
    }
}

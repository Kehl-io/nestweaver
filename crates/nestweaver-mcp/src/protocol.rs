//! JSON-RPC 2.0 + MCP wire types.
//!
//! Hand-rolled rather than pulling in a third-party SDK because the surface
//! the brain server needs is tiny: three methods (`initialize`, `tools/list`,
//! `tools/call`) plus the `notifications/initialized` notification, all
//! line-delimited JSON over stdio. Keeping it local also means dependency
//! drift in upstream MCP crates never breaks brain integration.

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;

/// Protocol version we advertise. Matches the version Claude Code's MCP
/// client and Claude Desktop currently speak.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC 2.0 request. `id` is absent for notifications.
#[derive(Debug)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug)]
pub struct InvalidRequest {
    /// Correlate envelope failures when the supplied ID itself is valid.
    /// Illegal ID shapes always use null, per JSON-RPC 2.0.
    pub response_id: Value,
    pub message: String,
}

pub fn validate_request(value: Value) -> Result<Request, InvalidRequest> {
    let Value::Object(mut object) = value else {
        return Err(InvalidRequest {
            response_id: Value::Null,
            message: "JSON-RPC request must be an object".to_string(),
        });
    };

    // Resolve ID validity first: every later envelope error can then preserve
    // a legal correlation ID, while a boolean/array/object ID must answer null.
    let id = match object.remove("id") {
        None => None,
        Some(value @ (Value::Null | Value::String(_) | Value::Number(_))) => Some(value),
        Some(_) => {
            return Err(InvalidRequest {
                response_id: Value::Null,
                message: "JSON-RPC request id must be a string, number, or null".to_string(),
            });
        }
    };
    let response_id = id.clone().unwrap_or(Value::Null);

    let jsonrpc = match object.remove("jsonrpc") {
        Some(Value::String(version)) if version == "2.0" => version,
        _ => {
            return Err(InvalidRequest {
                response_id,
                message: "jsonrpc must be exactly '2.0'".to_string(),
            });
        }
    };
    let method = match object.remove("method") {
        Some(Value::String(method)) => method,
        _ => {
            return Err(InvalidRequest {
                response_id,
                message: "JSON-RPC request method must be a string".to_string(),
            });
        }
    };
    // JSON-RPC 2.0: `params`, when present, MUST be structured — an array or
    // an object. A scalar is a malformed envelope, and the core methods
    // (`initialize`, `tools/list`, `ping`) never look at `params` at all, so
    // nothing downstream would ever reject one: `{"method":"ping","params":7}`
    // was answered with success.
    //
    // `null` keeps its carve-out deliberately. Real MCP clients send
    // `"params": null` for argument-less calls, and the strictness worth
    // having here is about malformed shapes, not about newly breaking those
    // clients — which is the compatibility policy this hardening already
    // chose. Method- and tool-specific validation still returns -32602 for
    // structurally valid params with wrong contents.
    let params = match object.remove("params") {
        None | Some(Value::Null) => None,
        Some(value @ (Value::Array(_) | Value::Object(_))) => Some(value),
        Some(_) => {
            return Err(InvalidRequest {
                response_id: id.unwrap_or(Value::Null),
                message: "JSON-RPC params must be an array or an object".to_string(),
            });
        }
    };
    Ok(Request {
        jsonrpc,
        id,
        method,
        params,
    })
}

impl std::fmt::Display for InvalidRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        validate_request(value).map_err(de::Error::custom)
    }
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

#[cfg(test)]
mod request_validation_tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> Result<Request, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn request_requires_jsonrpc_2_0() {
        for version in [json!("1.0"), json!("2.1"), Value::Null, json!(2)] {
            assert!(
                parse(json!({"jsonrpc": version, "id": 1, "method": "ping"})).is_err(),
                "accepted invalid jsonrpc version {version}"
            );
        }
        let error =
            validate_request(json!({"jsonrpc": "1.0", "id": "correlated", "method": "ping"}))
                .unwrap_err();
        assert_eq!(error.response_id, json!("correlated"));
    }

    #[test]
    fn request_rejects_illegal_id_shapes() {
        for id in [
            json!(true),
            json!(false),
            json!([]),
            json!([1]),
            json!({}),
            json!({"x": 1}),
        ] {
            assert!(
                parse(json!({"jsonrpc": "2.0", "id": id, "method": "ping"})).is_err(),
                "accepted illegal request id {id}"
            );
        }
        let error = validate_request(json!({
            "jsonrpc": "2.0", "id": true, "method": "ping"
        }))
        .unwrap_err();
        assert_eq!(error.response_id, Value::Null);
    }

    /// JSON-RPC 2.0 requires structured `params`. The core methods never read
    /// `params`, so a scalar reached no validator anywhere and was answered
    /// with success — `{"method":"ping","params":7}` returned a result.
    #[test]
    fn scalar_params_are_rejected_but_null_keeps_its_carve_out() {
        for scalar in [json!(false), json!(7), json!("x"), json!(1.5)] {
            let error = validate_request(json!({
                "jsonrpc": "2.0", "id": 1, "method": "ping", "params": scalar
            }))
            .expect_err("a scalar params must be refused");
            assert!(
                error.message.contains("array or an object"),
                "{}",
                error.message
            );
            // A legal ID still correlates the failure.
            assert_eq!(error.response_id, json!(1));
        }

        // Structured params still pass for every core method, including the
        // empty object real clients send.
        for method in ["initialize", "tools/list", "ping"] {
            for params in [json!({}), json!({"a": 1}), json!([])] {
                validate_request(json!({
                    "jsonrpc": "2.0", "id": 1, "method": method, "params": params
                }))
                .unwrap_or_else(|error| {
                    panic!("{method} must accept structured params: {}", error.message)
                });
            }
        }
    }

    #[test]
    fn request_preserves_valid_and_explicit_null_ids() {
        for id in [
            json!("request-1"),
            json!(0),
            json!(-1),
            json!(1.5),
            Value::Null,
        ] {
            let request = parse(json!({"jsonrpc": "2.0", "id": id, "method": "ping"})).unwrap();
            assert_eq!(request.id, Some(id));
        }
        let notification = parse(json!({"jsonrpc": "2.0", "method": "ping"})).unwrap();
        assert_eq!(notification.id, None);

        let explicit_null_params = parse(json!({
            "jsonrpc": "2.0", "id": 1, "method": "ping", "params": null
        }))
        .unwrap();
        assert_eq!(explicit_null_params.params, None);
    }
}

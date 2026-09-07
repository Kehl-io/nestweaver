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
        Some(value @ (Value::Null | Value::String(_))) => Some(value),
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
            Some(Value::Number(number))
        }
        Some(_) => {
            return Err(InvalidRequest {
                response_id: Value::Null,
                message: "JSON-RPC request id must be a string, null, or an integer in -9223372036854775808..=18446744073709551615; floating-point, exponent and out-of-range numeric IDs are not accepted".to_string(),
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
    // A tools/call is a request, never a notification. Reject before any
    // transport can dispatch it: response suppression is not write prevention.
    if method == "tools/call" && id.is_none() {
        return Err(InvalidRequest {
            response_id: Value::Null,
            message: "tools/call requires a correlated request id".to_string(),
        });
    }
    Ok(Request {
        jsonrpc,
        id,
        method,
        params,
    })
}

/// Validate MCP method parameters after envelope validation. All transports
/// use this same contract and report failures as INVALID_PARAMS, not tool errors.
/// `params: null` remains equivalent to omission for argument-less methods.
pub fn validate_method_params(req: &Request) -> Result<(), String> {
    let allowed: &[&str] = match req.method.as_str() {
        "initialize" => &["protocolVersion", "capabilities", "clientInfo", "_meta"],
        "ping" | "notifications/initialized" | "initialized" => &["_meta"],
        "tools/list" => &["cursor", "_meta"],
        "tools/call" => &["name", "arguments", "_meta"],
        "notifications/cancelled" => &["requestId", "reason", "_meta"],
        _ => return Ok(()),
    };
    let object = match req.params.as_ref() {
        None => None,
        Some(Value::Object(object)) => Some(object),
        Some(_) => return Err(format!("{} params must be an object", req.method)),
    };
    if let Some(object) = object {
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("{}: unknown parameter '{key}'", req.method));
            }
        }
        if object.get("_meta").is_some_and(|value| !value.is_object()) {
            return Err(format!("{}: '_meta' must be an object", req.method));
        }
    }
    let get = |key: &str| object.and_then(|object| object.get(key));
    let nonempty_string = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    match req.method.as_str() {
        "initialize" => {
            if !get("protocolVersion").is_some_and(Value::is_string) {
                return Err("initialize: 'protocolVersion' must be a string".into());
            }
            if !get("capabilities").is_some_and(Value::is_object) {
                return Err("initialize: 'capabilities' must be an object".into());
            }
            let info = get("clientInfo").and_then(Value::as_object);
            if !info
                .and_then(|info| info.get("name"))
                .is_some_and(Value::is_string)
                || !info
                    .and_then(|info| info.get("version"))
                    .is_some_and(Value::is_string)
            {
                return Err("initialize: 'clientInfo' requires string name and version".into());
            }
        }
        "tools/list" if get("cursor").is_some_and(|value| !value.is_string()) => {
            return Err("tools/list: 'cursor' must be a string".into());
        }
        "tools/call" => {
            if !nonempty_string(get("name")) {
                return Err("tools/call: 'name' must be a nonempty string".into());
            }
            if get("arguments").is_some_and(|value| !value.is_object()) {
                return Err("tools/call: 'arguments' must be an object".into());
            }
        }
        "notifications/cancelled" => {
            if req.id.is_some() {
                return Err("notifications/cancelled must not carry a request id".into());
            }
            let valid = match get("requestId") {
                Some(Value::String(_)) => true,
                Some(Value::Number(number)) => number.is_i64() || number.is_u64(),
                _ => false,
            };
            if !valid {
                return Err(
                    "notifications/cancelled: 'requestId' must be a string or lossless integer"
                        .into(),
                );
            }
            if get("reason").is_some_and(|value| !value.is_string()) {
                return Err("notifications/cancelled: 'reason' must be a string".into());
            }
        }
        _ => {}
    }
    Ok(())
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
            json!(i64::MIN),
            json!(u64::MAX),
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
    #[test]
    fn numeric_id_bounds_are_checked_before_rounding_can_escape() {
        for token in [
            "1234567890123456789012345678901234567890",
            "18446744073709551616",
            "-9223372036854775809",
            "1.5",
            "1e2",
            "1.0",
        ] {
            let raw = format!(r#"{{"jsonrpc":"2.0","id":{token},"method":"ping"}}"#);
            let error = validate_request(serde_json::from_str(&raw).unwrap()).unwrap_err();
            assert_eq!(error.response_id, Value::Null, "{token}");
        }
    }

    #[test]
    fn tool_calls_require_ids_but_null_remains_a_correlated_request() {
        assert!(
            validate_request(
                json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"set_extension"}})
            )
            .is_err()
        );
        assert!(validate_request(json!({"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"brain_status"}})).is_ok());
        assert!(
            validate_request(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_ok()
        );
    }

    #[test]
    fn method_params_reject_wrong_shapes_and_keep_client_metadata() {
        for (method, params) in [
            ("initialize", json!([])),
            ("initialize", json!({})),
            ("ping", json!([])),
            ("ping", json!({"unexpected":true})),
            ("tools/list", json!({"cursor":7})),
            ("tools/call", json!({"name":"brain_status","arguments":[]})),
        ] {
            let req =
                validate_request(json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
                    .unwrap();
            assert!(validate_method_params(&req).is_err(), "{method}");
        }
        for (method, params) in [
            (
                "initialize",
                json!({"protocolVersion":"2024-11-05","capabilities":{"experimental":{}},"clientInfo":{"name":"client","version":"1","title":"Client"},"_meta":{}}),
            ),
            ("ping", Value::Null),
            ("ping", json!({"_meta":{}})),
            ("tools/list", json!({"cursor":"next","_meta":{}})),
            (
                "tools/call",
                json!({"name":"brain_status","arguments":{},"_meta":{"progressToken":1}}),
            ),
        ] {
            let req =
                validate_request(json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
                    .unwrap();
            validate_method_params(&req).unwrap();
        }
    }
}

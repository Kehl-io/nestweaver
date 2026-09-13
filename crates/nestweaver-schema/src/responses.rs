//! Shared additive response contracts at transport boundaries.
use serde_json::{Value, json};

/// Keep published CLI (`nodes`) and MCP (`impact_nodes`) spellings as aliases
/// of one population while clients migrate. `symbol` is the caller's query;
/// `target` is the resolved UID, or null when resolution did not succeed.
pub fn impact(mut value: Value) -> Value {
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    if object.get("status").and_then(Value::as_str) == Some("not_found") {
        object.insert("error".into(), json!("not found"));
        object.insert(
            "name".into(),
            object.get("symbol").cloned().unwrap_or(Value::Null),
        );
    }
    let nodes = object
        .get("impact_nodes")
        .or_else(|| object.get("nodes"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let count = nodes.as_array().map_or(0, Vec::len);
    object.insert("nodes".into(), nodes.clone());
    object.insert("impact_nodes".into(), nodes);
    object.entry("target").or_insert(Value::Null);
    object.entry("note").or_insert(Value::Null);
    object.entry("total").or_insert(json!(count));
    object.entry("returned").or_insert(json!(count));
    for flag in [
        "truncated",
        "truncated_by_threshold",
        "truncated_by_depth",
        "truncated_by_limit",
    ] {
        object.entry(flag).or_insert(json!(false));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_and_mcp_aliases_normalize_to_one_contract() {
        let cli = impact(
            json!({"status":"ok","symbol":"query","target":"sym:x","nodes":[{"uid":"sym:y"}]}),
        );
        let mcp = impact(
            json!({"status":"ok","symbol":"query","target":"sym:x","impact_nodes":[{"uid":"sym:y"}]}),
        );
        assert_eq!(cli, mcp);
        assert!(cli["note"].is_null());
        let empty = impact(json!({"status":"not_found","symbol":"query"}));
        assert_eq!(empty["returned"], 0);
        assert!(empty["target"].is_null());
        for key in cli.as_object().unwrap().keys() {
            assert!(empty.get(key).is_some(), "missing {key}");
        }
    }
}

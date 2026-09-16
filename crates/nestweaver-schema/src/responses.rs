//! Shared additive response contracts at transport boundaries.
use serde_json::{Value, json};

/// Shared ambiguity guidance for CLI and MCP impact responses.
pub fn impact_ambiguity_remedy(repo_filter: Option<&str>) -> String {
    match repo_filter {
        Some(repo) => format!(
            "the symbol name matched multiple symbols; no impact was computed. \
             --repo {repo} is already set and every match is inside it, so it \
             cannot separate them. Pass a full UID instead — each candidate \
             below carries one."
        ),
        None => "the symbol name matched multiple symbols; no impact was computed. \
                 Disambiguate with --repo <name> or pass a full UID"
            .to_string(),
    }
}

/// Render ambiguity candidates consistently regardless of whether the caller
/// supplied full Symbol records or an already reduced transport representation.
pub fn impact_ambiguous(symbol: &str, repo_filter: Option<&str>, candidates: Value) -> Value {
    let mut candidates: Vec<Value> = candidates
        .as_array()
        .into_iter()
        .flatten()
        .map(|candidate| {
            json!({
                "uid": candidate.get("uid").cloned().unwrap_or(Value::Null),
                "name": candidate.get("name").cloned().unwrap_or(Value::Null),
                "file_path": candidate.get("file_path").cloned().unwrap_or(Value::Null),
                "start_line": candidate.get("start_line").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    candidates.sort_by(|a, b| {
        let key = |value: &Value| {
            (
                value["file_path"].as_str().unwrap_or_default().to_owned(),
                value["start_line"].as_u64().unwrap_or_default(),
                value["uid"].as_str().unwrap_or_default().to_owned(),
            )
        };
        key(a).cmp(&key(b))
    });
    impact(json!({"status": "ambiguous", "symbol": symbol,
        "candidates": candidates, "note": impact_ambiguity_remedy(repo_filter)}))
}

/// Attach bounded "did you mean" candidate names (nw-481) to a response
/// object, or leave it untouched when there are none.
///
/// `candidates` comes from the shared engine builder
/// (`nestweaver_engine::did_you_mean::did_you_mean_candidates`), which does
/// the actual substring symbol-name search this crate cannot perform itself
/// (zero internal deps — no store access). This half only shapes the JSON,
/// so the CLI (`impact_json_not_found` and its text twin) and MCP
/// (`tool_brain_impact`) cannot pick different insert timing or a different
/// empty-vs-absent convention: an empty `candidates` slice — a genuine miss,
/// never a fabricated placeholder — leaves `did_you_mean` OUT of the object
/// entirely, matching this codebase's `skip_serializing_if`-style honesty
/// convention (e.g. nw-477's `body_complete`).
///
/// Call this BEFORE [`impact`] / [`impact_ambiguous`] so the field is already
/// present on the object those normalize; both leave unrecognized keys
/// untouched.
pub fn with_did_you_mean(mut value: Value, candidates: &[String]) -> Value {
    if candidates.is_empty() {
        return value;
    }
    if let Some(object) = value.as_object_mut() {
        object.insert("did_you_mean".into(), json!(candidates));
    }
    value
}

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
    if object.get("status").and_then(Value::as_str) == Some("ambiguous") {
        object
            .entry("note")
            .or_insert_with(|| json!(impact_ambiguity_remedy(None)));
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
    fn with_did_you_mean_adds_the_field_only_when_candidates_exist() {
        let base = json!({"status": "not_found", "symbol": "project_context"});
        let with = with_did_you_mean(base.clone(), &["tool_project_context".to_string()]);
        assert_eq!(with["did_you_mean"], json!(["tool_project_context"]));

        // Counterweight: a genuine miss (empty candidates) must not add a
        // fabricated or always-present placeholder key.
        let without = with_did_you_mean(base.clone(), &[]);
        assert!(without.get("did_you_mean").is_none());
        assert_eq!(without, base);
    }

    #[test]
    fn with_did_you_mean_survives_the_impact_envelope_normalizer() {
        let value = with_did_you_mean(
            json!({"status": "not_found", "symbol": "project_context"}),
            &["tool_project_context".to_string()],
        );
        let normalized = impact(value);
        assert_eq!(normalized["did_you_mean"], json!(["tool_project_context"]));
        assert_eq!(normalized["status"], json!("not_found"));
    }

    #[test]
    fn full_and_minimal_ambiguity_candidates_have_one_contract() {
        let full = json!([{"uid":"sym:one","name":"same","file_path":"a.rs","start_line":2,
            "end_line":8,"repo_uid":"repo:one","kind":"Function","signature":"fn same()"}]);
        let minimal = json!([{"uid":"sym:one","name":"same","file_path":"a.rs","start_line":2}]);
        let second = json!({"uid":"sym:two","name":"same","file_path":"b.rs","start_line":1});
        let full = json!([second, full[0].clone()]);
        let minimal = json!([minimal[0].clone(), second]);
        for repo in [None, Some("repo:one")] {
            assert_eq!(
                impact_ambiguous("same", repo, full.clone()),
                impact_ambiguous("same", repo, minimal.clone())
            );
        }
    }

    #[test]
    fn ambiguous_routes_share_guidance_and_preserve_scoped_remedies() {
        let unscoped = impact(json!({"status":"ambiguous", "symbol":"same", "candidates":[]}));
        assert_eq!(unscoped["note"], impact_ambiguity_remedy(None));
        let scoped = impact(
            json!({"status":"ambiguous", "symbol":"same", "candidates":[],
            "note":impact_ambiguity_remedy(Some("repo"))}),
        );
        assert_eq!(scoped["note"], impact_ambiguity_remedy(Some("repo")));
        for status in ["ok", "not_found", "ambiguous"] {
            let response = impact(json!({"status":status,"symbol":"same"}));
            for key in [
                "note",
                "target",
                "nodes",
                "impact_nodes",
                "total",
                "returned",
                "truncated_by_limit",
            ] {
                assert!(response.get(key).is_some());
            }
        }
    }

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

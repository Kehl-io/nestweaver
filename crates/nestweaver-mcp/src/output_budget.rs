//! MCP presentation bounds. Graph computation has independent work limits.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, Write};

pub const LOGICAL_BYTES: usize = 20_000;
pub const RESULT_BYTES: usize = 40_000;
pub const CATALOGUE_BYTES: usize = 32_000;
pub const CATALOGUE_TOOLS: usize = 8;

#[derive(Default)]
struct Size(usize);
impl Write for Size {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        self.0 = self.0.saturating_add(
            bytes
                .windows(3)
                .filter(|b| *b == [0xe2, 0x80, 0xa8] || *b == [0xe2, 0x80, 0xa9])
                .count()
                * 3,
        );
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Includes the line separator escaping performed by stdio framing.
pub fn escaped_size(value: &Value) -> usize {
    let mut size = Size::default();
    serde_json::to_writer(&mut size, value).expect("JSON Value serializes");
    size.0
}

#[derive(Clone)]
enum Step {
    Key(String),
    Index(usize),
}
struct Cut {
    path: Vec<Step>,
    size: usize,
    array: bool,
}
fn identity(key: &str) -> bool {
    key == "uid"
        || key.ends_with("_uid")
        || key.ends_with("_uids")
        || key == "deduped_ref"
        || key == "status"
        || key == "scope"
        || key == "edge_type"
        || key == "verdict"
        || key == "reason"
        || key == "error"
        || key == "symbol"
        || key == "title"
        || key == "project"
        || key == "target"
        || key == "cluster_id"
        || key == "refused"
}
fn largest(value: &Value, path: &mut Vec<Step>, key: &str, best: &mut Option<Cut>) {
    if key == "output_budget" {
        return;
    }
    if key == "sources"
        && path
            .iter()
            .any(|step| matches!(step,Step::Key(key) if key == "_meta"))
        && escaped_size(value) <= 1024
    {
        return;
    }
    let candidate = match value {
        Value::Array(items) if items.len() > 1 => Some((escaped_size(value), true)),
        Value::String(text) if text.len() > 128 && !identity(key) => Some((text.len(), false)),
        _ => None,
    };
    if let Some((size, array)) = candidate {
        if best.as_ref().is_none_or(|old| size > old.size) {
            *best = Some(Cut {
                path: path.clone(),
                size,
                array,
            });
        }
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                path.push(Step::Key(key.clone()));
                largest(child, path, key, best);
                path.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                path.push(Step::Index(index));
                largest(child, path, key, best);
                path.pop();
            }
        }
        _ => {}
    }
}
fn cut_label(path: &[Step]) -> String {
    let mut label = String::new();
    for step in path {
        label.push('/');
        match step {
            Step::Key(key) => label.push_str(key),
            Step::Index(i) => label.push_str(&i.to_string()),
        }
    }
    if label.len() > 128 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        label.hash(&mut hash);
        label = format!(
            "{}#{:016x}",
            crate::tools::truncate_utf8_bytes(&label, 108),
            hash.finish()
        );
    }
    label
}

fn shrink(value: &mut Value, omitted: &mut BTreeMap<String, Value>) -> bool {
    let mut best = None;
    largest(value, &mut Vec::new(), "", &mut best);
    let Some(cut) = best else {
        return false;
    };
    let mirror_path = if !cut.array {
        let mut parent = &*value;
        let mut mirror = None;
        for (index, step) in cut.path.iter().enumerate() {
            if let Step::Key(key) = step {
                let sibling = match key.as_str() {
                    "nodes" => Some("impact_nodes"),
                    "impact_nodes" => Some("nodes"),
                    _ => None,
                };
                if let Some(sibling) = sibling {
                    if parent[key].is_array() && parent[key] == parent[sibling] {
                        let mut path = cut.path.clone();
                        path[index] = Step::Key(sibling.into());
                        mirror = Some(path);
                        break;
                    }
                }
            }
            parent = match step {
                Step::Key(key) => &parent[key],
                Step::Index(i) => &parent[*i],
            };
        }
        mirror
    } else {
        None
    };
    let mut selected = &mut *value;
    for step in &cut.path {
        selected = match step {
            Step::Key(key) => &mut selected[key],
            Step::Index(i) => &mut selected[*i],
        };
    }
    let label = cut_label(&cut.path);
    if cut.array {
        let items = selected.as_array_mut().unwrap();
        let keep = items.len().div_ceil(2);
        let count = items.len() - keep;
        items.truncate(keep);
        // Recognized shared contracts have paired identity/alias arrays and
        // emitted counters. Keep their prefixes together after late cuts.
        let mut paired = None;
        if let Some(Step::Key(key)) = cut.path.last() {
            let mut parent = &mut *value;
            for step in &cut.path[..cut.path.len() - 1] {
                parent = match step {
                    Step::Key(key) => &mut parent[key],
                    Step::Index(i) => &mut parent[*i],
                };
            }
            let sibling = match key.as_str() {
                "candidates" => Some("candidate_uids"),
                "candidate_uids" => Some("candidates"),
                "nodes" => Some("impact_nodes"),
                "impact_nodes" => Some("nodes"),
                _ => None,
            };
            if let Some(sibling) = sibling {
                if let Some(array) = parent.get_mut(sibling).and_then(Value::as_array_mut) {
                    if array.len() == keep + count {
                        array.truncate(keep);
                        paired = Some(sibling);
                    }
                }
            }
            let counters: &[&str] = match key.as_str() {
                "candidates" | "candidate_uids" => &["candidates_returned", "candidate_returned"],
                "members" => &["returned_members"],
                "impact_nodes" | "nodes" | "clusters" | "tags" | "results" => &["returned"],
                "affected_symbols" => &["returned_affected_symbol_count"],
                _ => &[],
            };
            let cursor = match key.as_str() {
                "members" => Some(("member_offset", "next_member_offset")),
                "clusters" => Some(("cluster_offset", "next_cluster_offset")),
                _ => None,
            };
            if let Some((offset, next)) = cursor {
                if parent.get(next).is_some_and(|v| !v.is_null()) {
                    parent[next] = json!(
                        parent[offset]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_add(keep as u64)
                    );
                }
            }
            for counter in counters {
                if parent.get(counter).is_some() {
                    parent[*counter] = json!(keep);
                }
            }
        }
        if omitted.len() < 16 || omitted.contains_key(&label) {
            let previous = omitted
                .get(&label)
                .and_then(|v| v["items_omitted"].as_u64())
                .unwrap_or(0);
            omitted.insert(label, json!({"items_omitted":previous + count as u64,"count_kind":"exact_completed_array","paired_array":paired}));
        }
    } else {
        let text = selected.as_str().unwrap();
        let previous_len = text.len();
        let clipped = crate::tools::truncate_utf8_bytes(text, (text.len() / 2).max(64));
        *selected = json!(clipped);
        let clipped_len = selected.as_str().unwrap().len();
        let mirror_label = mirror_path.as_ref().map(|path| cut_label(path));
        if let Some(path) = mirror_path {
            let replacement = selected.clone();
            let mut mirror = &mut *value;
            for step in &path {
                mirror = match step {
                    Step::Key(key) => &mut mirror[key],
                    Step::Index(i) => &mut mirror[*i],
                };
            }
            *mirror = replacement;
        }
        if omitted.len() < 16 || omitted.contains_key(&label) {
            let previous = omitted
                .get(&label)
                .and_then(|v| v["bytes_omitted"].as_u64())
                .unwrap_or(0);
            omitted.insert(label, json!({"bytes_omitted":previous + (previous_len - clipped_len) as u64,"text_truncated":true,"mirrored_alias_path":mirror_label}));
        }
    }
    true
}
fn disclose(payload: &mut Value, omitted: &BTreeMap<String, Value>, prior_complete: bool) {
    if let Some(map) = payload.as_object_mut() {
        map.insert("truncated".into(), json!(true));
        map.insert("output_budget".into(), json!({"logical_bytes_limit":LOGICAL_BYTES,"escaped_result_bytes_limit":RESULT_BYTES,
            "cuts":omitted,"cuts_complete":prior_complete && omitted.len() < 16,
            "retry":"Narrow the query, reduce limit, or request a specific UID/section. Output cuts do not describe unvisited graph work."}));
    }
}
fn synchronize(result: &mut Value) {
    if let Some(payload) = result.get("structuredContent") {
        let text = serde_json::to_string(payload).expect("JSON Value serializes");
        result["content"] = json!([{"type":"text","text":text}]);
    }
}
/// Finalize after all transport-specific metadata is attached. Limits apply to
/// the tool result only; request IDs and batch policy are independent.
pub fn finalize(mut result: Value) -> Value {
    let mut omitted: BTreeMap<String, Value> = result["structuredContent"]["output_budget"]["cuts"]
        .as_object()
        .map(|cuts| {
            cuts.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let prior_complete = result["structuredContent"]["output_budget"]["cuts_complete"]
        .as_bool()
        .unwrap_or(true);
    if result.get("structuredContent").is_some() {
        if !result["structuredContent"].is_object()
            && escaped_size(&result["structuredContent"]) > LOGICAL_BYTES
        {
            return refusal(result);
        }
        for _ in 0..512 {
            let logical = escaped_size(&result["structuredContent"]);
            if logical <= LOGICAL_BYTES {
                synchronize(&mut result);
                if escaped_size(&result) <= RESULT_BYTES {
                    return result;
                }
            }
            let reduced = shrink(&mut result["structuredContent"], &mut omitted)
                || result
                    .get_mut("_meta")
                    .is_some_and(|meta| shrink(meta, &mut omitted));
            if !reduced {
                return refusal(result);
            }
            disclose(&mut result["structuredContent"], &omitted, prior_complete);
        }
        refusal(result)
    } else {
        // Preserve generic plain-text error compatibility.
        if escaped_size(&result) > RESULT_BYTES {
            let original = result["content"][0]["text"].as_str().unwrap_or("");
            let original_len = original.len();
            let text = crate::tools::truncate_utf8_bytes(original, 4_000);
            result["content"] = json!([{"type":"text","text":format!("{text}\n[Error text truncated from {original_len} bytes; narrow the request and retry.]")}]);
        }
        result
    }
}
fn refusal(result: Value) -> Value {
    use std::hash::{Hash, Hasher};
    let original = &result["structuredContent"];
    let mut targets = serde_json::Map::new();
    for key in [
        "uid",
        "root_uid",
        "symbol",
        "title",
        "project",
        "project_uid",
        "cluster_id",
        "target",
    ] {
        if let Some(value) = original.get(key) {
            if escaped_size(value) <= 512 {
                targets.insert(key.into(), value.clone());
            } else {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                value.to_string().hash(&mut hash);
                targets.insert(key.into(), json!({"identity_truncated":true,"serialized_bytes":escaped_size(value),"hash":format!("{:016x}",hash.finish())}));
            }
        }
    }
    fn metadata(value: &Value, budget: &mut usize, depth: usize) -> Value {
        if depth > 6 || *budget < 8 {
            return Value::Null;
        }
        if escaped_size(value) <= (*budget).min(1024) {
            *budget = budget.saturating_sub(escaped_size(value));
            return value.clone();
        }
        match value {
            Value::Object(map) => {
                let mut bounded = serde_json::Map::new();
                for (key, value) in map {
                    if *budget < 32 || key.len() > 128 {
                        continue;
                    }
                    *budget = budget.saturating_sub(key.len() + 4);
                    bounded.insert(key.clone(), metadata(value, budget, depth + 1));
                }
                Value::Object(bounded)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .take(8)
                    .map(|item| metadata(item, budget, depth + 1))
                    .collect(),
            ),
            Value::String(text) => {
                let text = crate::tools::truncate_utf8_bytes(text, (*budget / 6).min(128));
                *budget = budget.saturating_sub(escaped_size(&json!(text)));
                json!(text)
            }
            _ => {
                *budget = budget.saturating_sub(8);
                value.clone()
            }
        }
    }
    let mut payload = json!({"status":"refused","error":"output_budget_exceeded",
        "original_status":original.get("status").and_then(Value::as_str).map(|s| crate::tools::truncate_utf8_bytes(s,128)),
        "targets":targets,"truncated":true,
        "message":"Required identity or metadata cannot fit the MCP output budget. Narrow the query or request a specific UID/section; this is not an empty complete answer."});
    payload["refused"] = json!(true);
    let mut assessment = serde_json::Map::new();
    let mut metadata_omissions = Vec::new();
    let mut budget = 3000usize;
    for key in [
        "_meta",
        "coverage",
        "verdict",
        "refused",
        "reason",
        "deadline_exceeded",
        "counts_complete",
        "traversal_truncated",
    ] {
        if let Some(value) = original.get(key) {
            let bounded = metadata(value, &mut budget, 0);
            if bounded != *value {
                metadata_omissions.push(json!({"path":key,"original_serialized_bytes":escaped_size(value),"metadata_truncated":true}));
            }
            if key == "_meta" || key == "coverage" {
                payload[key] = bounded.clone();
            }
            if key != "_meta" {
                assessment.insert(key.to_owned(), bounded);
            }
        }
    }
    // Provenance identifiers get their own small reservation even if the rest
    // of the metadata consumed its allowance.
    for key in ["scope", "sources"] {
        if let Some(value) = original.get("_meta").and_then(|meta| meta.get(key)) {
            let bounded = metadata(value, &mut 512usize, 0);
            if bounded != *value {
                metadata_omissions
                    .push(json!({"path":format!("_meta/{key}"),"metadata_truncated":true}));
            }
            if !payload["_meta"].is_object() {
                payload["_meta"] = json!({});
            }
            payload["_meta"][key] = bounded;
        }
    }
    if !assessment.is_empty() {
        payload["original_assessment"] = Value::Object(assessment);
    }
    if !metadata_omissions.is_empty() {
        payload["metadata_omissions"] = json!(metadata_omissions);
    }
    let response = json!({"content":[{"type":"text","text":payload.to_string()}],"structuredContent":payload,"isError":true});
    debug_assert!(escaped_size(&response) <= RESULT_BYTES);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn differing_singleton_aliases_are_not_overwritten_by_string_cuts() {
        let result = finalize(
            json!({"content":[],"structuredContent":{"status":"ok","nodes":[{"uid":"one","kind":"original-a","body":"x".repeat(120_000)}],"impact_nodes":[{"uid":"one","kind":"original-b","body":"y".repeat(120_000)}],"total":1,"returned":1},"isError":false}),
        );
        let payload = &result["structuredContent"];
        assert_eq!(payload["nodes"][0]["kind"], "original-a");
        assert_eq!(payload["impact_nodes"][0]["kind"], "original-b");
        assert_ne!(payload["nodes"], payload["impact_nodes"]);
        assert_eq!(payload["returned"], 1);
        assert_eq!(payload["status"], "ok");
        assert!(escaped_size(&result) <= RESULT_BYTES);
    }

    #[test]
    fn singleton_alias_string_cuts_preserve_equivalence_and_counts() {
        let row = json!({"uid":"symbol:one","file_path":"\"\\\u{2028}\u{2029}".repeat(4000),"body":"\"\\\u{2028}\u{2029}".repeat(6000)});
        let result = finalize(
            json!({"content":[],"structuredContent":{"status":"ok","nodes":[row],"impact_nodes":[row],"total":1,"returned":1},"isError":false}),
        );
        let payload = &result["structuredContent"];
        assert_eq!(
            payload["nodes"], payload["impact_nodes"],
            "originally equivalent singleton aliases must remain equivalent"
        );
        assert_eq!(payload["returned"], 1);
        assert_eq!(payload["total"], 1);
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["truncated"], true);
        assert_eq!(
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
            *payload
        );
        assert!(escaped_size(&result) <= RESULT_BYTES);
        assert!(
            payload["output_budget"]["cuts"]
                .as_object()
                .unwrap()
                .values()
                .any(|cut| cut["text_truncated"] == true)
        );
    }

    #[test]
    fn late_ambiguity_cuts_keep_uid_pairs_and_emitted_counts() {
        let candidates:Vec<_>=(0..50).map(|i|json!({"uid":format!("sym:{i}"),"name":"n".repeat(500),"file_path":"p".repeat(500)})).collect();
        let uids: Vec<_> = candidates.iter().map(|c| c["uid"].clone()).collect();
        let result = finalize(
            json!({"content":[],"structuredContent":{"status":"ambiguous","candidates":candidates,"candidate_uids":uids,"candidate_total":250,"candidates_returned":50},"isError":false}),
        );
        let payload = &result["structuredContent"];
        let candidates = payload["candidates"].as_array().unwrap();
        assert_eq!(payload["candidates_returned"], candidates.len());
        assert_eq!(payload["candidate_total"], 250);
        assert_eq!(
            payload["candidate_uids"],
            json!(
                candidates
                    .iter()
                    .map(|c| c["uid"].clone())
                    .collect::<Vec<_>>()
            )
        );
        assert!(escaped_size(&result) <= RESULT_BYTES);
    }

    #[test]
    fn late_impact_alias_cuts_keep_both_arrays_and_returned() {
        let nodes: Vec<_> = (0..200)
            .map(|i| json!({"uid":format!("sym:{i}"),"name":"n".repeat(300)}))
            .collect();
        let result = finalize(
            json!({"content":[],"structuredContent":{"status":"ok","nodes":nodes,"impact_nodes":nodes,"total":200,"returned":200},"isError":false}),
        );
        let payload = &result["structuredContent"];
        assert_eq!(payload["nodes"], payload["impact_nodes"]);
        assert_eq!(
            payload["returned"],
            payload["nodes"].as_array().unwrap().len()
        );
        assert_eq!(payload["total"], 200);
        assert!(escaped_size(&result) <= RESULT_BYTES);
    }

    #[test]
    fn second_finalization_preserves_cumulative_cut_counts() {
        let first = crate::tools::wrap_tool_result(
            json!({"uid":"sym:a","body":"x".repeat(120_000),"_meta":{"scope":"local","sources":["local"]}}),
        );
        let previous =
            first["structuredContent"]["output_budget"]["cuts"]["/body"]["bytes_omitted"]
                .as_u64()
                .unwrap();
        let mut with_metadata = first;
        with_metadata["_meta"] = json!({"transport_note":"y".repeat(90_000)});
        let second = finalize(with_metadata);
        assert!(escaped_size(&second) <= RESULT_BYTES);
        assert!(
            second["structuredContent"]["output_budget"]["cuts"]["/body"]["bytes_omitted"]
                .as_u64()
                .unwrap()
                >= previous
        );
        assert_eq!(
            serde_json::from_str::<Value>(second["content"][0]["text"].as_str().unwrap()).unwrap(),
            second["structuredContent"]
        );
    }
    #[test]
    fn oversized_non_object_is_an_explicit_refusal() {
        let result = crate::tools::wrap_tool_result(json!(vec!["row".repeat(1000); 100]));
        assert!(escaped_size(&result) <= RESULT_BYTES);
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["status"], "refused");
    }
    #[test]
    fn mandatory_identity_refusal_keeps_provenance_and_assessment() {
        let result = crate::tools::wrap_tool_result(
            json!({"status":"partial","uid":"sym:huge".repeat(20_000),
            "_meta":{"scope":"hybrid","sources":["local","server"]},"coverage":{"traversal_truncated":true,"visible_nodes":12},
            "verdict":"review","refused":false,"reason":"degraded_coverage"}),
        );
        let payload = &result["structuredContent"];
        assert!(escaped_size(&result) <= RESULT_BYTES);
        assert_eq!(payload["original_status"], "partial");
        assert_eq!(payload["_meta"]["sources"], json!(["local", "server"]));
        assert_eq!(payload["coverage"]["traversal_truncated"], true);
        assert_eq!(payload["coverage"]["visible_nodes"], 12);
        assert_eq!(payload["original_assessment"]["verdict"], "review");
        assert_eq!(
            payload["original_assessment"]["reason"],
            "degraded_coverage"
        );
        assert_eq!(payload["refused"], true);
        assert_eq!(payload["original_assessment"]["refused"], false);
    }
    #[test]
    fn huge_metadata_paths_do_not_share_omission_counters() {
        let prefix = "metadata_key".repeat(40);
        let payload = json!({format!("{prefix}a"):"x".repeat(40_000),format!("{prefix}b"):"y".repeat(40_000)});
        let result = crate::tools::wrap_tool_result(payload);
        let cuts = result["structuredContent"]["output_budget"]["cuts"]
            .as_object()
            .unwrap();
        assert_eq!(cuts.len(), 2);
        assert!(cuts.keys().all(|key| key.len() <= 128));
        assert!(escaped_size(&result) <= RESULT_BYTES);
    }
}

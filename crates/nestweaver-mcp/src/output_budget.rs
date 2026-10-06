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
    if let Some((size, array)) = candidate
        && best.as_ref().is_none_or(|old| size > old.size)
    {
        *best = Some(Cut {
            path: path.clone(),
            size,
            array,
        });
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

fn shrink(value: &mut Value, omitted: &mut BTreeMap<String, Value>) -> Result<bool, ()> {
    let mut best = None;
    largest(value, &mut Vec::new(), "", &mut best);
    let Some(cut) = best else {
        return Ok(false);
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
                if let Some(sibling) = sibling
                    && parent[key].is_array()
                    && parent[key] == parent[sibling]
                {
                    let mut path = cut.path.clone();
                    path[index] = Step::Key(sibling.into());
                    mirror = Some(path);
                    break;
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
    let mut shared_keep = None;
    if cut.array
        && let Some(Step::Key(key)) = cut.path.last()
        && matches!(key.as_str(), "broken_links" | "low_confidence")
    {
        let mut parent = &*value;
        for step in &cut.path[..cut.path.len() - 1] {
            parent = match step {
                Step::Key(key) => &parent[key],
                Step::Index(i) => &parent[*i],
            };
        }
        let sibling = if key == "broken_links" {
            "low_confidence"
        } else {
            "broken_links"
        };
        if let Some(other) = parent.get(sibling).and_then(Value::as_array) {
            let mut keep = parent[key].as_array().unwrap().len().div_ceil(2);
            let total_key = if sibling == "broken_links" {
                "total"
            } else {
                "low_confidence_total"
            };
            let offset = parent["offset"].as_u64().unwrap_or(0);
            let exhausted = offset.saturating_add(other.len() as u64)
                >= parent[total_key].as_u64().unwrap_or(u64::MAX);
            // A secondary-only cut cannot be safely advanced using the primary
            // returned counter. Refuse rather than publish a misleading window.
            if key == "low_confidence" && other.is_empty() {
                return Err(());
            }
            if other.len() < keep && !exhausted {
                keep = other.len();
            }
            if keep == 0 {
                return Err(());
            }
            shared_keep = Some(keep);
        }
    }
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
        let keep = shared_keep.unwrap_or_else(|| items.len().div_ceil(2));
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
                "broken_links" => Some("low_confidence"),
                "low_confidence" => Some("broken_links"),
                _ => None,
            };
            if let Some(sibling) = sibling {
                let broken_pair = matches!(key.as_str(), "broken_links" | "low_confidence");
                let total_key = if sibling == "broken_links" {
                    "total"
                } else {
                    "low_confidence_total"
                };
                let other_len = parent
                    .get(sibling)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let exhausted_smaller = broken_pair
                    && other_len <= keep
                    && parent["offset"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_add(other_len as u64)
                        >= parent[total_key].as_u64().unwrap_or(u64::MAX);
                if let Some(array) = parent.get_mut(sibling).and_then(Value::as_array_mut)
                    && !exhausted_smaller
                    && (array.len() == keep + count || (broken_pair && array.len() > keep))
                {
                    let paired_count = array.len() - keep;
                    array.truncate(keep);
                    paired = Some(sibling);
                    if broken_pair {
                        let mut path = cut.path.clone();
                        *path.last_mut().unwrap() = Step::Key(sibling.into());
                        let label = cut_label(&path);
                        let prior = omitted
                            .get(&label)
                            .and_then(|v| v["items_omitted"].as_u64())
                            .unwrap_or(0);
                        if omitted.len() < 16 || omitted.contains_key(&label) {
                            omitted.insert(label, json!({"items_omitted":prior+paired_count as u64,"count_kind":"exact_completed_array","paired_array":key}));
                        }
                    }
                }
            }
            let counters: &[&str] = match key.as_str() {
                "candidates" | "candidate_uids" => &["candidates_returned", "candidate_returned"],
                "members" => &["returned_members"],
                "impact_nodes" | "nodes" | "clusters" | "tags" | "results" => &["returned"],
                "affected_symbols" => &["returned_affected_symbol_count"],
                "unreachable_symbols" | "broken_links" => &["returned"],
                "backlinks" => &["count"],
                "summaries" | "hubs" | "bridges" => &["returned", "count"],
                _ => &[],
            };
            let cursor = match key.as_str() {
                "members" => Some(("member_offset", "next_member_offset")),
                "clusters" => Some(("cluster_offset", "next_cluster_offset")),
                "unreachable_symbols" => Some(("offset", "next_offset")),
                _ => None,
            };
            if let Some((offset, next)) = cursor
                && parent.get(next).is_some()
                && parent.get(offset).is_some()
            {
                parent[next] = json!(
                    parent[offset]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_add(keep as u64)
                );
            }
            if key == "unreachable_symbols" && parent.get("has_more").is_some() {
                parent["has_more"] = json!(true);
            }
            if key == "members" {
                parent["members_truncated"] = json!(true);
                if let Some(size) = parent["size"].as_u64() {
                    parent["members_omitted"] = json!(size.saturating_sub(keep as u64));
                }
                parent["retry_guidance"] = json!(
                    "Query this cluster_id with next_member_offset, identical repos/resolution, expected_generation and page_token."
                );
            }
            if matches!(key.as_str(), "broken_links" | "low_confidence") {
                if key == "low_confidence" || paired == Some("low_confidence") {
                    parent["low_confidence_truncated"] = json!(true);
                }
                if key == "broken_links" || paired == Some("broken_links") {
                    parent["truncated"] = json!(true);
                }
                if parent.get("broken_links").is_some() && parent.get("returned").is_some() {
                    parent["returned"] = json!(parent["broken_links"].as_array().unwrap().len());
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
    Ok(true)
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
            let reduced = match shrink(&mut result["structuredContent"], &mut omitted) {
                Err(()) => return refusal(result),
                Ok(true) => true,
                Ok(false) => match result.get_mut("_meta") {
                    Some(meta) => match shrink(meta, &mut omitted) {
                        Ok(reduced) => reduced,
                        Err(()) => return refusal(result),
                    },
                    None => false,
                },
            };
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
        "total",
        "returned",
        "low_confidence_total",
        "offset",
        "unresolved",
        "ambiguous",
        "graph_generation",
        "page_token",
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
    fn quality_wrapped(payload: Value) -> Value {
        let result = crate::tools::wrap_tool_result(payload);
        assert!(escaped_size(&result) <= RESULT_BYTES);
        assert_eq!(
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
            result["structuredContent"]
        );
        result["structuredContent"].clone()
    }

    #[test]
    fn quality_budget_dead_code_late_cut_preserves_continuation() {
        let rows: Vec<_> = (0..50).map(|index| json!({"uid":format!("sym:{index}"),"name":"\\".repeat(600),"file_path":"src/a.rs","confidence":"low"})).collect();
        for matching in [50, 100] {
            let payload = quality_wrapped(
                json!({"unreachable_symbols":rows,"returned":50,"matching_count":matching,"unreachable_count":100,"offset":0,"next_offset":if matching>50 {Some(50)} else {None},"has_more":matching>50,"truncated":matching>50,"graph_generation":17,"page_token":"nw-dead-code-original","review_only":true,"coverage":"complete"}),
            );
            let retained = payload["unreachable_symbols"].as_array().unwrap().len();
            assert!(
                retained > 0 && retained < 50,
                "actual array late cut required"
            );
            assert_eq!(payload["returned"], retained);
            assert_eq!(payload["has_more"], true);
            assert_eq!(payload["next_offset"], retained);
            assert_eq!(payload["matching_count"], matching);
            assert_eq!(payload["graph_generation"], 17);
            assert_eq!(payload["page_token"], "nw-dead-code-original");
            assert_eq!(payload["review_only"], true);
        }
    }

    #[test]
    fn quality_budget_completed_member_page_gets_continuation_after_late_cut() {
        let members: Vec<_> = (0..50).map(|index| json!({"uid":format!("sym:{index}"),"name":"\\".repeat(600),"file_path":"src/a.rs","kind":"Function"})).collect();
        let payload = quality_wrapped(
            json!({"clusters":[{"id":3,"name":"cluster","size":80,"members":members,"returned_members":50,"member_offset":30,"next_member_offset":null,"members_truncated":false,"members_omitted":30,"retry_guidance":null}],"total":1,"returned":1,"cluster_offset":0,"next_cluster_offset":null,"graph_generation":17,"page_token":"nw-clusters-original"}),
        );
        let row = &payload["clusters"][0];
        let retained = row["members"].as_array().unwrap().len();
        assert!(retained > 0 && retained < 50);
        assert_eq!(row["returned_members"], retained);
        assert_eq!(row["members_truncated"], true);
        assert_eq!(row["next_member_offset"], 30 + retained);
        assert_eq!(row["size"], 80);
        assert_eq!(row["members_omitted"], 80 - retained);
        assert!(
            row["retry_guidance"]
                .as_str()
                .unwrap()
                .contains("next_member_offset")
        );
        assert_eq!(payload["graph_generation"], 17);
        assert_eq!(payload["page_token"], "nw-clusters-original");
    }

    fn quality_count_case(key: &str, counters: &[&str]) {
        let rows: Vec<_> = (0..50)
            .map(|index| json!({"uid":format!("sym:{index}"),"name":"\\".repeat(600)}))
            .collect();
        let mut original = json!({"total":100,"truncated":false});
        for counter in counters {
            original[*counter] = json!(50);
        }
        if key == "summaries" {
            original["total_available"] = json!(100);
        }
        original[key] = json!(rows);
        let payload = quality_wrapped(original);
        let retained = payload[key].as_array().unwrap().len();
        assert!(retained < 50, "{key} must exercise array cut");
        for counter in counters {
            assert_eq!(payload[*counter], retained, "{key} {counter}");
        }
        assert_eq!(payload["total"], 100);
        if key == "summaries" {
            assert_eq!(payload["total_available"], 100);
        }
    }
    #[test]
    fn quality_budget_backlinks_count_follows_late_prefix() {
        quality_count_case("backlinks", &["count"]);
    }
    #[test]
    fn quality_budget_summary_count_aliases_follow_late_prefix() {
        quality_count_case("summaries", &["count", "returned"]);
    }
    #[test]
    fn quality_budget_broken_links_returned_follows_late_prefix() {
        quality_count_case("broken_links", &["returned"]);
    }
    #[test]
    fn quality_budget_hub_count_aliases_follow_late_prefix() {
        quality_count_case("hubs", &["count", "returned"]);
    }
    #[test]
    fn quality_budget_bridge_count_aliases_follow_late_prefix() {
        quality_count_case("bridges", &["count", "returned"]);
    }

    #[test]
    fn quality_budget_empty_primary_secondary_cut_has_retry_or_refusal() {
        let low: Vec<_> = (0..40).map(|index| json!({"wikilink_text":"\\".repeat(600),"source_note_uid":format!("note:{index}"),"confidence":0.8})).collect();
        let payload = quality_wrapped(
            json!({"offset":0,"returned":0,"total":0,"unresolved":0,"ambiguous":0,"truncated":false,"broken_links":[],"low_confidence":low,"low_confidence_total":40,"low_confidence_truncated":false}),
        );
        if payload["status"] == "refused" {
            assert_eq!(payload["error"], "output_budget_exceeded");
            assert_eq!(payload["original_assessment"]["total"], 0);
            assert_eq!(payload["original_assessment"]["low_confidence_total"], 40);
            assert_eq!(payload["original_assessment"]["offset"], 0);
            assert!(
                payload["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("Narrow"))
            );
        } else {
            assert_eq!(payload["returned"], 0);
            assert_eq!(payload["total"], 0);
            assert!(payload["low_confidence"].as_array().unwrap().len() < 40);
            assert_eq!(payload["low_confidence_truncated"], true);
            assert!(payload["output_budget"]["retry"].is_string());
        }
    }

    #[test]
    fn quality_budget_exhausted_primary_cannot_advance_past_cut_secondary() {
        let broken: Vec<_> = (0..30).map(|index| json!({"source_note_uid":format!("note:broken:{index}"),"wikilink_text":format!("missing{index}"),"confidence":0.0})).collect();
        let low: Vec<_> = (0..40).map(|index| json!({"source_note_uid":format!("note:low:{index}"),"wikilink_text":"\\".repeat(250),"confidence":0.8})).collect();
        let payload = quality_wrapped(
            json!({"offset":0,"broken_links":broken,"returned":30,"total":30,"unresolved":30,"ambiguous":0,"truncated":false,"low_confidence":low,"low_confidence_total":100,"low_confidence_truncated":false}),
        );
        let primary = payload["broken_links"].as_array().unwrap().len();
        let secondary = payload["low_confidence"].as_array().unwrap().len();
        assert!(
            secondary > 0 && secondary < 40,
            "actual secondary array cut required"
        );
        assert_eq!(payload["returned"], primary);
        assert!(
            primary <= secondary,
            "offset + returned would skip secondary rows {secondary}..{primary}"
        );
        assert_eq!(payload["total"], 30);
        assert_eq!(payload["low_confidence_total"], 100);
        assert_eq!(payload["low_confidence_truncated"], true);
        assert_eq!(payload["truncated"], true);
        assert_eq!(payload["offset"], 0);
        assert_eq!(
            payload["output_budget"]["cuts"]["/broken_links"]["items_omitted"],
            30 - primary
        );
    }

    #[test]
    fn quality_budget_exhausted_smaller_broken_population_keeps_legitimate_rows() {
        let broken: Vec<_> = (0..3).map(|index| json!({"source_note_uid":format!("note:{index}"),"wikilink_text":format!("missing{index}")})).collect();
        let low: Vec<_> = (0..40).map(|index| json!({"source_note_uid":format!("note:{index}"),"wikilink_text":"\\".repeat(600)})).collect();
        let payload = quality_wrapped(
            json!({"offset":7,"broken_links":broken,"returned":3,"total":10,"truncated":false,"low_confidence":low,"low_confidence_total":100,"low_confidence_truncated":false}),
        );
        assert_eq!(payload["broken_links"], json!(broken));
        assert_eq!(payload["returned"], 3);
        assert_eq!(payload["total"], 10);
        assert_eq!(payload["low_confidence_total"], 100);
        assert_eq!(payload["low_confidence_truncated"], true);
        assert_eq!(payload["offset"], 7);
    }

    #[test]
    fn quality_budget_dead_code_nonzero_offset_never_skips_late_rows() {
        let rows: Vec<_> = (10..60)
            .map(|index| json!({"uid":format!("sym:{index}"),"name":"\\".repeat(600)}))
            .collect();
        let payload = quality_wrapped(
            json!({"unreachable_symbols":rows,"returned":50,"matching_count":100,"offset":10,"next_offset":60,"has_more":true,"graph_generation":17,"page_token":"original-token"}),
        );
        let retained = payload["unreachable_symbols"].as_array().unwrap().len();
        assert!(retained < 50);
        assert_eq!(payload["next_offset"], 10 + retained);
        assert_eq!(payload["returned"], retained);
        assert_eq!(payload["page_token"], "original-token");
        assert_eq!(payload["graph_generation"], 17);
    }

    #[test]
    fn quality_budget_ordinary_completed_pages_and_empty_populations_stay_exact() {
        for payload in [
            json!({"unreachable_symbols":[{"uid":"sym:a"}],"returned":1,"matching_count":1,"offset":0,"next_offset":null,"has_more":false,"graph_generation":17,"page_token":"token"}),
            json!({"clusters":[{"id":0,"members":[{"uid":"sym:a"}],"returned_members":1,"size":1,"member_offset":0,"next_member_offset":null,"members_truncated":false}],"returned":1,"total":1,"cluster_offset":0,"next_cluster_offset":null,"graph_generation":17,"page_token":"token"}),
            json!({"broken_links":[],"low_confidence":[],"returned":0,"total":0,"low_confidence_total":0,"truncated":false,"low_confidence_truncated":false,"offset":0}),
        ] {
            assert_eq!(quality_wrapped(payload.clone()), payload);
        }
    }

    #[test]
    fn quality_budget_broken_link_shared_offset_keeps_compatible_prefixes() {
        let broken: Vec<_> = (0..40).map(|index| json!({"wikilink_text":format!("missing{index}"),"source_note_uid":format!("note:{index}"),"confidence":0.0})).collect();
        let low: Vec<_> = (0..40).map(|index| json!({"wikilink_text":"\\".repeat(600),"source_note_uid":format!("note:{index}"),"confidence":0.8})).collect();
        let payload = quality_wrapped(
            json!({"offset":5,"returned":40,"total":100,"unresolved":100,"ambiguous":0,"truncated":false,"broken_links":broken,"low_confidence":low,"low_confidence_total":100,"low_confidence_truncated":false}),
        );
        let broken = payload["broken_links"].as_array().unwrap();
        let low = payload["low_confidence"].as_array().unwrap();
        assert!(low.len() < 40);
        assert_eq!(
            broken.len(),
            low.len(),
            "one shared offset must not skip the longer emitted prefix"
        );
        assert_eq!(payload["returned"], broken.len());
        assert_eq!(payload["low_confidence_truncated"], true);
        assert_eq!(payload["truncated"], true);
        assert_eq!(payload["offset"], 5);
        assert_eq!(payload["total"], 100);
        assert_eq!(payload["low_confidence_total"], 100);
    }

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

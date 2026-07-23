//! Result-shaping helpers shared by every hybrid routing path: counting and
//! extracting result items, merging local + server responses (RRF + scope-hash
//! dedup, structured-schema preservation, fan-out concatenation), and
//! injecting `_meta` provenance / staleness annotations.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::merge::rrf_merge;

#[derive(Clone, Copy)]
struct SearchCountMetadata {
    lower_bound: Option<u64>,
    complete: bool,
}

fn search_count_metadata(value: &Value, result_count: usize) -> SearchCountMetadata {
    let total = value.get("total_matches").and_then(Value::as_u64);
    let returned = value.get("returned_matches").and_then(Value::as_u64);
    let relation = value.get("total_matches_relation").and_then(Value::as_str);
    let truncated = value.get("truncated").and_then(Value::as_bool);
    let actual = result_count as u64;

    let valid = matches!(
        (total, returned, relation, truncated),
        (Some(total), Some(returned), Some("eq"), Some(truncated))
            if returned == actual && total >= returned && (truncated || total == returned)
    ) || matches!(
        (total, returned, relation, truncated),
        (Some(total), Some(returned), Some("gte"), Some(true))
            if returned == actual && total >= returned
    );
    let complete = valid
        && matches!(
            (total, returned, relation, truncated),
            (Some(total), Some(returned), Some("eq"), Some(false))
                if returned == total
        );

    SearchCountMetadata {
        lower_bound: total.filter(|_| valid),
        complete,
    }
}

/// Return the canonical logical identity carried by a `brain_search` row.
///
/// Search UIDs are presentation-independent and domain-qualified: notes,
/// standalone tags, and symbols use distinct prefixes, while symbol UIDs also
/// encode repository ownership. Older concise responses can have an empty UID;
/// those rows are deliberately unkeyed and cannot prove union cardinality.
fn brain_search_logical_uid(value: &Value) -> Option<String> {
    let uid = value.get("uid").and_then(Value::as_str)?;
    let kind = value.get("kind").and_then(Value::as_str)?;
    let parts: Vec<&str> = uid.split(':').collect();
    let supported_domain = match parts.as_slice() {
        ["note" | "tag", "vlt", _, _, ..] => kind == "note",
        ["sym", "repo", _, _, ..] => kind.starts_with("Symbol/"),
        _ => false,
    };
    if !supported_domain {
        return None;
    }

    // UIDs include the indexing instance as their third component. The same
    // logical repo/vault indexed by a laptop and an org server must still
    // coalesce, so remove only that transport-local component and retain the
    // domain plus every ownership/content component after it.
    Some(
        parts[..2]
            .iter()
            .chain(parts[3..].iter())
            .copied()
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn has_proven_unique_search_identities(items: &[Value]) -> bool {
    let mut seen = HashSet::with_capacity(items.len());
    items
        .iter()
        .all(|item| brain_search_logical_uid(item).is_some_and(|uid| seen.insert(uid)))
}

struct RankedSearchResult {
    value: Value,
    score: f64,
    tiebreaker: String,
}

/// Brain-search-only weighted RRF keyed by canonical entity UID.
///
/// The generic merger intentionally keeps its symbol scope-hash identity for
/// every non-search tool. Search rows need a different contract because notes
/// may have no location and concise symbols omit presentation metadata.
fn merge_brain_search_items(
    local_items: Vec<Value>,
    server_items: Vec<Value>,
) -> (Vec<Value>, u64) {
    const RRF_K: f64 = 60.0;
    const LOCAL_WEIGHT: f64 = 1.5;
    const SERVER_WEIGHT: f64 = 1.0;

    let mut keyed: HashMap<String, RankedSearchResult> = HashMap::new();
    let mut unkeyed = Vec::new();

    for (source, items, weight) in [
        ("local", local_items, LOCAL_WEIGHT),
        ("server", server_items, SERVER_WEIGHT),
    ] {
        for (rank, value) in items.into_iter().enumerate() {
            let score = weight / (rank as f64 + RRF_K + 1.0);
            if let Some(uid) = brain_search_logical_uid(&value) {
                if let Some(existing) = keyed.get_mut(&uid) {
                    existing.score += score;
                } else {
                    keyed.insert(
                        uid.clone(),
                        RankedSearchResult {
                            value,
                            score,
                            tiebreaker: uid,
                        },
                    );
                }
            } else {
                unkeyed.push(RankedSearchResult {
                    tiebreaker: format!("\u{7f}{source}:{rank}:{value}"),
                    value,
                    score,
                });
            }
        }
    }

    let proven_identity_count = keyed.len() as u64;
    let mut ranked: Vec<RankedSearchResult> = keyed.into_values().chain(unkeyed).collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.tiebreaker.cmp(&b.tiebreaker))
    });
    (
        ranked.into_iter().map(|result| result.value).collect(),
        proven_identity_count,
    )
}

fn union_expansion_terms(local: &Value, server: &Value) -> Vec<String> {
    let mut terms = Vec::new();
    for value in [local, server] {
        if let Some(items) = value.get("expansion_terms").and_then(Value::as_array) {
            for term in items.iter().filter_map(Value::as_str) {
                if !terms.iter().any(|existing| existing == term) {
                    terms.push(term.to_string());
                }
            }
        }
    }
    terms
}

/// Count the number of result items in a JSON response.
///
/// Handles both `{ "results": [...] }` envelope and bare arrays.
pub fn count_results(value: &Value) -> usize {
    if let Some(arr) = value.as_array() {
        arr.len()
    } else if let Some(results) = value.get("results").and_then(|v| v.as_array()) {
        results.len()
    } else if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
        items.len()
    } else if let Some(connected) = value.get("connected").and_then(|v| v.as_array()) {
        // Structured responses (brain_context / project_context) carry their
        // payload in `connected` — count those so the fallback threshold is
        // meaningful instead of always treating the whole object as 1 result.
        connected.len()
    } else if value.is_object() {
        // A single object counts as 1 result.
        1
    } else {
        0
    }
}

/// Set (or replace) the `_meta.stale_repos` provenance on a response, creating
/// the `_meta` object if absent. No-op for non-object responses.
pub fn set_stale_repos(result: &mut Value, stale: &[String]) {
    if let Some(obj) = result.as_object_mut() {
        let meta = obj
            .entry("_meta")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(meta_obj) = meta.as_object_mut() {
            meta_obj.insert(
                "stale_repos".to_string(),
                serde_json::to_value(stale).unwrap_or(Value::Null),
            );
        }
    }
}

/// Extract the result items array from a JSON response.
pub fn extract_result_items(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        arr.clone()
    } else if let Some(results) = value.get("results").and_then(|v| v.as_array()) {
        results.clone()
    } else if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
        items.clone()
    } else if value.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        vec![value.clone()]
    } else {
        vec![]
    }
}

/// Merge two JSON responses by concatenating their result arrays and
/// deduplicating via scope-hash identity.
pub fn merge_json_results(local: &Value, server: &Value) -> Value {
    let local_items = extract_result_items(local);
    let server_items = extract_result_items(server);
    let is_brain_search =
        local.get("total_matches").is_some() || server.get("total_matches").is_some();
    let local_count = search_count_metadata(local, local_items.len());
    let server_count = search_count_metadata(server, server_items.len());
    let local_identities_complete = has_proven_unique_search_identities(&local_items);
    let server_identities_complete = has_proven_unique_search_identities(&server_items);

    let (values, proven_identity_count) = if is_brain_search {
        merge_brain_search_items(local_items, server_items)
    } else {
        let merged = rrf_merge(local_items, server_items);
        (merged.into_iter().map(|mr| mr.value).collect(), 0_u64)
    };
    let returned = values.len() as u64;

    let mut response = wrap_merged_response(values, &["local", "server"]);
    if is_brain_search {
        let complete = local_count.complete
            && server_count.complete
            && local_identities_complete
            && server_identities_complete;
        let (total, relation) = if complete {
            (proven_identity_count, "eq")
        } else {
            (
                local_count
                    .lower_bound
                    .unwrap_or(0)
                    .max(server_count.lower_bound.unwrap_or(0))
                    .max(proven_identity_count)
                    .max(u64::from(returned > 0)),
                "gte",
            )
        };
        response["query"] = local
            .get("query")
            .or_else(|| server.get("query"))
            .cloned()
            .unwrap_or(Value::String(String::new()));
        response["engine"] = Value::String("hybrid".to_string());
        response["total_matches"] = Value::from(total);
        response["total_matches_relation"] = Value::String(relation.to_string());
        response["returned_matches"] = Value::from(returned);
        response["truncated"] = Value::Bool(relation != "eq" || returned < total);
        let expansion_terms = union_expansion_terms(local, server);
        response["expansion_terms"] = serde_json::json!(expansion_terms);
    }

    response
}

/// Merge two FanOut tool responses (regex_search, count_patterns) by CONCATENATING their row
/// arrays instead of symbol-deduping. Any top-level array field present in `server` is appended
/// to the same field in `local`; scalar fields and provenance/meta (`_`-prefixed) keep the local
/// value. This preserves every aggregate row — the whole point of FanOut vs Merge.
pub fn concat_fanout(local: &Value, server: &Value) -> Value {
    // Boolean "not everything was returned" flags must be OR-ed across tiers — if
    // EITHER side truncated, the combined result is truncated. Keeping the local
    // value would report `truncated:false` while hiding a server-side cap.
    const COMPLETENESS_FLAGS: &[&str] = &["truncated", "capped", "limit_hit", "partial"];
    let mut out = local.clone();
    if let (Some(lo), Some(so)) = (out.as_object_mut(), server.as_object()) {
        for (k, sv) in so {
            if k.starts_with('_') {
                continue;
            }
            match (lo.get_mut(k), sv) {
                (Some(Value::Array(la)), Value::Array(sa)) => la.extend(sa.iter().cloned()),
                (Some(lv @ Value::Bool(_)), Value::Bool(true))
                    if COMPLETENESS_FLAGS.contains(&k.as_str()) =>
                {
                    *lv = Value::Bool(true);
                }
                (None, _) => {
                    lo.insert(k.clone(), sv.clone());
                }
                _ => {}
            }
        }
    }
    out
}

/// Merge two JSON responses, preserving structured schemas (e.g. brain_context's
/// `{ seeds, connected, unresolved_seeds, expansion_terms }`) when detected.
/// Falls back to flat `{ results: [...] }` envelope for non-structured responses.
pub fn merge_structured_results(local: &Value, server: &Value) -> Value {
    let local_connected = local.get("connected").and_then(|v| v.as_array());
    let server_connected = server.get("connected").and_then(|v| v.as_array());

    if let (Some(lc), Some(sc)) = (local_connected, server_connected) {
        let merged_connected = rrf_merge(lc.clone(), sc.clone());
        let merged_values: Vec<Value> = merged_connected
            .into_iter()
            .map(|mr| {
                let mut v = mr.value;
                if let Value::Object(ref mut map) = v {
                    map.insert(
                        "_provenance".to_string(),
                        serde_json::to_value(mr.provenance).unwrap_or(Value::Null),
                    );
                    map.insert(
                        "_confidence".to_string(),
                        serde_json::to_value(mr.confidence).unwrap_or(Value::Null),
                    );
                    map.insert("_rrf_score".to_string(), Value::from(mr.score));
                }
                v
            })
            .collect();

        let mut seeds = local
            .get("seeds")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if let Some(server_seeds) = server.get("seeds").and_then(|v| v.as_array()) {
            seeds.extend(server_seeds.iter().cloned());
        }

        let mut unresolved = local
            .get("unresolved_seeds")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if let Some(su) = server.get("unresolved_seeds").and_then(|v| v.as_array()) {
            unresolved.extend(su.iter().cloned());
        }

        let mut expansion = local
            .get("expansion_terms")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if let Some(se) = server.get("expansion_terms").and_then(|v| v.as_array()) {
            expansion.extend(se.iter().cloned());
        }

        let mut result = serde_json::json!({
            "seeds": seeds,
            "connected": merged_values,
        });
        if !unresolved.is_empty() {
            result["unresolved_seeds"] = Value::Array(unresolved);
        }
        if !expansion.is_empty() {
            result["expansion_terms"] = Value::Array(expansion);
        }
        // Additive accounting: the merged `connected` array draws from BOTH tiers,
        // so these counts must be the SUM, not the local-only value — otherwise an
        // agent that reads tokens_used/token_budget to decide whether to fetch more
        // context reasons against a number that doesn't match the payload it got.
        for key in ["seeds_expanded", "tokens_used", "token_budget"] {
            let l = local.get(key).and_then(|v| v.as_u64());
            let s = server.get(key).and_then(|v| v.as_u64());
            match (l, s) {
                (Some(a), Some(b)) => result[key] = Value::from(a.saturating_add(b)),
                (Some(a), None) => result[key] = Value::from(a),
                (None, Some(b)) => result[key] = Value::from(b),
                (None, None) => {}
            }
        }
        // Header/identity scalars: keep the local value (they describe the caller's
        // project, not a per-tier count).
        for key in ["project", "project_uid", "external_refs"] {
            if let Some(val) = local.get(key) {
                result[key] = val.clone();
            }
        }
        inject_or_wrap_provenance(&mut result, &["local", "server"], &[]);
        result
    } else {
        merge_json_results(local, server)
    }
}

/// Wrap merged results into a response envelope with provenance metadata.
pub fn wrap_merged_response(results: Vec<Value>, sources: &[&str]) -> Value {
    let scope = if sources.len() > 1 {
        "hybrid"
    } else {
        sources.first().copied().unwrap_or("local")
    };
    serde_json::json!({
        "results": results,
        "_meta": {
            "sources": sources,
            "stale_repos": [],
            "scope": scope,
        },
    })
}

/// Inject `_meta` provenance into an existing JSON object response.
///
/// This is the inner helper; prefer [`inject_or_wrap_provenance`] which
/// also handles bare-array responses.
pub fn inject_provenance(result: &mut Value, sources: &[&str], stale_repos: &[String]) {
    let scope = if sources.len() > 1 {
        "hybrid"
    } else {
        sources.first().copied().unwrap_or("local")
    };
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "_meta".to_string(),
            serde_json::json!({
                "sources": sources,
                "stale_repos": stale_repos,
                "scope": scope,
            }),
        );
    }
}

/// Add provenance to a response, wrapping bare result arrays when needed.
///
/// Most structured RPC responses are JSON objects and can receive `_meta`
/// directly. A few legacy JSON RPCs, notably `search_symbols`, still return a
/// bare array. In the upstream-only fallback path callers still need to know
/// that the data came from the server, so preserve the array under `results`.
pub fn inject_or_wrap_provenance(result: &mut Value, sources: &[&str], stale_repos: &[String]) {
    if result.is_array() {
        let items = result.take();
        *result = serde_json::json!({
            "results": items,
            "_meta": {
                "sources": sources,
                "stale_repos": stale_repos,
                "scope": if sources.len() > 1 {
                    "hybrid"
                } else {
                    sources.first().copied().unwrap_or("local")
                },
            },
        });
    } else {
        inject_provenance(result, sources, stale_repos);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn concat_fanout_appends_rows_without_dropping() {
        // regex_search-shaped: rows must concatenate, not symbol-dedupe (which would drop rows
        // that share a scope hash). Same-uid rows from both sides are preserved.
        let local = json!({ "results": [{ "uid": "a", "line": 1 }, { "uid": "b", "line": 2 }], "truncated": false });
        let server = json!({ "results": [{ "uid": "a", "line": 9 }, { "uid": "c", "line": 3 }] });
        let merged = concat_fanout(&local, &server);
        let rows = merged["results"].as_array().unwrap();
        assert_eq!(rows.len(), 4, "all rows preserved, none deduped");
        // Scalar field keeps local value.
        assert_eq!(merged["truncated"], json!(false));

        // count_patterns-shaped: per-pattern rows from both sides are kept.
        let l = json!({ "patterns": [{ "pattern": "foo", "total_matches": 2 }] });
        let s = json!({ "patterns": [{ "pattern": "foo", "total_matches": 5 }] });
        let m = concat_fanout(&l, &s);
        assert_eq!(m["patterns"].as_array().unwrap().len(), 2);

        // A field only on the server side is carried over.
        let m2 = concat_fanout(
            &json!({ "results": [] }),
            &json!({ "results": [], "extra": 7 }),
        );
        assert_eq!(m2["extra"], json!(7));
    }

    #[test]
    fn concat_fanout_ors_truncation_flags() {
        // If the SERVER truncated its half, the combined result is truncated even
        // though the local side reported complete — otherwise a consumer trusts a
        // false "complete" signal.
        let local = json!({ "results": [], "truncated": false });
        let server = json!({ "results": [], "truncated": true });
        assert_eq!(concat_fanout(&local, &server)["truncated"], json!(true));
        // Neither truncated → stays false.
        let l2 = json!({ "results": [], "truncated": false });
        let s2 = json!({ "results": [], "truncated": false });
        assert_eq!(concat_fanout(&l2, &s2)["truncated"], json!(false));
    }

    #[test]
    fn merge_structured_sums_token_accounting() {
        // tokens_used / seeds_expanded / token_budget must reflect BOTH tiers,
        // since `connected` is the merge of both.
        let local = json!({
            "connected": [{ "uid": "a" }],
            "seeds": [], "tokens_used": 100, "token_budget": 1000, "seeds_expanded": 2,
            "project": "p",
        });
        let server = json!({
            "connected": [{ "uid": "b" }],
            "seeds": [], "tokens_used": 40, "token_budget": 1000, "seeds_expanded": 3,
        });
        let m = merge_structured_results(&local, &server);
        assert_eq!(
            m["tokens_used"],
            json!(140),
            "tokens_used must sum both tiers"
        );
        assert_eq!(m["seeds_expanded"], json!(5));
        assert_eq!(m["token_budget"], json!(2000));
        // Header stays local.
        assert_eq!(m["project"], json!("p"));
    }

    // ── Result helper tests ───────────────────────────────────────

    #[test]
    fn count_results_bare_array() {
        let v = json!([1, 2, 3]);
        assert_eq!(count_results(&v), 3);
    }

    #[test]
    fn count_results_envelope() {
        let v = json!({"results": [1, 2, 3, 4, 5]});
        assert_eq!(count_results(&v), 5);
    }

    #[test]
    fn count_results_items_envelope() {
        let v = json!({"items": [1, 2]});
        assert_eq!(count_results(&v), 2);
    }

    #[test]
    fn count_results_single_object() {
        let v = json!({"name": "foo"});
        assert_eq!(count_results(&v), 1);
    }

    #[test]
    fn count_results_structured_connected() {
        // brain_context / project_context return a structured object whose real
        // payload is the `connected` array. Fallback must count that, not treat
        // the whole response as a single result (which would always trip the
        // server query and then mangle the merge).
        let v = json!({
            "seeds": ["x"],
            "connected": [{"name": "a"}, {"name": "b"}, {"name": "c"}],
        });
        assert_eq!(count_results(&v), 3);
    }

    #[test]
    fn merge_structured_results_preserves_connected_schema() {
        // Merging two structured responses must keep the structured schema
        // (top-level `connected`), not wrap both whole responses into a flat
        // `results` envelope. Regression guard for the fallback merge bug.
        let local = json!({
            "seeds": ["s"],
            "connected": [{"uid": "sym:1", "name": "a", "location": "a.rs"}],
        });
        let server = json!({
            "seeds": ["s"],
            "connected": [{"uid": "sym:2", "name": "b", "location": "b.rs"}],
        });
        let merged = merge_structured_results(&local, &server);
        assert!(
            merged.get("connected").and_then(|v| v.as_array()).is_some(),
            "merged response must retain the `connected` array; got: {merged}"
        );
        assert!(
            merged.get("results").is_none(),
            "structured merge must not flatten into a `results` envelope"
        );
        let connected = merged["connected"].as_array().unwrap();
        assert_eq!(
            connected.len(),
            2,
            "both repos' connected items should merge"
        );
        assert_eq!(merged["_meta"]["sources"][0], "local");
        assert_eq!(merged["_meta"]["sources"][1], "server");
    }

    #[test]
    fn set_stale_repos_populates_meta() {
        let mut v = json!({"results": [], "_meta": {"sources": ["local", "server"]}});
        set_stale_repos(&mut v, &["github.com/acme/api".to_string()]);
        assert_eq!(v["_meta"]["stale_repos"][0], "github.com/acme/api");
    }

    #[test]
    fn set_stale_repos_creates_meta_when_missing() {
        let mut v = json!({"results": []});
        set_stale_repos(&mut v, &["r".to_string()]);
        assert_eq!(v["_meta"]["stale_repos"][0], "r");
    }

    #[test]
    fn count_results_null() {
        assert_eq!(count_results(&Value::Null), 0);
    }

    #[test]
    fn extract_items_from_results_key() {
        let v = json!({"results": [{"a": 1}, {"b": 2}]});
        let items = extract_result_items(&v);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn extract_items_from_bare_array() {
        let v = json!([{"a": 1}]);
        let items = extract_result_items(&v);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn extract_items_from_single_object() {
        let v = json!({"name": "foo", "file": "bar.rs"});
        let items = extract_result_items(&v);
        assert_eq!(items.len(), 1);
    }

    // ── Merge helpers test ────────────────────────────────────────

    #[test]
    fn merge_json_results_deduplicates() {
        let local = json!([{
            "repo_url": "acme/api",
            "file_path": "src/lib.rs",
            "symbol_name": "init",
            "scope_chain": "api"
        }]);
        let server = json!([
            {
                "repo_url": "acme/api",
                "file_path": "src/lib.rs",
                "symbol_name": "init",
                "scope_chain": "api"
            },
            {
                "repo_url": "acme/billing",
                "file_path": "src/webhook.rs",
                "symbol_name": "handle",
                "scope_chain": "billing"
            }
        ]);

        let merged = merge_json_results(&local, &server);
        let results = merged["results"].as_array().unwrap();
        // init appears once (deduplicated), handle is new => 2 results
        assert_eq!(results.len(), 2);
        // Has _meta provenance
        assert!(merged["_meta"].is_object());
        let sources = merged["_meta"]["sources"].as_array().unwrap();
        assert!(sources.len() >= 2);
    }

    #[test]
    fn merge_json_results_empty_server() {
        let local = json!([{"name": "a"}]);
        let server = json!([]);
        let merged = merge_json_results(&local, &server);
        let results = merged["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
    }

    fn complete_search_response(query: &str, results: Vec<Value>) -> Value {
        let total = results.len();
        json!({
            "query": query,
            "engine": "bm25",
            "total_matches": total,
            "total_matches_relation": "eq",
            "returned_matches": total,
            "truncated": false,
            "results": results,
            "expansion_terms": [],
        })
    }

    #[test]
    fn merge_json_results_reports_exact_union_for_disjoint_complete_searches() {
        let mut local = complete_search_response(
            "needle",
            vec![json!({
                "uid": "sym:repo:local:a:one",
                "kind": "Symbol/Function",
                "title": "local_needle",
                "location": "src/local.rs:1"
            })],
        );
        local["expansion_terms"] = json!(["local", "common"]);
        let mut server = complete_search_response(
            "needle",
            vec![json!({
                "uid": "sym:repo:server:b:two",
                "kind": "Symbol/Function",
                "title": "server_needle",
                "location": "src/server.rs:1"
            })],
        );
        server["expansion_terms"] = json!(["server", "common"]);

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["query"], "needle");
        assert_eq!(merged["engine"], "hybrid");
        assert_eq!(merged["total_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "eq");
        assert_eq!(merged["returned_matches"], 2);
        assert_eq!(merged["truncated"], false);
        assert_eq!(
            merged["expansion_terms"],
            json!(["local", "common", "server"])
        );
    }

    #[test]
    fn merge_json_results_reports_exact_union_for_overlapping_complete_searches() {
        let local_shared = json!({
            "uid": "sym:repo:local-instance:repo-hash:file-hash:name-hash:1",
            "kind": "Symbol/Function",
            "title": "shared_needle",
            "location": "src/shared.rs:1"
        });
        let server_shared = json!({
            "uid": "sym:repo:server-instance:repo-hash:file-hash:name-hash:1",
            "kind": "Symbol/Function",
            "title": "shared_needle",
            "location": "src/shared.rs:1"
        });
        let local = complete_search_response("needle", vec![local_shared]);
        let server = complete_search_response("needle", vec![server_shared]);

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 1);
        assert_eq!(merged["total_matches"], 1);
        assert_eq!(merged["total_matches_relation"], "eq");
        assert_eq!(merged["returned_matches"], 1);
        assert_eq!(merged["truncated"], false);
    }

    #[test]
    fn merge_json_results_reports_safe_lower_bound_when_one_search_is_truncated() {
        let local = complete_search_response(
            "needle",
            vec![json!({
                "uid": "sym:repo:local:a:one",
                "kind": "Symbol/Function",
                "title": "local_needle",
                "location": "src/local.rs:1"
            })],
        );
        let server = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 7,
            "total_matches_relation": "eq",
            "returned_matches": 1,
            "truncated": true,
            "results": [{
                "uid": "sym:repo:server:b:two",
                "kind": "Symbol/Function",
                "title": "server_needle",
                "location": "src/server.rs:1"
            }],
            "expansion_terms": ["remote", "common"],
        });

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["total_matches"], 7);
        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(merged["returned_matches"], 2);
        assert_eq!(merged["truncated"], true);
        assert_eq!(merged["expansion_terms"], json!(["remote", "common"]));
    }

    #[test]
    fn merge_json_results_requires_complete_consistent_metadata_from_both_searches() {
        let keyed = |uid: &str| {
            json!({
                "uid": uid,
                "kind": "Symbol/Function",
                "title": "needle",
                "location": "src/lib.rs:1"
            })
        };
        let complete = complete_search_response("needle", vec![keyed("sym:repo:local:a:one")]);

        let missing = json!({
            "query": "needle",
            "engine": "bm25",
            "results": [keyed("sym:repo:server:b:two")],
        });
        assert_eq!(
            merge_json_results(&complete, &missing)["total_matches_relation"],
            "gte"
        );

        for invalid in [
            json!({
                "query": "needle",
                "engine": "bm25",
                "total_matches": -1,
                "total_matches_relation": "eq",
                "returned_matches": 1,
                "truncated": false,
                "results": [keyed("sym:repo:server:b:two")],
            }),
            json!({
                "query": "needle",
                "engine": "bm25",
                "total_matches": "1",
                "total_matches_relation": "eq",
                "returned_matches": 1,
                "truncated": false,
                "results": [keyed("sym:repo:server:b:two")],
            }),
            json!({
                "query": "needle",
                "engine": "bm25",
                "total_matches": 1,
                "total_matches_relation": "eq",
                "returned_matches": "1",
                "truncated": false,
                "results": [keyed("sym:repo:server:b:two")],
            }),
            json!({
                "query": "needle",
                "engine": "bm25",
                "total_matches": 1,
                "total_matches_relation": "eq",
                "returned_matches": 1,
                "truncated": "false",
                "results": [keyed("sym:repo:server:b:two")],
            }),
        ] {
            assert_eq!(
                merge_json_results(&complete, &invalid)["total_matches_relation"],
                "gte",
                "malformed metadata must never prove an exact union: {invalid}"
            );
        }
    }

    #[test]
    fn merge_json_results_rejects_truncated_or_row_count_inconsistent_exact_sources() {
        let local = complete_search_response(
            "needle",
            vec![json!({
                "uid": "sym:repo:local:a:one",
                "kind": "Symbol/Function",
                "title": "local",
                "location": "src/local.rs:1"
            })],
        );
        let row = json!({
            "uid": "sym:repo:server:b:two",
            "kind": "Symbol/Function",
            "title": "server",
            "location": "src/server.rs:1"
        });
        let explicitly_truncated = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 1,
            "total_matches_relation": "eq",
            "returned_matches": 1,
            "truncated": true,
            "results": [row.clone()],
        });
        let wrong_actual_count = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 2,
            "total_matches_relation": "eq",
            "returned_matches": 2,
            "truncated": false,
            "results": [row],
        });

        assert_eq!(
            merge_json_results(&local, &explicitly_truncated)["total_matches_relation"],
            "gte"
        );
        assert_eq!(
            merge_json_results(&local, &wrong_actual_count)["total_matches_relation"],
            "gte"
        );
    }

    #[test]
    fn merge_json_results_inconsistent_metadata_cannot_inflate_the_lower_bound() {
        let local = complete_search_response("needle", Vec::new());
        let inconsistent = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 999,
            "total_matches_relation": "eq",
            "returned_matches": 999,
            "truncated": false,
            "results": [{
                "uid": "sym:repo:server:repo-hash:file-hash:name-hash:1",
                "kind": "Symbol/Function",
                "title": "server",
                "location": "src/server.rs:1"
            }],
        });

        let merged = merge_json_results(&local, &inconsistent);

        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(
            merged["total_matches"], 1,
            "internally inconsistent source totals are not trustworthy lower bounds"
        );
    }

    #[test]
    fn merge_json_results_uses_uid_identity_for_concise_notes_and_tags() {
        let local = complete_search_response(
            "needle",
            vec![
                json!({"uid": "note:vlt:local-instance:vault-hash:note-hash", "kind": "note", "title": "needle"}),
                json!({"uid": "tag:vlt:local-instance:vault-hash:tag-hash", "kind": "note", "title": "needle"}),
            ],
        );
        let server = complete_search_response(
            "needle",
            vec![
                json!({"uid": "note:vlt:server-instance:vault-hash:note-hash", "kind": "note", "title": "needle"}),
                json!({"uid": "tag:vlt:server-instance:vault-hash:tag-hash", "kind": "note", "title": "needle"}),
            ],
        );

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 2);
        assert_eq!(merged["total_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "eq");
    }

    #[test]
    fn merge_json_results_keeps_same_path_symbols_from_distinct_repos() {
        let row = |uid: &str| {
            json!({
                "uid": uid,
                "kind": "Symbol/Function",
                "title": "same",
                "location": "src/lib.rs:7"
            })
        };
        let local = complete_search_response("needle", vec![row("sym:repo:local:abc:same:7")]);
        let server = complete_search_response("needle", vec![row("sym:repo:server:def:same:7")]);

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 2);
        assert_eq!(merged["total_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "eq");
    }

    #[test]
    fn merge_json_results_deduplicates_detailed_substring_notes_without_locations() {
        let note = json!({
            "uid": "note:vlt:a:one",
            "kind": "note",
            "title": "needle",
            "score": 1.0,
            "matched_headings": ["needle heading"]
        });
        let local = complete_search_response("needle", vec![note.clone()]);
        let server = complete_search_response("needle", vec![note]);

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 1);
        assert_eq!(merged["total_matches"], 1);
        assert_eq!(merged["total_matches_relation"], "eq");
    }

    #[test]
    fn merge_json_results_keeps_same_title_note_and_symbol_distinct() {
        let local = complete_search_response(
            "needle",
            vec![json!({
                "uid": "note:vlt:a:one",
                "kind": "note",
                "title": "needle"
            })],
        );
        let server = complete_search_response(
            "needle",
            vec![json!({
                "uid": "sym:repo:server:def:needle:7",
                "kind": "Symbol/Function",
                "title": "needle",
                "location": "src/lib.rs:7"
            })],
        );

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 2);
        assert_eq!(merged["total_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "eq");
    }

    #[test]
    fn merge_json_results_unkeyed_rows_never_prove_or_inflate_a_union() {
        let unkeyed = json!({"kind": "note", "title": "needle"});
        let local = complete_search_response("needle", vec![unkeyed.clone()]);
        let server = complete_search_response("needle", vec![unkeyed]);

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 2);
        assert_eq!(merged["returned_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(
            merged["total_matches"], 1,
            "unkeyed presentation rows cannot prove distinct logical entities"
        );
    }

    #[test]
    fn merge_json_results_duplicate_uid_inside_a_source_cannot_prove_exactness() {
        let duplicate = json!({
            "uid": "note:vlt:local-instance:vault-hash:note-hash",
            "kind": "note",
            "title": "needle"
        });
        let local = complete_search_response("needle", vec![duplicate.clone(), duplicate]);
        let server = complete_search_response("needle", Vec::new());

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 1);
        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(merged["truncated"], true);
    }

    #[test]
    fn wrap_merged_response_has_metadata() {
        let results = vec![json!({"name": "a"})];
        let wrapped = wrap_merged_response(results, &["local", "server"]);
        assert!(wrapped["results"].is_array());
        // _meta provenance
        assert!(wrapped["_meta"].is_object());
        let meta = &wrapped["_meta"];
        assert_eq!(meta["scope"], "hybrid");
        let sources = meta["sources"].as_array().unwrap();
        assert!(sources.len() >= 2);
    }

    #[test]
    fn wrap_merged_response_single_source_scope() {
        let results = vec![json!({"name": "a"})];
        let wrapped = wrap_merged_response(results, &["local"]);
        assert_eq!(wrapped["_meta"]["scope"], "local");
    }

    #[test]
    fn inject_or_wrap_provenance_wraps_bare_array() {
        let mut val = json!([{"name": "server_only"}]);
        inject_or_wrap_provenance(&mut val, &["server"], &[]);

        assert_eq!(val["results"][0]["name"], "server_only");
        assert_eq!(val["_meta"]["sources"][0], "server");
        assert_eq!(val["_meta"]["scope"], "server");
    }

    #[test]
    fn inject_provenance_adds_meta() {
        let mut val = json!({"results": [1, 2, 3]});
        inject_or_wrap_provenance(&mut val, &["local", "acme"], &["repo-a".to_string()]);
        assert!(val["_meta"].is_object());
        assert_eq!(val["_meta"]["scope"], "hybrid");
        assert_eq!(val["_meta"]["stale_repos"][0], "repo-a");
        assert_eq!(val["_meta"]["sources"][0], "local");
        assert_eq!(val["_meta"]["sources"][1], "acme");
    }

    #[test]
    fn inject_provenance_local_only_scope() {
        let mut val = json!({"results": []});
        inject_or_wrap_provenance(&mut val, &["local"], &[]);
        assert_eq!(val["_meta"]["scope"], "local");
        assert!(val["_meta"]["stale_repos"].as_array().unwrap().is_empty());
    }

    #[test]
    fn merge_structured_response_preserves_schema() {
        let local = json!({
            "seeds": [{"uid": "s1", "label": "foo"}],
            "connected": [
                {"uid": "c1", "label": "bar", "score": 0.9},
                {"uid": "c2", "label": "baz", "score": 0.7}
            ],
            "unresolved_seeds": []
        });
        let server = json!({
            "seeds": [{"uid": "s2", "label": "qux"}],
            "connected": [
                {"uid": "c3", "label": "quux", "score": 0.8},
                {"uid": "c1", "label": "bar", "score": 0.85}
            ],
            "unresolved_seeds": []
        });

        let merged = merge_structured_results(&local, &server);

        assert!(
            merged.get("connected").is_some(),
            "connected field must be preserved"
        );
        assert!(
            merged.get("seeds").is_some(),
            "seeds field must be preserved"
        );
        assert!(merged.get("_meta").is_some(), "_meta must be present");
        let connected = merged["connected"].as_array().unwrap();
        assert!(
            connected.len() >= 3,
            "should merge connected items from both"
        );
    }

    #[test]
    fn merge_flat_response_uses_results_envelope() {
        let local = json!({"results": [{"uid": "r1", "score": 0.9}]});
        let server = json!({"results": [{"uid": "r2", "score": 0.8}]});

        let merged = merge_structured_results(&local, &server);
        assert!(merged.get("results").is_some());
    }

    #[test]
    fn merge_structured_preserves_project_metadata() {
        let local = json!({
            "project": "billing",
            "project_uid": "uid-123",
            "seeds_expanded": 5,
            "tokens_used": 1200,
            "token_budget": 5000,
            "external_refs": [{"url": "https://example.com"}],
            "seeds": [{"uid": "s1"}],
            "connected": [{"uid": "c1", "score": 0.9}],
        });
        let server = json!({
            "project": "billing",
            "project_uid": "uid-456",
            "seeds": [],
            "connected": [{"uid": "c2", "score": 0.8}],
        });

        let merged = merge_structured_results(&local, &server);

        assert_eq!(merged["project"], "billing");
        assert_eq!(merged["project_uid"], "uid-123");
        assert_eq!(merged["seeds_expanded"], 5);
        assert_eq!(merged["tokens_used"], 1200);
        assert_eq!(merged["token_budget"], 5000);
        assert!(merged.get("external_refs").is_some());
    }
}

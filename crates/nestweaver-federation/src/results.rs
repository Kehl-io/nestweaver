//! Result-shaping helpers shared by every hybrid routing path: counting and
//! extracting result items, merging local + server responses (RRF + scope-hash
//! dedup, structured-schema preservation, fan-out concatenation), and
//! injecting `_meta` provenance / staleness annotations.

use std::collections::{HashMap, HashSet};

use nestweaver_schema::uid::{
    SearchEntityUidKind, normalize_search_entity_uid, normalize_search_symbol_canonical_id,
};
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

    let valid = match (total, returned, relation, truncated) {
        (Some(total), Some(returned), Some("eq"), Some(truncated)) => {
            returned == actual && total >= returned && truncated == (returned < total)
        }
        (Some(total), Some(returned), Some("gte"), Some(true)) => {
            returned == actual && total >= returned
        }
        _ => false,
    };
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

/// Return the stable logical identity carried by a `brain_search` row.
///
/// The shared schema parser enforces the exact constructor grammar before
/// removing the instance component. This layer additionally checks that the
/// row's presentation kind agrees with the parsed UID domain. Symbols must also
/// carry a validated edit-stable canonical ID; their line-sensitive UID is
/// never used as a federation proof identity. Older or malformed responses
/// remain visible but unkeyed and cannot prove union cardinality.
fn brain_search_logical_uid(value: &Value) -> Option<String> {
    let uid = value.get("uid").and_then(Value::as_str)?;
    let kind = value.get("kind").and_then(Value::as_str)?;
    let (uid_kind, normalized) = normalize_search_entity_uid(uid)?;
    match uid_kind {
        SearchEntityUidKind::Note | SearchEntityUidKind::Tag => {
            (kind == "note").then_some(normalized)
        }
        SearchEntityUidKind::Symbol => {
            kind.strip_prefix("Symbol/")
                .filter(|symbol_kind| !symbol_kind.is_empty())?;
            let canonical_id = value.get("canonical_id").and_then(Value::as_str)?;
            normalize_search_symbol_canonical_id(uid, canonical_id)
        }
    }
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

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// Merge the `semantic_applied` / `degraded_components` honesty fields across
/// the two tiers of a federated response.
///
/// These fields exist so a caller can tell "no semantic leg ran" apart from
/// "the field is not implemented on this path". Rebuilding a merged response
/// without them re-creates exactly that ambiguity, so they are merged here
/// rather than dropped.
///
/// Used by BOTH federated merges: the flat `{ results: [...] }` envelope
/// ([`merge_json_results`], serving `brain_search`) and the structured
/// `connected` envelope ([`merge_structured_results`], serving `brain_context`
/// and `project_context`). Both rebuild their response field by field, so both
/// have to re-add these deliberately. The rules below are identical for the two
/// callers; only their *reachability* differs, noted inline where it matters.
///
/// Merge rules, and why:
///
/// - `semantic_applied` is **conjunctive (AND across contributing tiers)**. The
///   merged result set is a union of rows from every tier, so the property can
///   only be claimed for the whole answer if it holds for every tier that
///   contributed to it. If one tier ran a semantic leg and the other did not,
///   part of the merged ranking is purely lexical, and reporting `true` would
///   overclaim on those rows. `false` is the honest, conservative answer. For
///   `brain_search` the merge is trivial — it is keyword/BM25-only everywhere,
///   so both tiers report `false` — but for `brain_context` /
///   `project_context` a real semantic leg exists and either tier can report
///   `true` on its own.
///
///   **Known limitation, deliberately accepted:** this collapses two states a
///   caller might want to tell apart. "Neither tier ran a semantic leg" and
///   "local ran one, the server did not" both emit `semantic_applied: false`.
///   For `brain_search` that mixed state is hypothetical; on the structured
///   path it is **reachable today** — two independently configured daemons can
///   differ in whether an embedding model is loaded, so a merged
///   `brain_context` can genuinely mix a semantically-ranked tier with a
///   lexical one and will report `false`. The AND is still the correct answer
///   (a union containing purely lexical rows cannot claim the property for the
///   whole answer), it is simply less informative than the two-tier truth.
///
///   It is NOT encoded here. A single boolean cannot carry per-tier
///   provenance, and inventing a vocabulary to encode it would put values into
///   `degraded_components` that no consumer understands — the field's
///   documented vocabulary is exactly `"semantic"` (README,
///   docs/server-mode.md, `agent_guide`) and agents are told it means retrieval
///   fell back to lexical ranking. Note the mixed case is also NOT a
///   degradation: a tier that never requested a semantic leg did not fall back
///   from one, so nothing may be added to `degraded_components` on its account.
///   If callers ever need to distinguish it, it belongs in `_meta` as explicit
///   per-tier provenance, not smuggled into this list.
///
/// - `degraded_components` is a **deduplicated union**, in local-then-server
///   encounter order. Degradation is a property of the merged answer as a
///   whole: if a component was degraded while producing any tier's rows, the
///   rows the caller received are affected by that degradation, so it must be
///   visible. Intersection or local-only would hide a real, server-side
///   degradation behind a healthy local tier. Order is insertion order (not
///   sorted) so the output is deterministic and matches the surrounding
///   `union_expansion_terms` convention.
///
///   **Blind spot, also accepted:** the union is over *tiers*, not over rows
///   that survived the merge. A tier that contributed zero surviving rows still
///   flags the merged answer degraded — reachable via `query_merge` when the
///   server returns nothing, and via `query_fallback`, where the server is
///   queried precisely because local was sparse. Over-reporting degradation is
///   the safe direction (the alternative silently drops a real degradation), so
///   this is intentional, not an oversight.
///
///   `semantic_applied` has the **identical** blind spot, in the same
///   direction: it too is computed over tiers, not over surviving rows. A
///   server that returns `connected: []` with `semantic_applied: false`, merged
///   with a local tier that ranked semantically, yields a merged row set that
///   is entirely local and entirely semantically ranked — reported as `false`.
///   Under-claiming is the safe direction for a property (as over-reporting is
///   for a degradation), so both are accepted rather than fixed by weighting
///   the fields by surviving-row provenance.
///
/// - **A tier that omits `semantic_applied` forces the merged value to
///   `false`** — we cannot claim a property we were not told about.
///   Reachability differs per tool, and the difference is load-bearing:
///
///   - `brain_search` and `brain_context`: **not reachable.** Both go through
///     `dispatch::dispatch_typed_brain_search` / `dispatch_typed_brain_context`,
///     which insert both keys unconditionally, and both proto responses carry
///     them as *typed* fields (`BrainSearchResponse`, `BrainContextResponse`),
///     so proto3 scalar defaults decode an older server to `false`/`[]` rather
///     than to an absent field. Here the branch is defensive only, and not a
///     supported way to detect an old server.
///
///   - `project_context`: **reachable.** `ProjectContextResponse` is
///     `{ string result_json = 1; }` — no typed honesty fields at all — and
///     `dispatch_typed_project_context` returns the parsed `result_json`
///     untouched, inserting nothing. Both fields therefore ride *inside* the
///     JSON, and `tool_project_context` has an early-return envelope for a
///     project with no members that emits `connected: []` and **neither**
///     field. A federated `project_context` where one side's project is empty
///     still enters the structured branch, so the AND below really can see a
///     silent tier and force `false` even when every surviving row came from a
///     tier that genuinely applied semantic ranking. That under-claims, which
///     is the safe direction, but it is a real case and not a hypothetical.
///     If BOTH tiers are empty projects, `any_reported` is false and neither
///     field is emitted — worth knowing, because `agent_guide` tells agents
///     `brain_context` / `project_context` always emit `degraded_components`.
///
/// - **If no tier reports either field, nothing is emitted.** Inventing
///   `semantic_applied: false` out of two silent tiers would destroy the very
///   signal these fields carry: an entirely absent field still has to read as
///   "not implemented on this path" at the top level. This also keeps the
///   merge a no-op for tools (regex_search, count_patterns, …) that never had
///   these fields.
fn merge_honesty_fields(local: &Value, server: &Value, response: &mut Value) {
    let tiers = [local, server];
    let any_reported = tiers.iter().any(|value| {
        value.get("semantic_applied").is_some() || value.get("degraded_components").is_some()
    });
    if !any_reported {
        return;
    }

    let mut semantic_applied = true;
    let mut degraded: Vec<String> = Vec::new();
    for value in tiers {
        semantic_applied &= value
            .get("semantic_applied")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(items) = value.get("degraded_components").and_then(Value::as_array) {
            for component in items.iter().filter_map(Value::as_str) {
                push_unique(&mut degraded, component.to_string());
            }
        }
    }

    response["semantic_applied"] = Value::Bool(semantic_applied);
    response["degraded_components"] = serde_json::json!(degraded);
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

    // `wrap_merged_response` emits only `results` + `_meta`, so every field the
    // tiers reported has to be re-added deliberately. The honesty fields are
    // merged (not copied) — see `merge_honesty_fields` for the rules — and are
    // a no-op for tools that never carried them.
    merge_honesty_fields(local, server, &mut response);

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
/// Union of both tiers' `cross_repo_links`, deduplicated on (package, link_type).
///
/// Where both tiers report the same link, the higher `confidence` wins: they are
/// two independent estimates of one fact, not two facts.
fn merge_cross_repo_links(local: &Value, server: &Value) -> Vec<Value> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut best: HashMap<(String, String), Value> = HashMap::new();

    for tier in [local, server] {
        let Some(links) = tier.get("cross_repo_links").and_then(|v| v.as_array()) else {
            continue;
        };
        for link in links {
            let key = (
                link.get("package")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                link.get("link_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            );
            let confidence = link
                .get("confidence")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            match best.get(&key) {
                Some(existing) => {
                    let existing_confidence = existing
                        .get("confidence")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    if confidence > existing_confidence {
                        best.insert(key, link.clone());
                    }
                }
                None => {
                    order.push(key.clone());
                    best.insert(key, link.clone());
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|key| best.remove(&key))
        .collect()
}

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

        // `cross_repo_links` is a UNION, not a local-only copy: an upstream tier
        // sees links a local graph cannot. Dropping it entirely — which this
        // rebuilt envelope did — is worse than either, because
        // `ContextResult::cross_repo_links` is a REQUIRED field, so the CLI's
        // `serde_json::from_value` failed outright the moment both tiers
        // answered. `nestweaver context` was broken for anyone with an upstream
        // configured, and only for them.
        //
        // Deduplicated on (package, link_type) keeping the HIGHER confidence:
        // the same dependency seen by both tiers is one link, and the stronger
        // of two independent estimates is the better one to report.
        let merged_links = merge_cross_repo_links(local, server);
        if !merged_links.is_empty() {
            result["cross_repo_links"] = Value::Array(merged_links);
        } else if local.get("cross_repo_links").is_some()
            || server.get("cross_repo_links").is_some()
        {
            // Present-but-empty is not the same as absent to a required field.
            result["cross_repo_links"] = Value::Array(Vec::new());
        }

        // RECOMPUTED from the merged payload, never summed. `rrf_merge`
        // deduplicates, so `local + server` overcounts by exactly the overlap —
        // and these two describe the arrays the caller is holding, unlike the
        // additive accounting above which describes work done by each tier.
        if local.get("seeds_resolved").is_some() || server.get("seeds_resolved").is_some() {
            let seeds_len = result["seeds"].as_array().map_or(0, Vec::len);
            result["seeds_resolved"] = Value::from(seeds_len);
        }
        if local.get("connected_count").is_some() || server.get("connected_count").is_some() {
            let connected_len = result["connected"].as_array().map_or(0, Vec::len);
            result["connected_count"] = Value::from(connected_len);
        }
        // This envelope is rebuilt key by key above, so anything not re-added
        // here is silently dropped. `semantic_applied` / `degraded_components`
        // are merged (not copied) under the same rules as `merge_json_results`
        // — AND for the boolean, deduplicated union for the list, both omitted
        // when no tier reported either. See `merge_honesty_fields`.
        //
        // Why it matters MORE here than on the `brain_search` path: these two
        // tools have a real semantic leg, so the fields carry information
        // rather than being trivially `false`/`[]`, and the mixed case (one
        // tier ranked semantically, the other lexically — two daemons need not
        // agree on whether an embedding model is loaded) is reachable in
        // production. The AND rule collapses that mixed case into `false`,
        // which is correct-but-lossy: correct because the merged rows include
        // purely lexical ones, lossy because a caller cannot see which tier
        // ranked how. Per-tier provenance is deliberately NOT encoded in
        // `degraded_components` — that field's vocabulary is exactly
        // `"semantic"` and means "fell back from a requested semantic leg",
        // which is not what a mixed merge describes. If it is ever needed it
        // belongs in `_meta`.
        merge_honesty_fields(local, server, &mut result);
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
    use nestweaver_schema::uid::{
        canonical_symbol_id, note_uid, repo_uid, symbol_uid, tag_uid, vault_uid,
    };
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

    /// `nestweaver context` was BROKEN for anyone with an upstream configured.
    /// `merge_structured_results` rebuilds its envelope key by key, and
    /// `cross_repo_links` was not among the keys re-added — but it is a
    /// REQUIRED field of `ContextResult`, so the CLI's `serde_json::from_value`
    /// failed outright the moment both tiers answered. Single-tier users never
    /// saw it, because the merge path is only taken when both succeed.
    #[test]
    fn cross_repo_links_survive_a_two_tier_merge() {
        let local = serde_json::json!({
            "seeds": [], "connected": [],
            "cross_repo_links": [{ "package": "serde", "link_type": "dependency", "confidence": 0.5 }],
        });
        let server = serde_json::json!({
            "seeds": [], "connected": [],
            "cross_repo_links": [{ "package": "tokio", "link_type": "dependency", "confidence": 0.9 }],
        });

        let merged = merge_structured_results(&local, &server);
        let links = merged["cross_repo_links"]
            .as_array()
            .expect("must be present");

        assert_eq!(links.len(), 2, "the union of both tiers, got {links:?}");
        let packages: Vec<&str> = links
            .iter()
            .filter_map(|l| l.get("package").and_then(|v| v.as_str()))
            .collect();
        assert!(packages.contains(&"serde") && packages.contains(&"tokio"));
    }

    /// The same dependency seen by both tiers is ONE link, and the stronger of
    /// two independent estimates is the better one to report. Without this, a
    /// federated caller sees every shared dependency twice.
    #[test]
    fn a_link_both_tiers_report_is_deduplicated_at_the_higher_confidence() {
        let local = serde_json::json!({
            "seeds": [], "connected": [],
            "cross_repo_links": [{ "package": "serde", "link_type": "dependency", "confidence": 0.4 }],
        });
        let server = serde_json::json!({
            "seeds": [], "connected": [],
            "cross_repo_links": [{ "package": "serde", "link_type": "dependency", "confidence": 0.8 }],
        });

        let merged = merge_structured_results(&local, &server);
        let links = merged["cross_repo_links"].as_array().unwrap();

        assert_eq!(links.len(), 1, "one dependency, one link");
        assert_eq!(links[0]["confidence"], serde_json::json!(0.8));
    }

    /// Present-but-empty is not the same as absent to a required field.
    #[test]
    fn an_empty_cross_repo_links_stays_present_rather_than_vanishing() {
        let local = serde_json::json!({ "seeds": [], "connected": [], "cross_repo_links": [] });
        let server = serde_json::json!({ "seeds": [], "connected": [], "cross_repo_links": [] });

        let merged = merge_structured_results(&local, &server);

        assert_eq!(merged["cross_repo_links"], serde_json::json!([]));
    }

    /// `rrf_merge` DEDUPLICATES, so summing the tiers overcounts by exactly the
    /// overlap. These two describe the arrays the caller is holding, unlike the
    /// additive token accounting, which describes work each tier did.
    #[test]
    fn counts_describe_the_merged_payload_not_the_sum_of_tiers() {
        let node = |uid: &str| serde_json::json!({ "uid": uid, "name": uid, "relevance": 1.0 });
        let local = serde_json::json!({
            "seeds": [node("sym:a")],
            "connected": [node("sym:shared"), node("sym:local_only")],
            "connected_count": 2, "seeds_resolved": 1,
        });
        let server = serde_json::json!({
            "seeds": [],
            "connected": [node("sym:shared")],
            "connected_count": 1, "seeds_resolved": 0,
        });

        let merged = merge_structured_results(&local, &server);

        let actual = merged["connected"].as_array().unwrap().len();
        assert_eq!(
            merged["connected_count"].as_u64().unwrap() as usize,
            actual,
            "connected_count must equal the array it describes, not 2 + 1"
        );
        assert_eq!(
            merged["seeds_resolved"].as_u64().unwrap() as usize,
            merged["seeds"].as_array().unwrap().len()
        );
    }

    /// The CLASS guard, not the instance. This envelope is rebuilt key by key,
    /// so any field a tool adds later is dropped SILENTLY — which is exactly how
    /// `cross_repo_links` broke `nestweaver context` without a single test
    /// noticing. A new key must either survive the merge or be added to the
    /// intentionally-dropped list here, deliberately and with a reason.
    #[test]
    fn no_local_field_is_dropped_without_being_declared_intentional() {
        // Per-tier bookkeeping that is meaningless after a merge.
        const INTENTIONALLY_DROPPED: &[&str] = &["_meta"];

        let local = serde_json::json!({
            "seeds": [], "connected": [], "cross_repo_links": [],
            "unresolved_seeds": ["ghost"], "expansion_terms": ["term"],
            "seeds_expanded": 1, "tokens_used": 10, "token_budget": 100,
            "project": "p", "project_uid": "proj:p", "external_refs": [],
            "seeds_resolved": 0, "connected_count": 0,
            "semantic_applied": true, "degraded_components": [],
            "_meta": { "sources": ["local"] },
        });
        let server = serde_json::json!({ "seeds": [], "connected": [] });

        let merged = merge_structured_results(&local, &server);

        let dropped: Vec<&String> = local
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| !INTENTIONALLY_DROPPED.contains(&key.as_str()))
            .filter(|key| merged.get(key.as_str()).is_none())
            .collect();

        assert!(
            dropped.is_empty(),
            "these fields vanished in the merge: {dropped:?} — carry them through \
             merge_structured_results, or add them to INTENTIONALLY_DROPPED with a reason"
        );
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

    fn symbol_search_uid(
        instance: &str,
        repo_url: &str,
        file_path: &str,
        name: &str,
        line: u32,
    ) -> String {
        symbol_uid(&repo_uid(instance, repo_url), file_path, name, line)
    }

    fn symbol_search_row(
        instance: &str,
        repo_url: &str,
        file_path: &str,
        name: &str,
        line: u32,
    ) -> Value {
        json!({
            "uid": symbol_search_uid(instance, repo_url, file_path, name, line),
            "canonical_id": canonical_symbol_id(repo_url, file_path, name, "module"),
            "kind": "Symbol/Function",
            "title": name,
            "location": format!("{file_path}:{line}")
        })
    }

    fn note_search_row(instance: &str, root_path: &str, rel_path: &str, title: &str) -> Value {
        json!({
            "uid": note_uid(&vault_uid(instance, root_path), rel_path),
            "kind": "note",
            "title": title
        })
    }

    fn tag_search_row(instance: &str, root_path: &str, tag: &str) -> Value {
        json!({
            "uid": tag_uid(&vault_uid(instance, root_path), tag),
            "kind": "note",
            "title": tag
        })
    }

    #[test]
    fn merge_json_results_reports_exact_union_for_disjoint_complete_searches() {
        let mut local = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "local",
                "https://github.com/acme/local",
                "src/local.rs",
                "local_needle",
                1,
            )],
        );
        local["expansion_terms"] = json!(["local", "common"]);
        let mut server = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "server",
                "https://github.com/acme/server",
                "src/server.rs",
                "server_needle",
                1,
            )],
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
        let local_shared = symbol_search_row(
            "local-instance",
            "git@github.com:acme/shared.git",
            "src/shared.rs",
            "shared_needle",
            1,
        );
        let server_shared = symbol_search_row(
            "server-instance",
            "https://github.com/acme/shared",
            "src/shared.rs",
            "shared_needle",
            1,
        );
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
    fn merge_json_results_uses_canonical_symbol_identity_across_line_shifts() {
        let local = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "local-instance",
                "https://github.com/acme/shared",
                "src/shared.rs",
                "shared_needle",
                7,
            )],
        );
        let server = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "server-instance",
                "git@github.com:acme/shared.git",
                "src/shared.rs",
                "shared_needle",
                42,
            )],
        );

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 1);
        assert_eq!(merged["total_matches"], 1);
        assert_eq!(merged["total_matches_relation"], "eq");
        assert_eq!(merged["returned_matches"], 1);
        assert_eq!(merged["truncated"], false);
    }

    #[test]
    fn merge_json_results_missing_symbol_canonical_id_stays_visible_but_unproven() {
        let mut local_row = symbol_search_row(
            "local-instance",
            "https://github.com/acme/shared",
            "src/shared.rs",
            "shared_needle",
            7,
        );
        let mut server_row = symbol_search_row(
            "server-instance",
            "git@github.com:acme/shared.git",
            "src/shared.rs",
            "shared_needle",
            7,
        );
        local_row.as_object_mut().unwrap().remove("canonical_id");
        server_row.as_object_mut().unwrap().remove("canonical_id");
        let local = complete_search_response("needle", vec![local_row]);
        let server = complete_search_response("needle", vec![server_row]);

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 2);
        assert_eq!(merged["returned_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(
            merged["total_matches"], 1,
            "rows without stable identity cannot inflate the proven lower bound"
        );
        assert_eq!(merged["truncated"], true);
    }

    #[test]
    fn merge_json_results_invalid_symbol_canonical_ids_stay_visible_but_unproven() {
        let repo_url = "https://github.com/acme/shared";
        let invalid_ids = [
            "not-a-canonical-id".to_string(),
            canonical_symbol_id(
                "https://github.com/acme/other",
                "src/shared.rs",
                "shared_needle",
                "module",
            ),
            canonical_symbol_id(repo_url, "src/other.rs", "shared_needle", "module"),
        ];

        for invalid_id in invalid_ids {
            let mut local_row = symbol_search_row(
                "local-instance",
                repo_url,
                "src/shared.rs",
                "shared_needle",
                7,
            );
            let mut server_row = symbol_search_row(
                "server-instance",
                repo_url,
                "src/shared.rs",
                "shared_needle",
                7,
            );
            local_row["canonical_id"] = json!(invalid_id);
            server_row["canonical_id"] = json!(invalid_id);
            let local = complete_search_response("needle", vec![local_row]);
            let server = complete_search_response("needle", vec![server_row]);

            let merged = merge_json_results(&local, &server);

            assert_eq!(
                merged["results"].as_array().unwrap().len(),
                2,
                "invalid canonical IDs must remain visible: {invalid_id}"
            );
            assert_eq!(merged["returned_matches"], 2);
            assert_eq!(merged["total_matches_relation"], "gte");
            assert_eq!(
                merged["total_matches"], 1,
                "invalid canonical IDs cannot inflate the proven lower bound: {invalid_id}"
            );
            assert_eq!(merged["truncated"], true);
        }
    }

    #[test]
    fn merge_json_results_reports_safe_lower_bound_when_one_search_is_truncated() {
        let local = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "local",
                "https://github.com/acme/local",
                "src/local.rs",
                "local_needle",
                1,
            )],
        );
        let server_row = symbol_search_row(
            "server",
            "https://github.com/acme/server",
            "src/server.rs",
            "server_needle",
            1,
        );
        let server = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 7,
            "total_matches_relation": "eq",
            "returned_matches": 1,
            "truncated": true,
            "results": [server_row],
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
    fn merge_json_results_legacy_missing_relation_cannot_prove_exact_union() {
        let local = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "local",
                "https://github.com/acme/local",
                "src/local.rs",
                "local_needle",
                1,
            )],
        );
        let server = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 1,
            "returned_matches": 1,
            "truncated": true,
            "results": [symbol_search_row(
                "server",
                "https://github.com/acme/server",
                "src/server.rs",
                "server_needle",
                1,
            )],
        });

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(merged["truncated"], true);
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
        let local_uid = symbol_search_uid(
            "local",
            "https://github.com/acme/local",
            "src/lib.rs",
            "needle",
            1,
        );
        let server_uid = symbol_search_uid(
            "server",
            "https://github.com/acme/server",
            "src/lib.rs",
            "needle",
            1,
        );
        let complete = complete_search_response("needle", vec![keyed(&local_uid)]);

        let missing = json!({
            "query": "needle",
            "engine": "bm25",
            "results": [keyed(&server_uid)],
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
                "results": [keyed(&server_uid)],
            }),
            json!({
                "query": "needle",
                "engine": "bm25",
                "total_matches": "1",
                "total_matches_relation": "eq",
                "returned_matches": 1,
                "truncated": false,
                "results": [keyed(&server_uid)],
            }),
            json!({
                "query": "needle",
                "engine": "bm25",
                "total_matches": 1,
                "total_matches_relation": "eq",
                "returned_matches": "1",
                "truncated": false,
                "results": [keyed(&server_uid)],
            }),
            json!({
                "query": "needle",
                "engine": "bm25",
                "total_matches": 1,
                "total_matches_relation": "eq",
                "returned_matches": 1,
                "truncated": "false",
                "results": [keyed(&server_uid)],
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
            vec![symbol_search_row(
                "local",
                "https://github.com/acme/local",
                "src/local.rs",
                "local",
                1,
            )],
        );
        let row = symbol_search_row(
            "server",
            "https://github.com/acme/server",
            "src/server.rs",
            "server",
            1,
        );
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
        let server_row = symbol_search_row(
            "server",
            "https://github.com/acme/server",
            "src/server.rs",
            "server",
            1,
        );
        let inconsistent = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 999,
            "total_matches_relation": "eq",
            "returned_matches": 999,
            "truncated": false,
            "results": [server_row],
        });

        let merged = merge_json_results(&local, &inconsistent);

        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(
            merged["total_matches"], 1,
            "internally inconsistent source totals are not trustworthy lower bounds"
        );
    }

    #[test]
    fn merge_json_results_rejects_noncanonical_or_kind_inconsistent_search_uids() {
        let valid_note = note_uid(&vault_uid("local", "/shared/vault"), "notes/needle.md");
        let valid_symbol = symbol_search_uid(
            "local",
            "https://github.com/acme/repo",
            "src/lib.rs",
            "needle",
            7,
        );
        for (uid, kind) in [
            // Missing, extra, and empty components.
            (
                "note:vlt:local:0123456789ab".to_string(),
                "note".to_string(),
            ),
            (
                "note:vlt:local:0123456789ab:abcdef012345:extra".to_string(),
                "note".to_string(),
            ),
            (
                "note:vlt::0123456789ab:abcdef012345".to_string(),
                "note".to_string(),
            ),
            (
                "sym:repo:local:0123456789ab:abcdef012345:123456789abc".to_string(),
                "Symbol/Function".to_string(),
            ),
            (
                "sym:repo:local:0123456789ab:abcdef012345:123456789abc:7:extra".to_string(),
                "Symbol/Function".to_string(),
            ),
            // Invalid instance and noncanonical hashes.
            (
                "note:vlt:local box:0123456789ab:abcdef012345".to_string(),
                "note".to_string(),
            ),
            (
                "note:vlt:local:0123456789AB:abcdef012345".to_string(),
                "note".to_string(),
            ),
            (
                "tag:vlt:local:0123456789ab:abcdef01234g".to_string(),
                "note".to_string(),
            ),
            (
                "sym:repo:local:0123456789ab:abcdef01234:123456789abc:7".to_string(),
                "Symbol/Function".to_string(),
            ),
            // Bad, overflowing, and empty symbol lines.
            (
                "sym:repo:local:0123456789ab:abcdef012345:123456789abc:not-a-line".to_string(),
                "Symbol/Function".to_string(),
            ),
            (
                "sym:repo:local:0123456789ab:abcdef012345:123456789abc:4294967296".to_string(),
                "Symbol/Function".to_string(),
            ),
            (
                "sym:repo:local:0123456789ab:abcdef012345:123456789abc:".to_string(),
                "Symbol/Function".to_string(),
            ),
            // A canonical UID must agree with the row's presentation domain.
            (valid_note, "Symbol/Function".to_string()),
            (valid_symbol, "note".to_string()),
        ] {
            let local = complete_search_response(
                "needle",
                vec![json!({"uid": uid, "kind": kind, "title": "needle"})],
            );
            let merged =
                merge_json_results(&local, &complete_search_response("needle", Vec::new()));
            assert_eq!(
                merged["total_matches_relation"], "gte",
                "invalid UID must remain unkeyed and cannot prove exactness: {local}"
            );
        }
    }

    #[test]
    fn merge_json_results_invalid_uids_do_not_inflate_the_proven_lower_bound() {
        let local = complete_search_response(
            "needle",
            vec![json!({
                "uid": "note:vlt:local:0123456789ab",
                "kind": "note",
                "title": "local"
            })],
        );
        let server = complete_search_response(
            "needle",
            vec![json!({
                "uid": "note:vlt:server:fedcba987654",
                "kind": "note",
                "title": "server"
            })],
        );

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["returned_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(
            merged["total_matches"], 1,
            "invalid presentation IDs cannot prove two distinct logical entities"
        );
    }

    #[test]
    fn merge_json_results_leading_zero_symbol_line_cannot_prove_or_inflate_union() {
        let canonical_uid = symbol_search_uid(
            "local",
            "https://github.com/acme/repo",
            "src/lib.rs",
            "needle",
            42,
        );
        let server_canonical_uid = symbol_search_uid(
            "server",
            "https://github.com/acme/repo",
            "src/lib.rs",
            "needle",
            42,
        );
        let malformed_uid = format!(
            "{}:042",
            server_canonical_uid
                .strip_suffix(":42")
                .expect("constructor must use canonical decimal line spelling")
        );
        let local = complete_search_response(
            "needle",
            vec![json!({
                "uid": canonical_uid,
                "kind": "Symbol/Function",
                "title": "canonical"
            })],
        );
        let server = complete_search_response(
            "needle",
            vec![json!({
                "uid": malformed_uid,
                "kind": "Symbol/Function",
                "title": "malformed"
            })],
        );

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["returned_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(
            merged["total_matches"], 1,
            "malformed line spelling cannot prove a second identity or inflate the lower bound"
        );
    }

    #[test]
    fn merge_json_results_discards_contradictory_equal_count_exact_metadata() {
        let duplicate = note_search_row("local", "/shared/vault", "notes/needle.md", "needle");
        let contradictory = json!({
            "query": "needle",
            "engine": "bm25",
            "total_matches": 2,
            "total_matches_relation": "eq",
            "returned_matches": 2,
            "truncated": true,
            "results": [duplicate.clone(), duplicate],
            "expansion_terms": [],
        });

        let merged = merge_json_results(
            &contradictory,
            &complete_search_response("needle", Vec::new()),
        );

        assert_eq!(merged["returned_matches"], 1);
        assert_eq!(merged["total_matches_relation"], "gte");
        assert_eq!(
            merged["total_matches"], 1,
            "contradictory source total must not outrank the one proven identity"
        );
    }

    #[test]
    fn merge_json_results_uses_uid_identity_for_concise_notes_and_tags() {
        let local = complete_search_response(
            "needle",
            vec![
                note_search_row(
                    "local-instance",
                    "/shared/vault",
                    "notes/needle.md",
                    "needle",
                ),
                tag_search_row("local-instance", "/shared/vault", "needle"),
            ],
        );
        let server = complete_search_response(
            "needle",
            vec![
                note_search_row(
                    "server-instance",
                    "/shared/vault",
                    "notes/needle.md",
                    "needle",
                ),
                tag_search_row("server-instance", "/shared/vault", "needle"),
            ],
        );

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 2);
        assert_eq!(merged["total_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "eq");
    }

    #[test]
    fn merge_json_results_keeps_same_path_symbols_from_distinct_repos() {
        let local = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "local",
                "https://github.com/acme/one",
                "src/lib.rs",
                "same",
                7,
            )],
        );
        let server = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "server",
                "https://github.com/acme/two",
                "src/lib.rs",
                "same",
                7,
            )],
        );

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 2);
        assert_eq!(merged["total_matches"], 2);
        assert_eq!(merged["total_matches_relation"], "eq");
    }

    #[test]
    fn merge_json_results_deduplicates_detailed_substring_notes_without_locations() {
        let mut local_note = note_search_row("local", "/shared/vault", "notes/needle.md", "needle");
        local_note["score"] = json!(1.0);
        local_note["matched_headings"] = json!(["needle heading"]);
        let mut server_note =
            note_search_row("server", "/shared/vault", "notes/needle.md", "needle");
        server_note["score"] = json!(1.0);
        server_note["matched_headings"] = json!(["needle heading"]);
        let local = complete_search_response("needle", vec![local_note]);
        let server = complete_search_response("needle", vec![server_note]);

        let merged = merge_json_results(&local, &server);

        assert_eq!(merged["results"].as_array().unwrap().len(), 1);
        assert_eq!(merged["total_matches"], 1);
        assert_eq!(merged["total_matches_relation"], "eq");
    }

    #[test]
    fn merge_json_results_keeps_same_title_note_and_symbol_distinct() {
        let local = complete_search_response(
            "needle",
            vec![note_search_row(
                "local",
                "/shared/vault",
                "notes/needle.md",
                "needle",
            )],
        );
        let server = complete_search_response(
            "needle",
            vec![symbol_search_row(
                "server",
                "https://github.com/acme/server",
                "src/lib.rs",
                "needle",
                7,
            )],
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
        let duplicate = note_search_row(
            "local-instance",
            "/shared/vault",
            "notes/needle.md",
            "needle",
        );
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

    /// Shape of a `brain_search` response as both tiers emit it today:
    /// keyword/BM25-only, so `semantic_applied: false` and no degradation.
    fn brain_search_tier(uid: &str, semantic_applied: bool, degraded: &[&str]) -> Value {
        json!({
            "query": "fn",
            "engine": "bm25",
            "results": [{"uid": uid, "score": 0.9}],
            "total_matches": 1,
            "total_matches_relation": "eq",
            "returned_matches": 1,
            "truncated": false,
            "semantic_applied": semantic_applied,
            "degraded_components": degraded,
        })
    }

    #[test]
    fn merge_carries_honesty_fields_when_both_tiers_are_keyword_only() {
        // The live case: both tiers are BM25-only. The merged response must
        // still SAY so — dropping the fields is the ambiguity they exist to
        // remove.
        let merged = merge_json_results(
            &brain_search_tier("a", false, &[]),
            &brain_search_tier("b", false, &[]),
        );
        assert_eq!(merged["semantic_applied"], json!(false));
        assert_eq!(merged["degraded_components"], json!([]));
    }

    #[test]
    fn merge_semantic_applied_is_conjunctive_across_tiers() {
        // One tier with a semantic leg and one without produces a merged row
        // set that is only partly semantically ranked; claiming `true` would
        // overclaim on the lexical half.
        let mixed = merge_json_results(
            &brain_search_tier("a", true, &[]),
            &brain_search_tier("b", false, &[]),
        );
        assert_eq!(mixed["semantic_applied"], json!(false));

        let mixed_other_way = merge_json_results(
            &brain_search_tier("a", false, &[]),
            &brain_search_tier("b", true, &[]),
        );
        assert_eq!(mixed_other_way["semantic_applied"], json!(false));

        // Only when EVERY contributing tier applied it is it true of the whole.
        let both = merge_json_results(
            &brain_search_tier("a", true, &[]),
            &brain_search_tier("b", true, &[]),
        );
        assert_eq!(both["semantic_applied"], json!(true));
    }

    #[test]
    fn merge_degraded_components_unions_and_dedupes() {
        // A component degraded in either tier is degraded in the merged answer.
        let merged = merge_json_results(
            &brain_search_tier("a", false, &["semantic", "reranker"]),
            &brain_search_tier("b", false, &["semantic", "expansion"]),
        );
        assert_eq!(
            merged["degraded_components"],
            json!(["semantic", "reranker", "expansion"]),
            "union in local-then-server encounter order, deduplicated"
        );
    }

    #[test]
    fn merge_cannot_claim_semantic_for_a_tier_that_did_not_report_it() {
        // Defensive only: not reachable for brain_search today, since both tiers
        // go through `dispatch_typed_brain_search` (which always emits both keys)
        // and proto3 scalars decode an older server to `false`/`[]` rather than
        // to an absent field. If a partial response ever does arrive, the merge
        // must not upgrade it to a claim — and must NOT invent a vocabulary term
        // in `degraded_components`, whose documented values are consumed by
        // agents as "retrieval fell back to lexical ranking".
        let mut partial = brain_search_tier("b", false, &[]);
        partial.as_object_mut().unwrap().remove("semantic_applied");

        let merged = merge_json_results(&brain_search_tier("a", true, &[]), &partial);
        assert_eq!(
            merged["semantic_applied"],
            json!(false),
            "a tier that did not report cannot be assumed to have applied it"
        );
        assert_eq!(
            merged["degraded_components"],
            json!([]),
            "an unreported field is not a degraded component; do not manufacture one"
        );
    }

    #[test]
    fn merge_omits_honesty_fields_when_no_tier_reported_them() {
        // Two tiers that both predate the fields (or a tool that never had
        // them). Synthesizing `false` here would manufacture a claim nobody
        // made and destroy the "not implemented / older server" signal that an
        // absent field carries.
        let local = json!({"results": [{"uid": "a", "score": 0.9}]});
        let server = json!({"results": [{"uid": "b", "score": 0.8}]});
        let merged = merge_json_results(&local, &server);
        assert!(merged.get("semantic_applied").is_none());
        assert!(merged.get("degraded_components").is_none());
    }

    /// Shape of a `brain_context` / `project_context` response: the structured
    /// `connected` envelope, which `merge_structured_results` rebuilds field by
    /// field. Unlike `brain_search` these tools have a real semantic leg, so
    /// `semantic_applied` can genuinely be `true` on either tier.
    fn brain_context_tier(uid: &str, semantic_applied: bool, degraded: &[&str]) -> Value {
        json!({
            "seeds": [{"uid": format!("seed-{uid}"), "title": "seed"}],
            "connected": [{"uid": uid, "title": uid, "score": 0.9}],
            "seeds_expanded": 1,
            "tokens_used": 100,
            "token_budget": 4000,
            "semantic_applied": semantic_applied,
            "degraded_components": degraded,
        })
    }

    #[test]
    fn merge_structured_carries_honesty_fields() {
        // The `connected`-schema path (brain_context / project_context) rebuilds
        // its envelope key by key, so these have to be re-added deliberately —
        // and they matter MORE here than on brain_search, because these tools
        // actually have a semantic leg.
        let merged = merge_structured_results(
            &brain_context_tier("a", true, &[]),
            &brain_context_tier("b", true, &[]),
        );
        assert_eq!(merged["connected"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            merged["semantic_applied"],
            json!(true),
            "both tiers applied a semantic leg, so the merged answer did"
        );
        assert_eq!(merged["degraded_components"], json!([]));
    }

    #[test]
    fn merge_structured_semantic_applied_is_conjunctive() {
        // Reachable in production on this path: two daemons need not agree on
        // whether an embedding model is loaded. The merged `connected` array
        // then contains purely lexical rows, so `true` would overclaim.
        for (local_semantic, server_semantic) in [(true, false), (false, true)] {
            let merged = merge_structured_results(
                &brain_context_tier("a", local_semantic, &[]),
                &brain_context_tier("b", server_semantic, &[]),
            );
            assert_eq!(
                merged["semantic_applied"],
                json!(false),
                "a mixed merge ({local_semantic}/{server_semantic}) must not claim semantic ranking"
            );
            // The mixed case is a loss of resolution, NOT a degradation: a tier
            // that never requested a semantic leg did not fall back from one.
            // Manufacturing a marker here would put a value into a vocabulary
            // whose only documented member is "semantic".
            assert_eq!(
                merged["degraded_components"],
                json!([]),
                "a mixed merge must not invent a degradation marker"
            );
        }
    }

    #[test]
    fn merge_structured_degraded_components_unions_and_dedupes() {
        // "semantic" is the only value in the documented vocabulary, and it is
        // genuinely reachable here: a tier that requested a semantic leg and
        // could not run it reports it, and that degradation must remain visible
        // in the merged answer even if the other tier was healthy.
        let merged = merge_structured_results(
            &brain_context_tier("a", false, &["semantic"]),
            &brain_context_tier("b", false, &["semantic"]),
        );
        assert_eq!(
            merged["degraded_components"],
            json!(["semantic"]),
            "deduplicated union, not a concatenation"
        );

        let one_sided = merge_structured_results(
            &brain_context_tier("a", true, &[]),
            &brain_context_tier("b", false, &["semantic"]),
        );
        assert_eq!(
            one_sided["degraded_components"],
            json!(["semantic"]),
            "a server-side degradation must not be hidden behind a healthy local tier"
        );
        assert_eq!(one_sided["semantic_applied"], json!(false));

        // Distinct per-tier values, so the assertion can actually tell a union
        // apart from "whatever the local tier said". A single-element expected
        // value cannot: it passes identically whether one tier or both reported.
        //
        // NOT VOCABULARY. `"reranker"` and `"expansion"` are deliberately
        // chosen because nothing in this codebase emits them: the merge is a
        // value-agnostic string union, and inputs no producer can generate are
        // what prove that, where reusing `"semantic"` twice would not. The
        // real, complete vocabulary is exactly one value, `"semantic"`, as
        // stated in `agent_guide`, README.md and docs/server-mode.md — if you
        // arrived here by grepping `degraded_components` for what values exist,
        // these two are not among them. (Same convention as the `brain_search`
        // sibling test `merge_degraded_components_unions_and_dedupes`, which
        // has used these names since before the structured merge carried these
        // fields; keeping them aligned beats two conventions in one file.)
        let distinct = merge_structured_results(
            &brain_context_tier("a", false, &["semantic"]),
            &brain_context_tier("b", false, &["reranker"]),
        );
        assert_eq!(
            distinct["degraded_components"],
            json!(["semantic", "reranker"]),
            "union of distinct per-tier values, in local-then-server order"
        );

        // Overlap plus a distinct value: dedup and order together.
        let overlapping = merge_structured_results(
            &brain_context_tier("a", false, &["semantic", "reranker"]),
            &brain_context_tier("b", false, &["reranker", "expansion"]),
        );
        assert_eq!(
            overlapping["degraded_components"],
            json!(["semantic", "reranker", "expansion"]),
            "deduplicated, first-seen order preserved across tiers"
        );
    }

    #[test]
    fn merge_structured_omits_honesty_fields_when_no_tier_reported_them() {
        // An older server (or a response predating the fields) must not be
        // upgraded into a manufactured `false`: on a merged response, absence
        // has to keep meaning "no tier reported".
        let mut local = brain_context_tier("a", false, &[]);
        let mut server = brain_context_tier("b", false, &[]);
        for tier in [&mut local, &mut server] {
            let obj = tier.as_object_mut().unwrap();
            obj.remove("semantic_applied");
            obj.remove("degraded_components");
        }
        let merged = merge_structured_results(&local, &server);
        assert!(
            merged.get("connected").is_some(),
            "still a structured merge"
        );
        assert!(merged.get("semantic_applied").is_none());
        assert!(merged.get("degraded_components").is_none());
    }
}

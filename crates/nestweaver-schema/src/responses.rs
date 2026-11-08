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

/// Same pin/UID guidance as [`impact_ambiguity_remedy`], without claiming
/// an impact walk ran. Used by flow-trace, cross-repo-contracts, and other
/// name lookups.
pub fn name_lookup_ambiguity_remedy(repo_filter: Option<&str>) -> String {
    match repo_filter {
        Some(repo) => format!(
            "the symbol name matched multiple symbols. \
             --repo {repo} is already set and every match is inside it, so it \
             cannot separate them. Instead, pass a UID or path::name — each \
             candidate below carries its UID and file path."
        ),
        None => "the symbol name matched multiple symbols. \
                 Disambiguate with --repo <name>, or pass a UID or path::name"
            .to_string(),
    }
}

/// Same candidate objects `impact` / `read-symbols` already list for exit 3.
pub fn name_lookup_candidates(candidates: Value) -> Vec<Value> {
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
    candidates
}

/// Render ambiguity candidates consistently regardless of whether the caller
/// supplied full Symbol records or an already reduced transport representation.
pub fn impact_ambiguous(symbol: &str, repo_filter: Option<&str>, candidates: Value) -> Value {
    let candidates = name_lookup_candidates(candidates);
    impact(json!({"status": "ambiguous", "symbol": symbol,
        "candidates": candidates, "note": impact_ambiguity_remedy(repo_filter)}))
}

/// Structured name-lookup refusal for tools that are not `brain_impact`.
///
/// Carries the same candidate listing as [`impact_ambiguous`] plus
/// `candidate_uids` so CLI/MCP clients can disambiguate without scraping.
pub fn name_lookup_ambiguous(symbol: &str, repo_filter: Option<&str>, candidates: Value) -> Value {
    let candidates = name_lookup_candidates(candidates);
    let candidate_uids: Vec<Value> = candidates
        .iter()
        .filter_map(|candidate| candidate.get("uid").cloned())
        .collect();
    json!({
        "status": "ambiguous",
        "error": "ambiguous",
        "symbol": symbol,
        "candidates": candidates,
        "candidate_uids": candidate_uids,
        "note": name_lookup_ambiguity_remedy(repo_filter),
    })
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
        // nw-557: every not-found envelope carries `message`.
        let symbol = object
            .get("symbol")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        object
            .entry("message")
            .or_insert_with(|| json!(format!("no symbol found: '{symbol}'")));
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

/// Presentation bounds for preflight ambiguity, independent of ranked-node caps.
pub const CONTEXT_AMBIGUITY_BYTES: usize = 20_000;
pub const CONTEXT_AMBIGUITY_TEXT_BYTES: usize = 4096;
pub const CONTEXT_AMBIGUITY_SEEDS_PER_TIER: usize = 256;
pub const CONTEXT_AMBIGUITY_CANDIDATES: usize = 50;

/// Validated response disclosure. The engine's ranking result deliberately does
/// not own preflight diagnostics. Keep the original follow-up handles and all
/// bounded additive fields while sharing validation across transport consumers.
#[derive(Debug, Default, Clone)]
pub struct ContextAmbiguityDisclosure {
    value: Value,
}

impl ContextAmbiguityDisclosure {
    pub fn from_response(response: &Value) -> Self {
        // A surviving single tier may have no per-diagnosis provenance yet.
        // Only explicit one-source transport metadata establishes its origin.
        let source = response["_meta"]["sources"].as_array().and_then(|sources| {
            if sources.len() == 1 {
                sources[0]
                    .as_str()
                    .filter(|s| matches!(*s, "local" | "server"))
            } else {
                None
            }
        });
        Self::normalize(&[(response, source)])
    }

    /// Scope observations to their actual producer, never canonicalize candidate
    /// UIDs using the ranked-row vault mappings or sum candidate populations.
    pub fn merge(local: &Value, server: &Value) -> Self {
        Self::normalize(&[(local, Some("local")), (server, Some("server"))])
    }

    pub fn entries(&self) -> &[Value] {
        self.value["ambiguous_seeds"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn output_budget(&self) -> Option<&Value> {
        self.value.get("output_budget")
    }

    pub fn stamp_into(&self, response: &mut Value) {
        if let (Some(target), Some(disclosure)) = (response.as_object_mut(), self.value.as_object())
        {
            // Own these fields completely: invalid/omitted raw diagnostics may
            // not survive beside the normalized non-echo disclosure.
            target.remove("ambiguous_seeds");
            target.remove("output_budget");
            for (key, value) in disclosure {
                target.insert(key.clone(), value.clone());
            }
        }
    }

    fn normalize(tiers: &[(&Value, Option<&str>)]) -> Self {
        let mut entries = Vec::new();
        let mut cuts = serde_json::Map::new();
        let mut prior_budget = serde_json::Map::new();
        for (response, source) in tiers {
            // Carry existing producer cuts once. A single-tier normalization is
            // idempotent; merge qualifies paths rather than adding their counts.
            if response
                .get("output_budget")
                .is_some_and(|value| !value.is_object())
            {
                ambiguity_cut(
                    &mut cuts,
                    "/ambiguous_seeds/metadata",
                    1,
                    "invalid_ambiguity_disclosure",
                );
            }
            if let Some(budget) = response.get("output_budget").and_then(Value::as_object) {
                for (key, value) in budget {
                    if key == "cuts" {
                        if let Some(prior) = value.as_object() {
                            for (path, cut) in prior {
                                let valid = cut.is_object()
                                    && (cut["items_omitted"].as_u64().is_some()
                                        || cut["bytes_omitted"].as_u64().is_some())
                                    && ["items_omitted", "bytes_omitted"].iter().all(|key| {
                                        cut.get(*key).is_none_or(|v| v.as_u64().is_some())
                                    })
                                    && ["reason", "count_kind"]
                                        .iter()
                                        .all(|key| cut.get(*key).is_none_or(Value::is_string))
                                    && ["paired_array", "mirrored_alias_path"].iter().all(|key| {
                                        cut.get(*key).is_none_or(|v| v.is_string() || v.is_null())
                                    })
                                    && cut.get("text_truncated").is_none_or(Value::is_boolean);
                                if let Some(cut) =
                                    valid.then(|| ambiguity_bounded_clone(cut)).flatten()
                                {
                                    let path = source.filter(|_| tiers.len() > 1).map_or_else(
                                        || path.clone(),
                                        |source| format!("/{source}{path}"),
                                    );
                                    if path.len() <= CONTEXT_AMBIGUITY_TEXT_BYTES && cuts.len() < 64
                                    {
                                        cuts.entry(path).or_insert(cut);
                                    } else {
                                        ambiguity_cut(
                                            &mut cuts,
                                            "/ambiguous_seeds/metadata",
                                            1,
                                            "metadata_omitted",
                                        );
                                    }
                                } else {
                                    ambiguity_cut(
                                        &mut cuts,
                                        "/ambiguous_seeds/metadata",
                                        1,
                                        "invalid_ambiguity_disclosure",
                                    );
                                }
                            }
                        } else {
                            ambiguity_cut(
                                &mut cuts,
                                "/ambiguous_seeds/metadata",
                                1,
                                "invalid_ambiguity_disclosure",
                            );
                        }
                    } else {
                        let valid = match key.as_str() {
                            "logical_bytes_limit"
                            | "escaped_result_bytes_limit"
                            | "ambiguity_bytes_limit" => value.as_u64().is_some(),
                            "cuts_complete" => value.is_boolean(),
                            "retry" => value.is_string(),
                            _ => true,
                        };
                        if valid
                            && key.len() <= CONTEXT_AMBIGUITY_TEXT_BYTES
                            && let Some(value) = ambiguity_bounded_clone(value)
                        {
                            prior_budget.entry(key.clone()).or_insert(value);
                        } else {
                            ambiguity_cut(
                                &mut cuts,
                                "/ambiguous_seeds/metadata",
                                1,
                                "invalid_ambiguity_disclosure",
                            );
                        }
                    }
                }
            }
            let Some(raw) = response.get("ambiguous_seeds") else {
                continue;
            };
            let scope = source.unwrap_or("local");
            let root = format!("/ambiguous_seeds.{scope}");
            let Some(raw) = raw.as_array() else {
                ambiguity_cut(&mut cuts, &root, 1, "invalid_ambiguity_disclosure");
                continue;
            };
            let tier_limit = if source.is_none() {
                // Already merged payloads carry per-entry producer identity.
                CONTEXT_AMBIGUITY_SEEDS_PER_TIER * 2
            } else {
                CONTEXT_AMBIGUITY_SEEDS_PER_TIER
            };
            let mut seen = Vec::new();
            let mut counts = [0usize; 2];
            for (index, raw_entry) in raw.iter().take(tier_limit).enumerate() {
                let provenance = source.or_else(|| raw_entry["_provenance"].as_str());
                let scope = provenance
                    .filter(|s| matches!(*s, "local" | "server"))
                    .unwrap_or("local");
                let count = &mut counts[usize::from(scope == "server")];
                *count += 1;
                let path = format!("/ambiguous_seeds.{scope}/{index}");
                if *count > CONTEXT_AMBIGUITY_SEEDS_PER_TIER {
                    ambiguity_cut(
                        &mut cuts,
                        &format!("/ambiguous_seeds.{scope}"),
                        1,
                        "diagnoses_omitted",
                    );
                    continue;
                }
                let Some(object) = raw_entry.as_object() else {
                    ambiguity_cut(&mut cuts, &path, 1, "invalid_ambiguity_disclosure");
                    continue;
                };
                let valid_seed = raw_entry["seed"].as_str().is_some_and(|s| !s.is_empty());
                let candidates = raw_entry["candidates"].as_array();
                let uids = raw_entry["candidate_uids"].as_array();
                let valid_fields = ["symbol", "title", "status", "error", "note", "entity_kind"]
                    .iter()
                    .all(|key| raw_entry.get(*key).is_none_or(Value::is_string))
                    && ["candidate_total", "candidates_returned"]
                        .iter()
                        .all(|key| raw_entry.get(*key).is_none_or(|v| v.as_u64().is_some()))
                    && raw_entry.get("truncated").is_none_or(Value::is_boolean);
                if !valid_seed || !valid_fields || candidates.is_none() || uids.is_none() {
                    ambiguity_cut(&mut cuts, &path, 1, "invalid_ambiguity_disclosure");
                    continue;
                }
                let (Some(candidates), Some(uids)) = (candidates, uids) else {
                    continue;
                };
                if candidates.len() != uids.len()
                    || raw_entry["candidates_returned"]
                        .as_u64()
                        .is_some_and(|n| n != candidates.len() as u64)
                    || raw_entry["candidate_total"]
                        .as_u64()
                        .is_some_and(|total| total < candidates.len() as u64)
                {
                    ambiguity_cut(&mut cuts, &path, 1, "invalid_ambiguity_disclosure");
                    continue;
                }
                let mut entry = serde_json::Map::new();
                let mut valid = true;
                for (key, value) in object {
                    if matches!(
                        key.as_str(),
                        "candidates" | "candidate_uids" | "_provenance"
                    ) {
                        continue;
                    }
                    if key.len() > CONTEXT_AMBIGUITY_TEXT_BYTES {
                        valid = false;
                        break;
                    }
                    match ambiguity_bounded_clone(value) {
                        Some(value) => {
                            entry.insert(key.clone(), value);
                        }
                        None => {
                            valid = false;
                            break;
                        }
                    }
                }
                if !valid {
                    ambiguity_cut(&mut cuts, &path, 1, "indivisible_text_or_entry_omitted");
                    continue;
                }
                if let Some(source) = provenance.filter(|s| matches!(*s, "local" | "server")) {
                    entry.insert("_provenance".into(), json!(source));
                }
                let mut kept = Vec::new();
                let mut candidate_cut_reason = "candidates_omitted";
                for (candidate, uid) in candidates
                    .iter()
                    .zip(uids)
                    .take(CONTEXT_AMBIGUITY_CANDIDATES)
                {
                    if !candidate.is_object()
                        || !uid.as_str().is_some_and(|s| !s.is_empty())
                        || candidate.get("uid") != Some(uid)
                        || !candidate.get("file_path").is_some_and(Value::is_string)
                        || ["name", "kind"].iter().any(|key| {
                            candidate
                                .get(*key)
                                .is_some_and(|v| !v.is_string() && !v.is_null())
                        })
                        || candidate
                            .get("start_line")
                            .is_some_and(|v| !v.is_null() && v.as_u64().is_none())
                    {
                        candidate_cut_reason = "invalid_ambiguity_disclosure";
                        break;
                    }
                    let Some(candidate) = ambiguity_bounded_clone(candidate) else {
                        break;
                    };
                    kept.push(candidate);
                }
                let omitted = candidates.len() - kept.len();
                if omitted > 0 {
                    // One cut per completed prefix, not one per paired array.
                    let cut_path = format!("{path}/candidates");
                    ambiguity_cut(&mut cuts, &cut_path, omitted, candidate_cut_reason);
                }
                entry.insert(
                    "candidate_uids".into(),
                    json!(kept.iter().map(|v| &v["uid"]).collect::<Vec<_>>()),
                );
                entry.insert("candidates_returned".into(), json!(kept.len()));
                if omitted > 0 {
                    entry.insert("truncated".into(), json!(true));
                }
                entry.insert("candidates".into(), Value::Array(kept));
                let entry = Value::Object(entry);
                if !seen.contains(&entry) {
                    seen.push(entry.clone());
                    entries.push(entry);
                }
            }
            if raw.len() > tier_limit {
                ambiguity_cut(
                    &mut cuts,
                    &root,
                    raw.len() - tier_limit,
                    "diagnoses_omitted",
                );
            }
        }
        // Bounded accounting metadata has priority over displayed candidates.
        // Repeatedly remove a whole suffix candidate/entry; identities are atomic.
        loop {
            let value = ambiguity_payload(&entries, &cuts, &prior_budget);
            if ambiguity_escaped_size(&value) <= CONTEXT_AMBIGUITY_BYTES {
                return Self { value };
            }
            if !entries.is_empty() {
                let index = entries.len() - 1;
                let scope = entries[index]["_provenance"]
                    .as_str()
                    .unwrap_or("local")
                    .to_string();
                let entry = &mut entries[index];
                if index == 0
                    && entry["candidates"]
                        .as_array()
                        .is_some_and(|c| !c.is_empty())
                {
                    if let Some(candidates) = entry["candidates"].as_array_mut() {
                        candidates.pop();
                    }
                    if let Some(uids) = entry["candidate_uids"].as_array_mut() {
                        uids.pop();
                    }
                    entry["candidates_returned"] =
                        json!(entry["candidates"].as_array().map_or(0, Vec::len));
                    entry["truncated"] = json!(true);
                    ambiguity_cut(
                        &mut cuts,
                        &format!("/ambiguous_seeds.{scope}/{index}/candidates"),
                        1,
                        "candidates_omitted",
                    );
                } else {
                    entries.pop();
                    ambiguity_cut(
                        &mut cuts,
                        &format!("/ambiguous_seeds.{scope}"),
                        1,
                        "diagnoses_omitted",
                    );
                }
            } else if !prior_budget.is_empty() {
                prior_budget.clear();
                ambiguity_cut(
                    &mut cuts,
                    "/ambiguous_seeds/metadata",
                    1,
                    "metadata_omitted",
                );
            } else {
                // Fold oversized producer cut detail into an explicit count;
                // do not claim that detailed paths survived this presentation.
                let omitted = cuts.len();
                cuts.clear();
                ambiguity_cut(
                    &mut cuts,
                    "/ambiguous_seeds/metadata",
                    omitted,
                    "cut_details_omitted",
                );
            }
        }
    }
}

fn ambiguity_payload(
    entries: &[Value],
    cuts: &serde_json::Map<String, Value>,
    prior: &serde_json::Map<String, Value>,
) -> Value {
    let mut payload = serde_json::Map::new();
    if !entries.is_empty() {
        payload.insert("ambiguous_seeds".into(), json!(entries));
    }
    if !cuts.is_empty() || !prior.is_empty() {
        let mut budget = prior.clone();
        if !cuts.is_empty() {
            budget.insert("cuts".into(), json!(cuts));
            budget.insert(
                "ambiguity_bytes_limit".into(),
                json!(CONTEXT_AMBIGUITY_BYTES),
            );
        }
        payload.insert("output_budget".into(), Value::Object(budget));
    }
    Value::Object(payload)
}

fn ambiguity_cut(
    cuts: &mut serde_json::Map<String, Value>,
    path: &str,
    omitted: usize,
    reason: &str,
) {
    // Keep the accounting itself bounded even for hostile response objects.
    let path = if cuts.len() < 64 || cuts.contains_key(path) {
        path
    } else if path.contains("server") {
        "/ambiguous_seeds.server/other_cuts"
    } else {
        "/ambiguous_seeds.local/other_cuts"
    };
    let cut = cuts.entry(path.to_string()).or_insert_with(
        || json!({"items_omitted":0,"count_kind":"exact_completed_array","reason":reason}),
    );
    if reason == "invalid_ambiguity_disclosure" {
        cut["reason"] = json!(reason);
    }
    let prior = cut["items_omitted"].as_u64().unwrap_or(0);
    cut["items_omitted"] = json!(prior.saturating_add(omitted as u64));
    if path.ends_with("/candidates") {
        cut["paired_array"] = json!("candidate_uids");
    }
}

fn ambiguity_bounded_clone(value: &Value) -> Option<Value> {
    fn safe(value: &Value, depth: usize) -> bool {
        if depth > 32 {
            return false;
        }
        match value {
            Value::String(s) => s.len() <= CONTEXT_AMBIGUITY_TEXT_BYTES,
            Value::Array(a) => a.len() <= 512 && a.iter().all(|v| safe(v, depth + 1)),
            Value::Object(o) => {
                o.len() <= 64
                    && o.iter()
                        .all(|(k, v)| k.len() <= CONTEXT_AMBIGUITY_TEXT_BYTES && safe(v, depth + 1))
            }
            _ => true,
        }
    }
    (safe(value, 0) && ambiguity_escaped_size(value) <= CONTEXT_AMBIGUITY_BYTES)
        .then(|| value.clone())
}

fn ambiguity_escaped_size(value: &Value) -> usize {
    struct Size(usize);
    impl std::io::Write for Size {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len()).saturating_add(
                bytes
                    .windows(3)
                    .filter(|b| *b == [0xe2, 0x80, 0xa8] || *b == [0xe2, 0x80, 0xa9])
                    .count()
                    * 3,
            );
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut size = Size(0);
    let _ = serde_json::to_writer(&mut size, value);
    size.0
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

    #[test]
    fn name_lookup_ambiguous_does_not_claim_an_impact_walk() {
        let payload = name_lookup_ambiguous(
            "ping",
            None,
            json!([{"uid":"sym:a","name":"ping","file_path":"a.py","start_line":1}]),
        );
        let note = payload["note"].as_str().unwrap_or_default();
        assert!(
            !note.contains("impact"),
            "flow-trace/cross-repo-contracts reuse this note: {note}"
        );
        assert_eq!(note, name_lookup_ambiguity_remedy(None));
        assert_ne!(note, impact_ambiguity_remedy(None));
    }
}

#[cfg(test)]
mod context_ambiguity_tests {
    use super::*;

    fn diagnosis(seed: &str, n: usize) -> Value {
        let candidates: Vec<_> = (0..n).map(|i| json!({"uid":format!("note:{i}"),"name":"Same","file_path":format!("{i}/Same.md"),"start_line":0})).collect();
        let mut d = name_lookup_ambiguous(seed, None, json!(candidates));
        d["seed"] = json!(seed);
        d["candidate_total"] = json!(n);
        d["candidates_returned"] = json!(n);
        d["truncated"] = json!(false);
        d
    }

    fn payload(disclosure: &ContextAmbiguityDisclosure) -> Value {
        let mut result = json!({});
        disclosure.stamp_into(&mut result);
        result
    }

    #[test]
    fn context_ambiguity_preserves_additive_fields_and_producer_cuts_idempotently() {
        let mut d = diagnosis("Same", 2);
        d["future"] = json!({"explanation":"kept"});
        let wire = json!({"ambiguous_seeds":[d],"output_budget":{"cuts":{"/connected":{"items_omitted":3,"count_kind":"exact_completed_array"}},"cuts_complete":true}});
        let once = payload(&ContextAmbiguityDisclosure::from_response(&wire));
        assert_eq!(once["ambiguous_seeds"], wire["ambiguous_seeds"]);
        assert_eq!(once["output_budget"]["cuts"], wire["output_budget"]["cuts"]);
        assert_eq!(
            payload(&ContextAmbiguityDisclosure::from_response(&once)),
            once
        );
        assert_eq!(
            payload(&ContextAmbiguityDisclosure::from_response(&json!({}))),
            json!({})
        );
    }

    #[test]
    fn context_ambiguity_atomic_handles_and_paired_candidate_cap() {
        let d = diagnosis("Same", 55);
        let result = payload(&ContextAmbiguityDisclosure::from_response(
            &json!({"ambiguous_seeds":[d]}),
        ));
        let d = &result["ambiguous_seeds"][0];
        assert_eq!(d["candidate_total"], 55);
        assert_eq!(d["candidates_returned"], 50);
        assert_eq!(d["truncated"], true);
        assert_eq!(
            d["candidate_uids"],
            json!(
                d["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| &v["uid"])
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(
            result["output_budget"]["cuts"]["/ambiguous_seeds.local/0/candidates"]["items_omitted"],
            5
        );
        let mut d = diagnosis("Same", 2);
        d["candidates"][0]["uid"] = json!("x".repeat(4097));
        d["candidate_uids"][0] = d["candidates"][0]["uid"].clone();
        let result = payload(&ContextAmbiguityDisclosure::from_response(
            &json!({"ambiguous_seeds":[d]}),
        ));
        assert_eq!(result["ambiguous_seeds"][0]["candidates_returned"], 0);
        assert!(!result.to_string().contains(&"x".repeat(4097)));
        assert!(result["output_budget"]["cuts"].is_object());
    }

    #[test]
    fn context_ambiguity_invalid_data_is_non_echo_and_does_not_replace_ranked_success() {
        let private = "private-invalid-object";
        let wire = json!({"connected":[{"uid":"note:valid"}],"ambiguous_seeds":[{"seed":{"secret":private}}]});
        let mut result = wire.clone();
        ContextAmbiguityDisclosure::from_response(&wire).stamp_into(&mut result);
        assert_eq!(result["connected"], wire["connected"]);
        assert!(!result.to_string().contains(private));
        assert_eq!(
            result["output_budget"]["cuts"]["/ambiguous_seeds.local/0"]["reason"],
            "invalid_ambiguity_disclosure"
        );
    }

    #[test]
    fn context_ambiguity_malformed_prior_cut_cannot_panic_or_echo() {
        let wire = json!({"ambiguous_seeds":[diagnosis("Same",55)],
            "output_budget":{"cuts":{"/ambiguous_seeds.local/0/candidates":"private-invalid-object"}}});
        let result = payload(&ContextAmbiguityDisclosure::from_response(&wire));
        assert_eq!(result["ambiguous_seeds"][0]["candidates_returned"], 50);
        assert!(!result.to_string().contains("private-invalid-object"));
        assert!(result.to_string().contains("invalid_ambiguity_disclosure"));
        let mut d = diagnosis("Same", 1);
        d["candidates"][0]["file_path"] = json!({"secret":"private-invalid-object"});
        let wire = json!({"ambiguous_seeds":[d],"output_budget":{"cuts":{
            "/ambiguous_seeds.local/0/candidates":{"items_omitted":5,"count_kind":"exact_completed_array","reason":"candidates_omitted"}}}});
        let result = payload(&ContextAmbiguityDisclosure::from_response(&wire));
        let cut = &result["output_budget"]["cuts"]["/ambiguous_seeds.local/0/candidates"];
        assert_eq!(cut["items_omitted"], 6);
        assert_eq!(cut["reason"], "invalid_ambiguity_disclosure");
        assert!(!result.to_string().contains("private-invalid-object"));
        for field in ["reason", "count_kind", "items_omitted"] {
            let mut cut = json!({"items_omitted":5,"count_kind":"exact_completed_array"});
            cut[field] = json!({"secret":"private-invalid-object"});
            let response =
                json!({"output_budget":{"cuts":{"/ambiguous_seeds.local/0/candidates":cut}}});
            let result = payload(&ContextAmbiguityDisclosure::from_response(&response));
            assert!(result.to_string().contains("invalid_ambiguity_disclosure"));
            assert!(!result.to_string().contains("private-invalid-object"));
        }
    }

    #[test]
    fn context_ambiguity_rejects_malformed_known_fields_without_echo() {
        for field in [
            "candidate_total",
            "candidates_returned",
            "truncated",
            "file_path",
        ] {
            let mut d = diagnosis("Same", 1);
            if field == "file_path" {
                d["candidates"][0][field] = json!({"secret":"private-invalid-object"});
            } else {
                d[field] = json!({"secret":"private-invalid-object"});
            }
            let mut response = json!({"connected":[{"uid":"note:valid"}],"ambiguous_seeds":[d]});
            let disclosure = ContextAmbiguityDisclosure::from_response(&response);
            disclosure.stamp_into(&mut response);
            assert_eq!(response["connected"][0]["uid"], "note:valid");
            assert!(
                !response.to_string().contains("private-invalid-object"),
                "{response}"
            );
            assert!(
                response
                    .to_string()
                    .contains("invalid_ambiguity_disclosure"),
                "{response}"
            );
        }
    }

    #[test]
    fn context_ambiguity_accumulates_only_new_omissions_after_a_producer_cut() {
        let mut d = diagnosis("Same", 55);
        d["candidate_total"] = json!(60);
        d["truncated"] = json!(true);
        let wire = json!({"ambiguous_seeds":[d],"output_budget":{"cuts":{
            "/ambiguous_seeds.local/0/candidates":{"items_omitted":5,"count_kind":"exact_completed_array"}}}});
        let result = payload(&ContextAmbiguityDisclosure::from_response(&wire));
        assert_eq!(result["ambiguous_seeds"][0]["candidates_returned"], 50);
        assert_eq!(
            result["output_budget"]["cuts"]["/ambiguous_seeds.local/0/candidates"]["items_omitted"],
            10
        );
        assert_eq!(
            payload(&ContextAmbiguityDisclosure::from_response(&result)),
            result
        );
    }

    #[test]
    fn context_ambiguity_escaped_aggregate_boundary_includes_metadata() {
        let d = diagnosis("Same", 2);
        // Construct exact escaped sizes using bounded additive text leaves;
        // the final one-byte increase must cut atomically and disclose it.
        let mut wire = json!({"ambiguous_seeds":[d]});
        for i in 0..4 {
            wire["ambiguous_seeds"][0][format!("padding{i}")] = json!("\"".repeat(2000));
        }
        let base = ambiguity_escaped_size(&wire);
        assert!(base < 20_000);
        wire["ambiguous_seeds"][0]["padding_last"] = json!("a".repeat(20_000 - base - 18));
        // Account exactly for the added key/JSON punctuation rather than
        // assuming character length equals escaped serialization size.
        let size = ambiguity_escaped_size(&wire);
        let text = wire["ambiguous_seeds"][0]["padding_last"]
            .as_str()
            .unwrap()
            .to_string();
        let adjusted = text.len() + 20_000 - size;
        wire["ambiguous_seeds"][0]["padding_last"] = json!("a".repeat(adjusted));
        assert_eq!(ambiguity_escaped_size(&wire), 20_000);
        let exact = payload(&ContextAmbiguityDisclosure::from_response(&wire));
        assert_eq!(exact, wire);
        wire["ambiguous_seeds"][0]["padding_last"] = json!("a".repeat(adjusted + 1));
        assert_eq!(ambiguity_escaped_size(&wire), 20_001);
        let cut = payload(&ContextAmbiguityDisclosure::from_response(&wire));
        assert!(ambiguity_escaped_size(&cut) <= 20_000);
        assert!(cut["output_budget"]["cuts"].is_object());
    }

    #[test]
    fn context_ambiguity_tier_bounds_source_handles_and_duplicate_observations() {
        let d = diagnosis("Same", 2);
        let local = json!({"ambiguous_seeds":[d.clone(),d.clone()]});
        let mut remote = d.clone();
        remote["candidates"][0]["uid"] = json!("note:remote");
        remote["candidate_uids"][0] = json!("note:remote");
        let merged =
            ContextAmbiguityDisclosure::merge(&local, &json!({"ambiguous_seeds":[remote.clone()]}));
        assert_eq!(merged.entries().len(), 2);
        assert_eq!(merged.entries()[0]["_provenance"], "local");
        assert_eq!(merged.entries()[1]["_provenance"], "server");
        assert_eq!(
            merged.entries()[1]["candidate_uids"],
            remote["candidate_uids"]
        );
        let raw: Vec<_> = (0..257)
            .map(|i| diagnosis(&format!("Same{i}"), 0))
            .collect();
        let bounded = payload(&ContextAmbiguityDisclosure::from_response(
            &json!({"ambiguous_seeds":raw}),
        ));
        assert!(bounded["ambiguous_seeds"].as_array().map_or(0, Vec::len) <= 256);
        assert!(ambiguity_escaped_size(&bounded) <= 20_000);
        assert!(bounded["output_budget"]["cuts"].is_object());
    }
}

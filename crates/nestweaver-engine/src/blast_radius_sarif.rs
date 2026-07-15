// Serialize a `BlastRadiusResult` to SARIF v2.1.0 so the trust signals render
// in GitHub code scanning, Azure DevOps, and the VS Code SARIF viewer.
//
// SARIF models tools, results, and locations well but has no vocabulary for the
// blast-radius trust core (gate state, coverage, blind spots, analysis
// direction). Those are carried as namespaced `nestweaver/*` extensions under
// `run.properties` and per-result `properties`, which SARIF explicitly permits
// via property bags. The conversion is pure and side-effect free.

use serde_json::{Value, json};

use crate::blast_radius::{BlastRadiusResult, BlindSpot, NotificationLevel};
use crate::process::RiskLevel;
use crate::signature_diff::{BreakTier, BreakingChange};

/// Static metadata for every [`BlindSpot`] variant: (kebab id, rule name,
/// short description). One `reportingDescriptor` (SARIF rule) is emitted per
/// variant so a viewer can resolve `nw/blind-spot/<kebab>` rule references.
const BLIND_SPOT_META: [(&str, &str, &str); 6] = [
    (
        "dynamic-dispatch",
        "DynamicDispatch",
        "Calls resolved at runtime (trait objects, virtual dispatch) are invisible to static traversal",
    ),
    (
        "reflection",
        "Reflection",
        "Reflective or runtime metaprogramming call sites are not followed",
    ),
    (
        "config-wiring",
        "ConfigWiring",
        "Dependencies wired through configuration or dependency injection are not traced",
    ),
    (
        "codegen",
        "Codegen",
        "Generated code that is not indexed can hide real dependents",
    ),
    (
        "pruned-below-threshold",
        "PrunedBelowThreshold",
        "Traversal was cut short by depth or score threshold — dependents may exist beyond the reported set",
    ),
    (
        "not-indexed",
        "NotIndexed",
        "A repo referenced by the change is not indexed, so its impact on it is unknown",
    ),
];

/// Map a [`NotificationLevel`] to its SARIF `level` string.
fn notification_level(level: NotificationLevel) -> &'static str {
    match level {
        NotificationLevel::Error => "error",
        NotificationLevel::Warning => "warning",
        NotificationLevel::Note => "note",
    }
}

/// Map a [`RiskLevel`] to a lowercase label for the namespaced extension.
fn risk_level_label(level: RiskLevel) -> &'static str {
    match level {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    }
}

/// The kebab-case serde representation of a [`BlindSpot`] (e.g. `not-indexed`).
fn blind_spot_kebab(bs: &BlindSpot) -> String {
    serde_json::to_value(bs)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Build a SARIF `physicalLocation` for a file and optional line. The SARIF
/// `region.startLine` is 1-based, so a missing/zero line clamps up to 1.
fn physical_location(uri: &str, start_line: i64) -> Value {
    json!({
        "physicalLocation": {
            "artifactLocation": { "uri": uri },
            "region": { "startLine": start_line.max(1) }
        }
    })
}

/// Convert a [`BlastRadiusResult`] into a single-run SARIF v2.1.0 document.
///
/// Standard SARIF carries the tool, its rules, the invocation status +
/// notifications, and one result per affected/cross-repo symbol. The
/// blast-radius trust core (gate state, coverage, blind spots, analysis
/// direction) rides along in namespaced `nestweaver/*` property bags.
pub fn blast_radius_to_sarif(result: &BlastRadiusResult, tool_version: &str) -> Value {
    // ── Rules (reportingDescriptors) ──────────────────────────────────────
    let mut rules: Vec<Value> = BLIND_SPOT_META
        .iter()
        .map(|(kebab, name, desc)| {
            json!({
                "id": format!("nw/blind-spot/{kebab}"),
                "name": name,
                "shortDescription": { "text": desc }
            })
        })
        .collect();
    rules.push(json!({
        "id": "nw/affected",
        "name": "AffectedSymbol",
        "shortDescription": { "text": "A symbol that may be affected by the change" }
    }));
    rules.push(json!({
        "id": "nw/org-impact",
        "name": "CrossRepoImpact",
        "shortDescription": { "text": "A symbol in another repo that may be affected by the change" }
    }));

    // ── Invocation: status + tool execution notifications ─────────────────
    let notifications: Vec<Value> = result
        .notifications
        .iter()
        .map(|n| {
            json!({
                "level": notification_level(n.level),
                "message": { "text": n.message },
                "descriptor": { "id": n.descriptor }
            })
        })
        .collect();
    let execution_successful = result.status != crate::blast_radius::AnalysisStatus::Failed;

    // ── Results: one per affected symbol, plus cross-repo impact items ────
    let mut results: Vec<Value> = Vec::new();
    for sym in &result.affected_symbols {
        // Anchor the region at the symbol's real start line; `physical_location`
        // clamps a missing/zero line up to the SARIF-required minimum of 1.
        let rank = (sym.impact_score * 100.0).clamp(0.0, 100.0);
        results.push(json!({
            "ruleId": "nw/affected",
            "kind": "review",
            "level": "note",
            "rank": rank,
            "message": {
                "text": format!(
                    "Affected by change via {} (depth {}): {}",
                    sym.edge_type, sym.depth, sym.name
                )
            },
            "locations": [ physical_location(&sym.file_path, sym.start_line as i64) ],
            "properties": {
                "nestweaver/edgeType": sym.edge_type,
                "nestweaver/impactScore": sym.impact_score,
                "nestweaver/repoUid": sym.repo_uid,
                "nestweaver/severitySource": "reach-only"
            }
        }));
    }

    if let Some(org) = &result.org_wide {
        let mut push_item = |item: &crate::blast_radius::OrgImpactItem, level: &str| {
            results.push(json!({
                "ruleId": "nw/org-impact",
                "kind": "review",
                "level": level,
                "message": {
                    "text": format!("{} — {}", item.reason, item.affected_name)
                },
                "locations": [
                    physical_location(&item.affected_file, item.affected_line as i64)
                ],
                "properties": {
                    "nestweaver/affectedRepo": item.affected_repo,
                    "nestweaver/severitySource": "reach-only",
                    "nestweaver/severity": item.severity
                }
            }));
        };
        for item in &org.breaking {
            push_item(item, "error");
        }
        for item in &org.warnings {
            push_item(item, "warning");
        }
        for item in &org.info {
            push_item(item, "note");
        }
    }

    // ── Namespaced extensions (the trust core SARIF can't model) ──────────
    let blind_spots: Vec<String> = result.blind_spots.iter().map(blind_spot_kebab).collect();
    let run_properties = json!({
        "nestweaver/analysisDirection": result.analysis_direction,
        "nestweaver/gateState": serde_json::to_value(result.gate_state).unwrap_or(Value::Null),
        "nestweaver/status": serde_json::to_value(result.status).unwrap_or(Value::Null),
        "nestweaver/riskLevel": risk_level_label(result.risk_level),
        "nestweaver/blindSpots": blind_spots,
        "nestweaver/coverage": serde_json::to_value(&result.coverage).unwrap_or(Value::Null),
        "nestweaver/summary": result.summary,
    });

    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "nestweaver-blast-radius",
                        "version": tool_version,
                        "informationUri": "https://github.com/Kehl-io/nestweaver",
                        "rules": rules
                    }
                },
                "invocations": [
                    {
                        "executionSuccessful": execution_successful,
                        "toolExecutionNotifications": notifications
                    }
                ],
                "results": results,
                "properties": run_properties
            }
        ]
    })
}

/// Map a [`BreakTier`] to its SARIF result `level`: `Breaking` is an error,
/// `LikelyBreaking` a warning, `ReachOnly` a note.
fn break_tier_level(tier: BreakTier) -> &'static str {
    match tier {
        BreakTier::Breaking => "error",
        BreakTier::LikelyBreaking => "warning",
        BreakTier::ReachOnly => "note",
    }
}

/// Append contract-verified breaking-change results (and their rule) to an
/// already-built SARIF document from [`blast_radius_to_sarif`].
///
/// Each result rides under a new `nw/contract-break` rule and carries
/// `properties["nestweaver/severitySource"] = "contract-verified"`, contrasting
/// with the `"reach-only"` affected/org items so a viewer can tell a verified
/// signature break from a reach-based heuristic. A no-op when `breaks` is empty.
pub fn append_contract_breaks_to_sarif(sarif: &mut Value, breaks: &[BreakingChange]) {
    if breaks.is_empty() {
        return;
    }
    let run = &mut sarif["runs"][0];
    if let Some(rules) = run["tool"]["driver"]["rules"].as_array_mut() {
        rules.push(json!({
            "id": "nw/contract-break",
            "name": "ContractBreak",
            "shortDescription": {
                "text": "A contract-verified breaking change to a public API symbol"
            }
        }));
    }
    if let Some(results) = run["results"].as_array_mut() {
        for b in breaks {
            results.push(json!({
                "ruleId": "nw/contract-break",
                "kind": "review",
                "level": break_tier_level(b.tier),
                "message": { "text": b.detail },
                "properties": {
                    "nestweaver/severitySource": "contract-verified",
                    "nestweaver/breakKind": serde_json::to_value(b.kind).unwrap_or(Value::Null),
                    "nestweaver/breakTier": serde_json::to_value(b.tier).unwrap_or(Value::Null),
                    "nestweaver/symbolName": b.symbol_name,
                    "nestweaver/symbolUid": b.symbol_uid
                }
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blast_radius::{
        AffectedSymbol, AnalysisStatus, BlastRadiusResult, Coverage, GateState, Notification,
        NotificationLevel, OrgImpactItem, OrgWideImpact,
    };
    use crate::process::RiskLevel;
    use crate::signature_diff::{BreakKind, BreakTier, BreakingChange};

    /// A minimal, complete result with no affected symbols or notifications.
    fn base_result() -> BlastRadiusResult {
        BlastRadiusResult {
            changed_symbols: vec![],
            affected_symbols: vec![],
            affected_clusters: vec![],
            risk_level: RiskLevel::Low,
            summary: "0 changed, 0 affected".to_string(),
            org_wide: None,
            status: AnalysisStatus::Complete,
            notifications: vec![],
            gate_state: GateState::Ok,
            coverage: Coverage::default(),
            blind_spots: vec![BlindSpot::DynamicDispatch],
            analysis_direction: "over-approximate".to_string(),
        }
    }

    fn affected(name: &str, file: &str, impact: f64) -> AffectedSymbol {
        AffectedSymbol {
            uid: format!("sym:{name}"),
            name: name.to_string(),
            file_path: file.to_string(),
            kind: "Function".to_string(),
            depth: 1,
            edge_type: "Calls".to_string(),
            confidence: 0.9,
            start_line: 42,
            impact_score: impact,
            repo_uid: "repo:1".to_string(),
        }
    }

    #[test]
    fn sarif_has_valid_envelope() {
        let sarif = blast_radius_to_sarif(&base_result(), "9.9.9");
        assert_eq!(sarif["version"], "2.1.0");
        assert!(sarif["$schema"].is_string());
        assert_eq!(
            sarif["runs"][0]["tool"]["driver"]["name"],
            "nestweaver-blast-radius"
        );
        assert_eq!(sarif["runs"][0]["tool"]["driver"]["version"], "9.9.9");
        let rules = sarif["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .expect("rules array");
        assert!(!rules.is_empty(), "rules must be non-empty");
    }

    #[test]
    fn sarif_execution_successful_reflects_status() {
        let complete = blast_radius_to_sarif(&base_result(), "1.0.0");
        assert_eq!(
            complete["runs"][0]["invocations"][0]["executionSuccessful"],
            true
        );

        let mut failed = base_result();
        failed.status = AnalysisStatus::Failed;
        let sarif = blast_radius_to_sarif(&failed, "1.0.0");
        assert_eq!(
            sarif["runs"][0]["invocations"][0]["executionSuccessful"],
            false
        );
    }

    #[test]
    fn sarif_notifications_mapped() {
        let mut result = base_result();
        result.notifications = vec![
            Notification {
                level: NotificationLevel::Error,
                message: "boom".to_string(),
                descriptor: "store.impact-failed".to_string(),
            },
            Notification {
                level: NotificationLevel::Warning,
                message: "drift".to_string(),
                descriptor: "changed-file-no-symbols".to_string(),
            },
        ];
        let sarif = blast_radius_to_sarif(&result, "1.0.0");
        let notes = sarif["runs"][0]["invocations"][0]["toolExecutionNotifications"]
            .as_array()
            .expect("notifications array");
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0]["level"], "error");
        assert_eq!(notes[0]["descriptor"]["id"], "store.impact-failed");
        assert_eq!(notes[1]["level"], "warning");
        assert_eq!(notes[1]["descriptor"]["id"], "changed-file-no-symbols");
    }

    #[test]
    fn sarif_affected_symbol_becomes_result_with_rank() {
        let mut result = base_result();
        result.affected_symbols = vec![affected("fn_b", "src/b.rs", 0.9)];
        let sarif = blast_radius_to_sarif(&result, "1.0.0");
        let results = sarif["runs"][0]["results"].as_array().expect("results");
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r["ruleId"], "nw/affected");
        let rank = r["rank"].as_f64().expect("rank");
        assert!(
            (rank - 90.0).abs() < 1e-6,
            "expected rank ~90.0, got {rank}"
        );
        assert_eq!(
            r["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "src/b.rs"
        );
        // The affected symbol's real start line drives the SARIF region.
        assert_eq!(
            r["locations"][0]["physicalLocation"]["region"]["startLine"],
            42
        );
    }

    #[test]
    fn sarif_org_wide_breaking_is_error_level() {
        let mut result = base_result();
        result.org_wide = Some(OrgWideImpact {
            breaking: vec![OrgImpactItem {
                change_name: "Handler".to_string(),
                change_kind: "Function".to_string(),
                affected_name: "Caller".to_string(),
                affected_repo: "repo:client".to_string(),
                affected_file: "src/client.rs".to_string(),
                affected_line: 12,
                severity: "breaking".to_string(),
                reason: "cross-repo dependency".to_string(),
            }],
            warnings: vec![],
            info: vec![],
            impacted_repos: vec!["repo:client".to_string()],
            source_server: "local".to_string(),
        });
        let sarif = blast_radius_to_sarif(&result, "1.0.0");
        let results = sarif["runs"][0]["results"].as_array().expect("results");
        let org = results
            .iter()
            .find(|r| r["ruleId"] == "nw/org-impact")
            .expect("an org-impact result");
        assert_eq!(org["level"], "error");
        assert_eq!(org["properties"]["nestweaver/affectedRepo"], "repo:client");
        assert_eq!(
            org["locations"][0]["physicalLocation"]["region"]["startLine"],
            12
        );
    }

    #[test]
    fn sarif_extensions_present() {
        let sarif = blast_radius_to_sarif(&base_result(), "1.0.0");
        let props = &sarif["runs"][0]["properties"];
        assert!(props["nestweaver/gateState"].is_string());
        assert_eq!(props["nestweaver/analysisDirection"], "over-approximate");
        assert!(props["nestweaver/blindSpots"].is_array());
        assert!(props["nestweaver/coverage"].is_object());
    }

    #[test]
    fn sarif_contract_breaks_appended_with_contract_verified_source() {
        let mut sarif = blast_radius_to_sarif(&base_result(), "1.0.0");
        let breaks = vec![
            BreakingChange {
                symbol_uid: "src/api.rs:foo".to_string(),
                symbol_name: "foo".to_string(),
                kind: BreakKind::ParamAdded,
                tier: BreakTier::Breaking,
                detail: "parameter count increased from 1 to 2".to_string(),
            },
            BreakingChange {
                symbol_uid: "src/api.rs:bar".to_string(),
                symbol_name: "bar".to_string(),
                kind: BreakKind::ReturnTypeChanged,
                tier: BreakTier::LikelyBreaking,
                detail: "return type changed from i32 to i64".to_string(),
            },
        ];
        append_contract_breaks_to_sarif(&mut sarif, &breaks);

        let results = sarif["runs"][0]["results"].as_array().expect("results");
        let cb: Vec<_> = results
            .iter()
            .filter(|r| r["ruleId"] == "nw/contract-break")
            .collect();
        assert_eq!(cb.len(), 2);
        assert_eq!(cb[0]["level"], "error");
        assert_eq!(
            cb[0]["properties"]["nestweaver/severitySource"],
            "contract-verified"
        );
        assert_eq!(cb[1]["level"], "warning");

        // The rule is registered exactly once.
        let rules = sarif["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .expect("rules");
        let rule_count = rules
            .iter()
            .filter(|r| r["id"] == "nw/contract-break")
            .count();
        assert_eq!(rule_count, 1);
    }

    #[test]
    fn sarif_contract_breaks_empty_is_noop() {
        let mut sarif = blast_radius_to_sarif(&base_result(), "1.0.0");
        let before = sarif["runs"][0]["results"].as_array().unwrap().len();
        append_contract_breaks_to_sarif(&mut sarif, &[]);
        let after = sarif["runs"][0]["results"].as_array().unwrap().len();
        assert_eq!(before, after, "empty breaks must not add results");
    }
}

//! Two-tier impact composition — combine an already-computed LOCAL impact
//! result with an upstream server's org-wide impact into a
//! `{ local_impact, org_wide_impact }` response.
//!
//! The LOCAL tier is parameterized: callers run the tool against their local
//! daemon themselves and pass the resulting JSON in. This crate only handles
//! the upstream query, instance-independent repo dedup, and response
//! assembly — no `DaemonClient` coupling.

use std::sync::Mutex;
use std::time::Instant;

use serde_json::Value;
use tracing::debug;

use crate::dispatch::dispatch_json_rpc_authed;
use crate::health::{effective_timeout, eject_with_cap, is_upstream_down};
use crate::results::inject_or_wrap_provenance;
use crate::upstream::UpstreamHandle;

/// Execute the org-wide half of a two-tier impact query and combine it with
/// the already-computed local result.
///
/// When an upstream server is available:
/// 1. Take the LOCAL result (computed by the caller against its local tier)
/// 2. Query the server for the same tool
/// 3. Combine into a response with `local_impact` and `org_wide_impact` sections
///
/// Used for blast_radius, brain_impact, and affected_tests.
pub async fn two_tier_query(
    mut local_result: Value,
    upstreams: &[UpstreamHandle],
    ejection_guard: &Mutex<()>,
    tool_name: &str,
    params: &Value,
) -> Value {
    // 1. If no healthy upstream is configured, return local-only with clear
    // annotation.
    if !upstreams.iter().any(|u| u.is_healthy()) {
        inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
        if let Some(obj) = local_result.as_object_mut() {
            obj.insert("tier".to_string(), Value::String("local_only".into()));
        }
        return local_result;
    }

    // 2. Query upstream for org-wide impact.
    let upstream = match upstreams.iter().find(|u| u.is_healthy()) {
        Some(u) => u,
        None => {
            inject_or_wrap_provenance(&mut local_result, &["local"], &[]);
            if let Some(obj) = local_result.as_object_mut() {
                obj.insert("tier".to_string(), Value::String("local_only".into()));
                obj.insert(
                    "org_note".to_string(),
                    Value::String("upstream unavailable — showing local impact only".into()),
                );
            }
            return local_result;
        }
    };

    let server_name = upstream.name.clone();
    let mut up_client = upstream.client();
    let token = upstream.auth_token().map(|t| t.to_string());
    // Route the org-wide tier through the adaptive resolver instead of the
    // static configured timeout, mirroring query_upstream/query_merge.
    let timeout = effective_timeout(upstream.mode, upstream);
    let tool = tool_name.to_string();

    let server_params = params.clone();
    let started = Instant::now();
    let server_result = match tokio::time::timeout(
        timeout,
        dispatch_json_rpc_authed(&mut up_client, &tool, &server_params, token.as_deref()),
    )
    .await
    {
        Ok(Ok(result)) => {
            upstream.record_latency(started.elapsed());
            Some(result)
        }
        Ok(Err(e)) => {
            debug!(error = %e, tool = %tool, "org-wide two-tier query failed");
            // Eject on a genuine outage so a dead server used only via blast_radius/impact
            // doesn't stay "healthy" and time out every call (this path bypasses query_upstream).
            if is_upstream_down(&e) {
                eject_with_cap(upstream, upstreams, ejection_guard, "two-tier query failed");
            }
            None
        }
        Err(_) => {
            debug!(tool = %tool, "org-wide two-tier query timed out");
            eject_with_cap(
                upstream,
                upstreams,
                ejection_guard,
                "two-tier query timed out",
            );
            None
        }
    };

    // 3. Build two-tier response.
    let mut response = serde_json::json!({
        "tier": "two_tier",
        "local_impact": local_result,
    });

    if let Some(server) = server_result {
        // Filter out results that are already in the local impact to avoid
        // duplicating repos the user has indexed locally.
        let local_repos = extract_local_repos(&local_result);
        let filtered_server = filter_org_results(&server, &local_repos);

        response["org_wide_impact"] = serde_json::json!({
            "source_server": server_name,
            "results": filtered_server,
        });
    } else {
        response["org_wide_impact"] = serde_json::json!({
            "source_server": server_name,
            "status": "unavailable",
            "note": "upstream server query failed — showing local impact only",
        });
    }

    inject_or_wrap_provenance(&mut response, &["local", &server_name], &[]);

    response
}

/// Reduce a `repo_uid` to its instance-independent identity for two-tier dedup.
///
/// `repo_uid` is `repo:{instance}:{url_hash}`. The `{instance}` segment differs
/// between the LOCAL daemon (`local` / db-path hash) and the SERVER
/// (`nestweaver-server`) *by construction*, while `{url_hash}` is normalized at
/// mint time (T3.1b) and is therefore identical for the same repo across
/// instances. Matching on the full `repo_uid` never dedups across instances;
/// matching on `{url_hash}` does. This mirrors the instance-stripping the merge
/// dedup performs in [`crate::dedup::extract_identity`].
///
/// Falls back to the raw string when the value is not in canonical
/// `repo:{instance}:{url_hash}` form (e.g. a `file_path` stand-in), so both
/// sides key consistently.
pub fn repo_identity_key(repo_uid: &str) -> String {
    // "repo:{instance}:{url_hash}" -> "{url_hash}"
    repo_uid
        .strip_prefix("repo:")
        .and_then(|rest| rest.split_once(':'))
        .map(|(_instance, url_hash)| url_hash)
        .filter(|url_hash| !url_hash.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| repo_uid.to_string())
}

/// Extract repo identifiers mentioned in local blast_radius results.
///
/// Prefers the `repo_uid` field on each symbol (populated from the graph
/// store), reduced to its instance-independent identity via
/// [`repo_identity_key`] so the local and server tiers reconcile despite their
/// differing instance segments. Falls back to `file_path` as a whole-path key
/// when `repo_uid` is absent, so entries are still tracked but dedup is
/// path-exact rather than repo-level.
pub fn extract_local_repos(local: &Value) -> std::collections::HashSet<String> {
    let mut repos = std::collections::HashSet::new();

    for key in &["changed_symbols", "affected_symbols"] {
        if let Some(arr) = local.get(key).and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(repo) = item.get("repo_uid").and_then(|v| v.as_str())
                    && !repo.is_empty()
                {
                    repos.insert(repo_identity_key(repo));
                    continue;
                }
                // Fallback: use the full file_path as identity when no
                // repo_uid is available (should not happen for indexed repos).
                if let Some(fp) = item.get("file_path").and_then(|v| v.as_str()) {
                    repos.insert(fp.to_string());
                }
            }
        }
    }

    repos
}

/// Filter org-wide results to exclude repos already covered by local impact.
///
/// Removes entries from the server's `affected_symbols` and `changed_symbols`
/// whose repo matches a repo already present in the local impact set. Repo
/// identity is compared instance-independently via [`repo_identity_key`] — the
/// full `repo_uid` carries an `{instance}` segment that differs between the
/// local daemon and the server, so a full-string match never dedups. Falls back
/// to full `file_path` matching when `repo_uid` is absent (for backward
/// compatibility with older servers that don't emit it).
pub fn filter_org_results(
    server: &Value,
    local_repos: &std::collections::HashSet<String>,
) -> Value {
    if local_repos.is_empty() {
        return server.clone();
    }
    let mut filtered = server.clone();

    // Filter affected_symbols and changed_symbols arrays.
    for key in &["affected_symbols", "changed_symbols"] {
        if let Some(arr) = filtered.get_mut(key).and_then(|v| v.as_array_mut()) {
            arr.retain(|item| {
                // Prefer repo_uid for matching; fall back to full file_path.
                let dominated = item
                    .get("repo_uid")
                    .and_then(|v| v.as_str())
                    .filter(|r| !r.is_empty())
                    .map(|repo| local_repos.contains(&repo_identity_key(repo)))
                    .unwrap_or_else(|| {
                        item.get("file_path")
                            .and_then(|v| v.as_str())
                            .is_some_and(|fp| local_repos.contains(fp))
                    });
                !dominated
            });
        }
    }

    // Filter affected_clusters entries whose repo matches a local repo.
    if let Some(clusters) = filtered
        .get_mut("affected_clusters")
        .and_then(|v| v.as_array_mut())
    {
        clusters.retain(|cluster| {
            // Prefer repo_uid; fall back to full representative_file path.
            let dominated = cluster
                .get("repo_uid")
                .and_then(|v| v.as_str())
                .filter(|r| !r.is_empty())
                .map(|repo| local_repos.contains(&repo_identity_key(repo)))
                .unwrap_or_else(|| {
                    cluster
                        .get("representative_file")
                        .and_then(|v| v.as_str())
                        .is_some_and(|fp| local_repos.contains(fp))
                });
            !dominated
        });
    }

    filtered
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_local_repos_from_changed_symbols_with_repo_uid() {
        let local = json!({
            "changed_symbols": [
                {"uid": "1", "name": "foo", "file_path": "src/lib.rs", "repo_uid": "repo-alpha"},
                {"uid": "2", "name": "bar", "file_path": "api/handler.rs", "repo_uid": "repo-beta"},
            ],
            "affected_symbols": [
                {"uid": "3", "name": "baz", "file_path": "src/util.rs", "repo_uid": "repo-alpha"},
            ]
        });
        let repos = extract_local_repos(&local);
        assert!(repos.contains("repo-alpha"));
        assert!(repos.contains("repo-beta"));
        // Should NOT contain path components — repo_uid takes precedence.
        assert!(!repos.contains("src"));
        assert!(!repos.contains("api"));
    }

    #[test]
    fn extract_local_repos_falls_back_to_file_path() {
        // When repo_uid is absent, falls back to full file_path.
        let local = json!({
            "changed_symbols": [
                {"uid": "1", "name": "foo", "file_path": "src/lib.rs"},
            ],
            "affected_symbols": []
        });
        let repos = extract_local_repos(&local);
        assert!(repos.contains("src/lib.rs"));
    }

    #[test]
    fn filter_org_results_returns_full_for_now() {
        let server = json!({"affected_symbols": [{"name": "x"}]});
        let local_repos = std::collections::HashSet::new();
        let filtered = filter_org_results(&server, &local_repos);
        assert_eq!(filtered, server);
    }

    #[test]
    fn filter_org_results_dedups_same_repo_across_instances() {
        // Finding #6: the LOCAL tier and the SERVER's org-wide tier index the
        // SAME repo — identical normalized `url_hash` post-T3.1b — but under
        // DIFFERENT instance ids (`local` vs `nestweaver-server`). Matching on
        // the FULL `repo_uid` never coalesces, so `org_wide_impact` duplicates
        // everything the local tier already reported. Dedup must match on the
        // instance-independent repo identity and drop the redundant server rows.
        let url_hash = "abc123def456";
        let local = json!({
            "changed_symbols": [
                {"name": "process_payment", "file_path": "src/billing.rs",
                 "repo_uid": format!("repo:local:{url_hash}")},
            ],
            "affected_symbols": []
        });
        let local_repos = extract_local_repos(&local);

        let server = json!({
            "affected_symbols": [
                {"name": "process_payment", "file_path": "src/billing.rs",
                 "repo_uid": format!("repo:nestweaver-server:{url_hash}")},
            ],
            "affected_clusters": [
                {"representative_file": "src/billing.rs",
                 "repo_uid": format!("repo:nestweaver-server:{url_hash}")},
            ]
        });
        let filtered = filter_org_results(&server, &local_repos);

        let affected = filtered["affected_symbols"].as_array().unwrap();
        assert!(
            affected.is_empty(),
            "server rows for a repo already covered by the local tier must be \
             dropped despite the differing instance segment, got {affected:?}"
        );
        let clusters = filtered["affected_clusters"].as_array().unwrap();
        assert!(
            clusters.is_empty(),
            "server clusters for a locally-covered repo must be dropped too, \
             got {clusters:?}"
        );
    }

    #[test]
    fn filter_org_results_retains_distinct_repo_and_uncovered_symbol() {
        // Guard: a genuinely different repo (different `url_hash`) must NOT be
        // collapsed, and a server symbol whose repo is NOT in the local tier
        // must be RETAINED in org_wide_impact.
        let local = json!({
            "changed_symbols": [
                {"name": "f", "file_path": "src/a.rs",
                 "repo_uid": "repo:local:aaaaaaaaaaaa"},
            ],
            "affected_symbols": []
        });
        let local_repos = extract_local_repos(&local);

        let server = json!({
            "affected_symbols": [
                {"name": "g", "file_path": "src/b.rs",
                 "repo_uid": "repo:nestweaver-server:bbbbbbbbbbbb"},
            ]
        });
        let filtered = filter_org_results(&server, &local_repos);
        let affected = filtered["affected_symbols"].as_array().unwrap();
        assert_eq!(
            affected.len(),
            1,
            "a server symbol in a repo not covered locally must be retained"
        );
    }
}

//! Upstream health policy — outage classification, passive ejection with a
//! blast-radius cap, the mode-aware adaptive timeout, and the staleness
//! comparison between a local tier's repo states and each upstream's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tracing::warn;

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;

use crate::discovery::RoutingMode;
use crate::repo_identity::{normalized_repo_key, repo_name};
use crate::upstream::{HealthState, MAX_EJECTION_PERCENT, UpstreamHandle, can_eject};

/// Multiplier applied to the observed latency EWMA when deriving the adaptive
/// timeout. ~1.75x the smoothed mean approximates a p95-ish trigger point
/// without maintaining a full histogram — "The Tail at Scale" (Dean &
/// Barroso, 2013) argues for hedging/aborting around the 95th percentile
/// rather than a fixed deadline.
const TIMEOUT_K: f64 = 1.75;

/// Lower bound on any adaptive timeout. Below this, scheduling jitter
/// dominates and a tight deadline only manufactures spurious timeouts.
const TIMEOUT_FLOOR: Duration = Duration::from_millis(50);

/// Hard cap on the *Fallback*-mode upstream timeout. In Fallback mode the
/// local index is the fast path and the server is consulted only when local
/// results are sparse, so the upstream must never block the local answer past
/// the product's <200ms budget. Merge/Primary keep the configured ceiling
/// (~1s) because the richer upstream answer is the entire point of those modes.
const FALLBACK_MODE_CAP: Duration = Duration::from_millis(200);

/// Upper bound on a single `repo_states` RPC during the background staleness
/// refresh. This runs off the hot path (never a user query), but without a
/// bound a wedged upstream would stall the refresh loop indefinitely and freeze
/// the staleness verdict. Generous — staleness is not latency-sensitive — but
/// finite.
const STALENESS_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Classify whether an upstream query error means the server is DOWN (unreachable / timed out /
/// connection reset) versus a rejection from a HEALTHY server (auth failed, bad request,
/// server-side error). Only the former should passively eject the upstream: ejecting a healthy
/// server for an expired token or an `InvalidArgument` would pull a working server out of
/// rotation for the whole cooldown, indistinguishable from a real outage. Either way the query
/// still degrades to local — but a healthy upstream stays in rotation so the next query retries
/// it (and a persistent auth failure keeps surfacing in the logs, which is the actionable signal).
pub fn is_upstream_down(err: &anyhow::Error) -> bool {
    match err.chain().find_map(|e| e.downcast_ref::<tonic::Status>()) {
        // A gRPC status means the request reached a live server and it answered. Only
        // Unavailable (can't serve / still connecting) and DeadlineExceeded (too slow)
        // mean the server is effectively down for routing purposes.
        Some(status) => code_is_down(status.code()),
        // No gRPC status in the chain → a transport/connection error (connect refused, broken
        // pipe, reset) surfaced as a plain io/hyper error → the server is unreachable.
        None => true,
    }
}

/// Whether a raw gRPC status code means the upstream is down (vs a rejection from a live
/// server). Shared by [`is_upstream_down`] and the paths that hold a `tonic::Status` directly.
pub fn code_is_down(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
    )
}

/// Passively eject `upstream` after a failed live query, honoring the
/// blast-radius cap: never let one correlated network blip force MORE than
/// [`MAX_EJECTION_PERCENT`] of upstreams local-only at once (Envoy
/// `max_ejection_percent`). Recovery is the background task's job — this only
/// ever marks *down*, never permanently.
///
/// The recount→decide→eject sequence runs under `guard` so two concurrent
/// failing queries can't both read "under cap" and both eject, exceeding the
/// cap during exactly the correlated-blip scenario it exists for. Recovery
/// (`mark_up`) only ever *frees* a slot, so it needs no lock — serializing
/// the increment side alone upholds the invariant. `guard` synchronizes the
/// Relaxed health loads/stores (mutex acquire/release = happens-before), so
/// each ejection observes all prior ones.
pub fn eject_with_cap(
    upstream: &UpstreamHandle,
    all: &[UpstreamHandle],
    guard: &Mutex<()>,
    reason: &str,
) {
    // Poison-tolerant: the guard protects only the decision, so a panicked
    // prior holder must not brick ejection.
    let _decision = guard.lock().unwrap_or_else(|e| e.into_inner());
    let total = all.len();
    let currently_ejected = all.iter().filter(|u| !u.is_healthy()).count();
    if can_eject(total, currently_ejected, MAX_EJECTION_PERCENT) {
        warn!(upstream = %upstream.name, reason, "upstream ejected (passive mark-down)");
        upstream.mark_unhealthy();
    } else {
        warn!(
            upstream = %upstream.name,
            reason,
            ejected = currently_ejected,
            total,
            "ejection capped (blast-radius guard) — keeping upstream in rotation"
        );
    }
}

/// Per-upstream data the background maintenance task needs. Holds cloned
/// handles (cheap channel clone + shared `Arc<HealthState>`) so the task is
/// decoupled from the `HybridClient`'s mutable borrow on the query path.
pub struct MaintenanceProbe {
    pub name: String,
    pub client: NestWeaverDaemonClient<Channel>,
    pub token: Option<String>,
    pub health: Arc<HealthState>,
}

/// Compute the mode-aware *adaptive* upstream timeout for `handle`.
///
/// Rationale: a single static deadline rots as fleet latency drifts (the
/// InfoQ adaptive-hedging finding), so we scale off a rolling EWMA of observed
/// successful upstream latencies rather than a fixed 1s:
///
/// ```text
/// effective = clamp(K * latency_ewma, FLOOR, mode_ceiling)
/// ```
///
/// where `K ≈ 1.75` puts the deadline near the upstream's p95 ("The Tail at
/// Scale", Dean & Barroso, 2013) and `FLOOR = 50ms` avoids deadlines so tight
/// that scheduling jitter alone trips them. The per-upstream *configured*
/// timeout is the hard ceiling; `mode_ceiling = min(configured, mode_cap)`
/// where the Fallback cap is 200ms (keep the local fast path unblocked —
/// honors the <200ms budget) and Merge/Primary keep the full configured
/// ceiling (the richer upstream answer is the point). On a cold start (no
/// EWMA samples yet) we use `mode_ceiling`.
pub fn effective_timeout(mode: RoutingMode, handle: &UpstreamHandle) -> Duration {
    let configured = handle.timeout;
    let mode_ceiling = match mode {
        RoutingMode::Fallback => configured.min(FALLBACK_MODE_CAP),
        RoutingMode::Merge | RoutingMode::Primary => configured,
    };

    match handle.latency_ewma_ms() {
        // Cold start — no observed latency yet. Use the mode ceiling.
        None => mode_ceiling,
        Some(ewma_ms) => {
            let scaled = Duration::from_secs_f64((TIMEOUT_K * ewma_ms).max(0.0) / 1000.0);
            // `min(FLOOR, ceiling)` keeps the clamp bounds ordered even when an
            // explicit configured timeout is below the floor (e.g. "30ms").
            scaled.clamp(TIMEOUT_FLOOR.min(mode_ceiling), mode_ceiling)
        }
    }
}

pub fn local_sha_for_server_repo<'a>(
    local_states: &'a std::collections::HashMap<String, String>,
    server_repo_url: &str,
) -> Option<&'a str> {
    if let Some(sha) = local_states.get(server_repo_url) {
        return Some(sha.as_str());
    }

    let server_key = normalized_repo_key(server_repo_url);
    for (local_url, sha) in local_states {
        if normalized_repo_key(local_url) == server_key {
            return Some(sha.as_str());
        }
    }

    let server_name = repo_name(server_repo_url)?;
    let mut match_sha = None;
    for (local_url, sha) in local_states {
        let Some(local_name) = repo_name(local_url) else {
            continue;
        };
        if local_name.eq_ignore_ascii_case(&server_name) {
            if match_sha.is_some() {
                return None;
            }
            match_sha = Some(sha.as_str());
        }
    }
    match_sha
}

/// Compare the LOCAL tier's indexed SHAs against each healthy upstream's
/// `RepoStates` and return the repo URLs where the local index is behind.
///
/// The local tier is parameterized as data: `local_states` maps the local
/// daemon's repo URLs to their indexed SHAs, already fetched by the caller
/// (this crate never talks to a local daemon). On any upstream RPC failure
/// the affected source is skipped (staleness degrades to a false-negative
/// rather than blocking).
pub async fn compute_stale_repos(
    local_states: &HashMap<String, String>,
    probes: &[MaintenanceProbe],
) -> Vec<String> {
    let mut stale = Vec::new();
    for p in probes {
        if !p.health.is_healthy() {
            continue;
        }
        let mut client = p.client.clone();
        let mut req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
        if let Some(ref t) = p.token
            && let Ok(val) = format!("Bearer {t}").parse::<MetadataValue<_>>()
        {
            req.metadata_mut().insert("authorization", val);
        }
        // Bound the background RPC: a wedged upstream must not stall the
        // refresh loop. Timeout OR transport error → skip this probe (its
        // last-known verdict simply isn't refreshed this tick).
        if let Ok(Ok(resp)) =
            tokio::time::timeout(STALENESS_RPC_TIMEOUT, client.repo_states(req)).await
        {
            for server_repo in resp.into_inner().repos {
                if let Some(local_sha) =
                    local_sha_for_server_repo(local_states, &server_repo.repo_url)
                    && local_sha != server_repo.indexed_sha.as_str()
                    && !server_repo.indexed_sha.is_empty()
                {
                    stale.push(server_repo.repo_url.clone());
                }
            }
        }
    }
    stale
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mode-aware adaptive timeout (`effective_timeout`) ─────────────────

    fn handle_with(mode: RoutingMode, timeout: &str) -> UpstreamHandle {
        use crate::discovery::UpstreamConfig;
        let cfg = UpstreamConfig {
            name: Some("t".to_string()),
            url: "http://127.0.0.1:19999".to_string(),
            token: None,
            repos: vec![],
            mode,
            timeout: timeout.to_string(),
            ca_cert: None,
        };
        UpstreamHandle::from_config(&cfg).unwrap()
    }

    #[tokio::test]
    async fn effective_timeout_cold_start_uses_mode_ceiling() {
        // Fallback with the default 1s config: ceiling is capped at 200ms.
        let h = handle_with(RoutingMode::Fallback, "1s");
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(200)
        );
        // Merge with default 1s config: ceiling is the full configured 1s.
        let h = handle_with(RoutingMode::Merge, "1s");
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn effective_timeout_warm_scales_by_k() {
        // Small p95: 80ms EWMA, Merge mode, 1s ceiling -> ~K*ewma = 140ms.
        let h = handle_with(RoutingMode::Merge, "1s");
        h.record_latency(Duration::from_millis(80));
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_secs_f64(0.140)
        );
    }

    #[tokio::test]
    async fn effective_timeout_fallback_capped_at_200ms() {
        // Even with a 1s config and a high EWMA, Fallback never exceeds 200ms.
        let h = handle_with(RoutingMode::Fallback, "1s");
        h.record_latency(Duration::from_millis(400)); // K*400 = 700ms
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(200)
        );
    }

    #[tokio::test]
    async fn effective_timeout_merge_primary_up_to_configured_ceiling() {
        // A large EWMA clamps up to the configured 1s ceiling, not beyond.
        let h = handle_with(RoutingMode::Merge, "1s");
        h.record_latency(Duration::from_millis(2000)); // K*2000 = 3.5s
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_secs(1)
        );
        let h = handle_with(RoutingMode::Primary, "1s");
        h.record_latency(Duration::from_millis(2000));
        assert_eq!(
            effective_timeout(RoutingMode::Primary, &h),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn effective_timeout_explicit_small_config_caps_all_modes() {
        // An explicit "200ms" config is the hard ceiling for every mode.
        for mode in [
            RoutingMode::Fallback,
            RoutingMode::Merge,
            RoutingMode::Primary,
        ] {
            let h = handle_with(mode, "200ms");
            h.record_latency(Duration::from_millis(900)); // K*900 = 1.575s
            assert_eq!(
                effective_timeout(mode, &h),
                Duration::from_millis(200),
                "mode {mode:?} should cap at the configured 200ms"
            );
        }
    }

    #[tokio::test]
    async fn effective_timeout_respects_floor() {
        // A tiny EWMA would scale below the 50ms floor — clamp up to it.
        let h = handle_with(RoutingMode::Merge, "1s");
        h.record_latency(Duration::from_millis(5)); // K*5 = 8.75ms
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_millis(50)
        );
    }

    #[tokio::test]
    async fn effective_timeout_floor_clamp_ordered_for_tiny_config() {
        // Configured ceiling below the floor must not panic the clamp; the
        // ceiling wins (effective bounds become [ceiling, ceiling]).
        let h = handle_with(RoutingMode::Merge, "30ms");
        h.record_latency(Duration::from_millis(5));
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_millis(30)
        );
    }

    #[test]
    fn repo_state_lookup_matches_file_checkout_to_remote_url() {
        let local_states = std::collections::HashMap::from([
            (
                "file:///home/dev/checkouts/api".to_string(),
                "local-sha".to_string(),
            ),
            (
                "file:///home/dev/checkouts/billing".to_string(),
                "billing-sha".to_string(),
            ),
        ]);

        assert_eq!(
            local_sha_for_server_repo(&local_states, "https://github.com/acme/api.git"),
            Some("local-sha")
        );
        assert_eq!(
            local_sha_for_server_repo(&local_states, "git@github.com:acme/billing.git"),
            Some("billing-sha")
        );
    }

    #[tokio::test]
    async fn mass_ejection_capped_keeps_last_upstream() {
        // Two upstreams, 50% cap → at most one may be ejected at a time. A
        // correlated blip that fails both must not force the whole session
        // local-only; the blast-radius guard keeps the last one in rotation.
        let cfg = |name: &str| crate::discovery::UpstreamConfig {
            name: Some(name.to_string()),
            url: "http://127.0.0.1:19990".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
            ca_cert: None,
        };
        let all = vec![
            UpstreamHandle::from_config(&cfg("a")).unwrap(),
            UpstreamHandle::from_config(&cfg("b")).unwrap(),
        ];
        let guard = Mutex::new(());

        // First failure ejects one (0 ejected < cap of 1).
        eject_with_cap(&all[0], &all, &guard, "query failed");
        assert!(!all[0].is_healthy());

        // Second failure would exceed the cap → the upstream stays healthy.
        eject_with_cap(&all[1], &all, &guard, "query failed");
        assert!(
            all[1].is_healthy(),
            "blast-radius guard must keep the last upstream in rotation"
        );
    }

    #[tokio::test]
    async fn concurrent_ejection_respects_cap() {
        // Two upstreams, 50% cap → at most one may be ejected. Two threads
        // each try to eject a *different* upstream at the same instant; the
        // shared guard must serialize the recount→eject decision so exactly
        // one wins. Without the guard both could read "0 ejected" and eject,
        // breaching the cap during the correlated-blip scenario it guards.
        let cfg = |name: &str| crate::discovery::UpstreamConfig {
            name: Some(name.to_string()),
            url: "http://127.0.0.1:19990".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
            ca_cert: None,
        };
        let all = vec![
            UpstreamHandle::from_config(&cfg("a")).unwrap(),
            UpstreamHandle::from_config(&cfg("b")).unwrap(),
        ];
        let guard = Mutex::new(());

        std::thread::scope(|s| {
            for u in &all {
                s.spawn(|| eject_with_cap(u, &all, &guard, "race"));
            }
        });

        let ejected = all.iter().filter(|u| !u.is_healthy()).count();
        assert_eq!(ejected, 1, "cap must hold under concurrent ejection");
    }
}

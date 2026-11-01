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

/// Consecutive timeouts at the full mode ceiling before an upstream is
/// ejected (nw-905). One slow call is variance, not an outage.
pub const CEILING_TIMEOUTS_TO_EJECT: u32 = 2;

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
    let mode_ceiling = mode_ceiling(mode, handle);

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

/// The hard ceiling of the adaptive timeout for `mode`: the configured
/// `timeout`, except in Fallback mode where it is the upstream's
/// `fallback_timeout` cap (default 200ms, never above `timeout`; nw-834).
pub fn mode_ceiling(mode: RoutingMode, handle: &UpstreamHandle) -> Duration {
    match mode {
        RoutingMode::Fallback => handle.fallback_cap(),
        RoutingMode::Merge | RoutingMode::Primary => handle.timeout,
    }
}

/// Handle a live query that missed its `deadline` (nw-905).
///
/// The miss feeds the latency estimate (so the next adaptive deadline widens
/// instead of staying pinned near the floor) and ejects the upstream only
/// after [`CEILING_TIMEOUTS_TO_EJECT`] consecutive misses AT the mode ceiling
/// — i.e. the server is slower than the operator's configured budget, twice
/// running. A miss below the ceiling is just the adaptive deadline learning.
/// Genuine outages (`Unavailable`, connection errors) still eject at once via
/// [`is_upstream_down`].
pub fn handle_query_timeout(
    mode: RoutingMode,
    upstream: &UpstreamHandle,
    all: &[UpstreamHandle],
    guard: &Mutex<()>,
    deadline: Duration,
    reason: &str,
) {
    let at_ceiling = deadline >= mode_ceiling(mode, upstream);
    let streak = upstream.record_timeout(deadline, at_ceiling);
    if streak >= CEILING_TIMEOUTS_TO_EJECT {
        upstream.reset_timeout_streak();
        eject_with_cap(upstream, all, guard, reason);
    } else {
        tracing::debug!(
            upstream = %upstream.name,
            deadline_ms = deadline.as_millis() as u64,
            streak,
            reason,
            "upstream timed out; widening adaptive deadline, not ejecting yet"
        );
    }
}

/// Find the local repo state that corresponds to `server_repo_url`: the
/// matching local URL and its indexed SHA.
///
/// Matching is by full canonical repo identity (`host/owner/name`), so
/// `other-org/x` never matches `acme/x` (nw-906). The one leaf-name fallback
/// is for a local repo that is *path-only* (`file://…`, indexed without a
/// remote): it carries no owner to compare, so a unique leaf-name match is the
/// only way to pair it with its server counterpart. Remote-form local URLs
/// never use the leaf fallback.
pub fn local_entry_for_server_repo<'a>(
    local_states: &'a HashMap<String, String>,
    server_repo_url: &str,
) -> Option<(&'a str, &'a str)> {
    if let Some((url, sha)) = local_states.get_key_value(server_repo_url) {
        return Some((url.as_str(), sha.as_str()));
    }

    let server_key = normalized_repo_key(server_repo_url);
    for (local_url, sha) in local_states {
        if normalized_repo_key(local_url) == server_key {
            return Some((local_url.as_str(), sha.as_str()));
        }
    }

    let server_name = repo_name(server_repo_url)?;
    let mut found = None;
    for (local_url, sha) in local_states {
        if local_checkout_path(local_url).is_none() {
            continue;
        }
        let Some(local_name) = repo_name(local_url) else {
            continue;
        };
        if local_name.eq_ignore_ascii_case(&server_name) {
            if found.is_some() {
                return None;
            }
            found = Some((local_url.as_str(), sha.as_str()));
        }
    }
    found
}

pub fn local_sha_for_server_repo<'a>(
    local_states: &'a HashMap<String, String>,
    server_repo_url: &str,
) -> Option<&'a str> {
    local_entry_for_server_repo(local_states, server_repo_url).map(|(_, sha)| sha)
}

/// The on-disk checkout path of a path-only local repo URL (`file://…` or an
/// absolute path); `None` for remote-form URLs.
fn local_checkout_path(local_url: &str) -> Option<&std::path::Path> {
    let path = local_url.strip_prefix("file://").unwrap_or(local_url);
    path.starts_with('/').then(|| std::path::Path::new(path))
}

/// Upper bound on one `git merge-base --is-ancestor` probe. It runs in the
/// background staleness task, never a user query, but a wedged checkout (hung
/// network filesystem) must not stall the verdict refresh.
const ANCESTRY_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// `true` when `server_sha` is an ancestor of (or equal to) `local_sha` in the
/// checkout at `path`, i.e. the local checkout already contains everything the
/// server indexed. Runs off the runtime thread (`tokio::process`), is killed on
/// drop, and is bounded by [`ANCESTRY_CHECK_TIMEOUT`]. Any failure (git
/// missing, unknown object, timeout) is `false`.
async fn server_sha_is_in_local_history(
    path: &std::path::Path,
    server_sha: &str,
    local_sha: &str,
) -> bool {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(path)
        .args(["merge-base", "--is-ancestor", server_sha, local_sha])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(ANCESTRY_CHECK_TIMEOUT, cmd.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

/// Whether the local index is BEHIND the server for one repo (nw-906).
///
/// Unknown is not stale: an empty or sentinel (`"local"`, a non-git
/// directory) SHA on either side carries no revision to compare. For a
/// path-only local checkout we ask git whether the server's SHA is already in
/// the local history — a local checkout that is *ahead* is not stale. Where
/// that cannot be checked (remote-form local URL, git unavailable, SHA not in
/// the checkout) a differing SHA stays stale, the conservative direction:
/// consult the server.
pub async fn local_is_behind(local_url: &str, local_sha: &str, server_sha: &str) -> bool {
    let unknown = |sha: &str| sha.is_empty() || sha == "local";
    if unknown(server_sha) || unknown(local_sha) || local_sha == server_sha {
        return false;
    }
    match local_checkout_path(local_url) {
        Some(path) => !server_sha_is_in_local_history(path, server_sha, local_sha).await,
        None => true,
    }
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
                if let Some((local_url, local_sha)) =
                    local_entry_for_server_repo(local_states, &server_repo.repo_url)
                    && local_is_behind(local_url, local_sha, &server_repo.indexed_sha).await
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
            fallback_timeout: None,
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
            fallback_timeout: None,
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
            fallback_timeout: None,
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

    #[tokio::test]
    async fn fallback_timeout_key_raises_or_lowers_the_cap() {
        // nw-834: the cap is its own per-upstream key, bounded by `timeout`.
        let mk = |timeout: &str, cap: Option<&str>| {
            use crate::discovery::UpstreamConfig;
            UpstreamHandle::from_config(&UpstreamConfig {
                name: Some("t".to_string()),
                url: "http://127.0.0.1:19999".to_string(),
                token: None,
                repos: vec![],
                mode: RoutingMode::Fallback,
                timeout: timeout.to_string(),
                ca_cert: None,
                fallback_timeout: cap.map(String::from),
            })
            .unwrap()
        };
        let h = mk("1s", Some("750ms"));
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(750)
        );
        h.record_latency(Duration::from_millis(2000));
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(750)
        );
        // `timeout` bounds the cap; a plain "1s" is no longer special.
        let h = mk("500ms", Some("2s"));
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(500)
        );
        let h = mk("1000ms", None);
        assert_eq!(
            effective_timeout(RoutingMode::Fallback, &h),
            Duration::from_millis(200)
        );
        // Merge/Primary ignore fallback_timeout.
        let h = mk("1s", Some("750ms"));
        assert_eq!(
            effective_timeout(RoutingMode::Merge, &h),
            Duration::from_secs(1)
        );
    }

    fn two_handles() -> Vec<UpstreamHandle> {
        vec![
            handle_with(RoutingMode::Merge, "1s"),
            handle_with(RoutingMode::Merge, "1s"),
        ]
    }

    #[tokio::test]
    async fn one_adaptive_timeout_miss_neither_ejects_nor_forgets() {
        // nw-905: a fast burst pins the deadline near the floor; one heavier
        // call misses it. That must widen the estimate, not eject.
        let all = two_handles();
        let guard = Mutex::new(());
        all[0].record_latency(Duration::from_millis(8));
        let deadline = effective_timeout(RoutingMode::Merge, &all[0]);
        assert_eq!(deadline, Duration::from_millis(50));
        handle_query_timeout(RoutingMode::Merge, &all[0], &all, &guard, deadline, "t");
        assert!(
            all[0].is_healthy(),
            "one miss below the ceiling must not eject"
        );
        assert!(
            effective_timeout(RoutingMode::Merge, &all[0]) > deadline,
            "the miss must feed the latency estimate"
        );
    }

    #[tokio::test]
    async fn repeated_ceiling_timeouts_eject_and_a_success_resets_the_streak() {
        let all = two_handles();
        let guard = Mutex::new(());
        let ceiling = Duration::from_secs(1);
        handle_query_timeout(RoutingMode::Merge, &all[0], &all, &guard, ceiling, "t");
        assert!(
            all[0].is_healthy(),
            "a single ceiling miss is not an outage"
        );
        all[0].record_latency(Duration::from_millis(300));
        handle_query_timeout(RoutingMode::Merge, &all[0], &all, &guard, ceiling, "t");
        assert!(all[0].is_healthy(), "success in between resets the streak");
        handle_query_timeout(RoutingMode::Merge, &all[0], &all, &guard, ceiling, "t");
        assert!(!all[0].is_healthy(), "two consecutive ceiling misses eject");
    }

    // ── Staleness (nw-906) ────────────────────────────────────────────────

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn local_ahead_is_not_stale_but_local_behind_is() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let older = git(p, &["rev-parse", "HEAD"]);
        git(p, &["commit", "-q", "--allow-empty", "-m", "two"]);
        git(p, &["commit", "-q", "--allow-empty", "-m", "three"]);
        let newer = git(p, &["rev-parse", "HEAD"]);
        let url = format!("file://{}", p.display());

        // Local is two commits ahead of the server's indexed SHA.
        assert!(
            !local_is_behind(&url, &newer, &older).await,
            "ahead is not stale"
        );
        // Local is behind the server.
        assert!(
            local_is_behind(&url, &older, &newer).await,
            "behind is stale"
        );
        // Equal.
        assert!(!local_is_behind(&url, &newer, &newer).await);
        // Server SHA the checkout has never seen -> consult the server.
        assert!(local_is_behind(&url, &newer, &"f".repeat(40)).await);
    }

    #[tokio::test]
    async fn non_git_sentinel_and_empty_shas_are_unknown_not_stale() {
        assert!(!local_is_behind("file:///srv/plain-dir", "local", &"a".repeat(40)).await);
        assert!(!local_is_behind("file:///srv/x", "", &"a".repeat(40)).await);
        assert!(!local_is_behind("file:///srv/x", &"a".repeat(40), "").await);
        assert!(!local_is_behind("file:///srv/x", &"a".repeat(40), "local").await);
    }

    #[tokio::test]
    async fn remote_form_local_url_with_differing_sha_stays_stale() {
        // No checkout path to inspect: conservative, consult the server.
        assert!(
            local_is_behind(
                "https://github.com/acme/x",
                &"a".repeat(40),
                &"b".repeat(40)
            )
            .await
        );
    }

    #[test]
    fn same_leaf_name_in_another_org_does_not_match() {
        let local_states = HashMap::from([(
            "https://github.com/other-org/x.git".to_string(),
            "sha-other".to_string(),
        )]);
        assert_eq!(
            local_sha_for_server_repo(&local_states, "https://github.com/acme/x"),
            None
        );
        // Same identity in another URL form still matches.
        let local_states = HashMap::from([(
            "git@github.com:acme/x.git".to_string(),
            "sha-acme".to_string(),
        )]);
        assert_eq!(
            local_sha_for_server_repo(&local_states, "https://github.com/acme/x"),
            Some("sha-acme")
        );
    }
}

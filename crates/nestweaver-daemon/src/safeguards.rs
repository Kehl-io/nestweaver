//! Query safeguards for server-mode deployments.
//!
//! Provides per-tool timeouts, depth limits, result count caps, slow query
//! logging, and per-client rate limiting. Every gRPC handler is wrapped in
//! `with_safeguard` which races the handler future against a timeout.

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use governor::{Quota, RateLimiter, clock::DefaultClock, state::keyed::DashMapStateStore};
use tonic::Status;

// ── Timeout & depth configuration ──────────────────────────────────────

/// Result of depth clamping. Carries the effective depth along with
/// metadata indicating whether the original request was clamped.
#[derive(Debug, Clone)]
pub struct DepthResult {
    /// The depth to use for the query.
    pub depth: u32,
    /// Whether the requested depth was clamped to the hard cap.
    pub clamped: bool,
    /// The client's original requested depth, if it was clamped.
    pub original_depth: Option<u32>,
}

/// Per-tool safeguard configuration for server-mode query protection.
#[derive(Debug, Clone)]
pub struct QuerySafeguards {
    /// Per-tool default timeout (used when client doesn't specify one).
    pub timeouts: HashMap<String, Duration>,
    /// Per-tool hard cap (client can lower but never exceed).
    pub hard_caps: HashMap<String, Duration>,
    /// Fallback timeout when tool is not in the map.
    pub default_timeout: Duration,
    /// Fallback hard cap when tool is not in the map.
    pub default_hard_cap: Duration,
    /// Per-tool default depth for graph traversals.
    pub default_depths: HashMap<String, u32>,
    /// Per-tool maximum depth (hard cap).
    pub max_depths: HashMap<String, u32>,
    /// Per-tool default result count limit.
    pub default_result_limits: HashMap<String, usize>,
    /// Per-tool hard cap on result count.
    pub max_result_limits: HashMap<String, usize>,
}

impl QuerySafeguards {
    /// Returns production-ready defaults matching the safeguards spec.
    pub fn default_server() -> Self {
        let mut timeouts = HashMap::new();
        let mut hard_caps = HashMap::new();

        // Search tools — 10s default
        for tool in &[
            "brain_search",
            "brain_context",
            "regex_search",
            "read_symbols",
            "count_patterns",
            "brain_status",
            "backlinks",
            "brain_guide",
            "brain_diff",
            "brain_broken_links",
            "brain_orphan_documents",
            "brain_topic_clusters",
            "brain_tag_graph",
            "brain_doc_stats",
            "brain_memory_lint",
            "brain_memory_consolidate",
            "brain_memory_related",
            "detect_changes",
            "affected_tests",
            "stale_check",
            "bridge_nodes",
            "get_summary",
            "investigate",
            "investigate_expand",
            "investigate_hydrate",
            "set_extension",
            "query_extensions",
            "hub_nodes",
            "clusters",
            "note_get",
            "project_context",
            "brain_impact",
        ] {
            timeouts.insert(tool.to_string(), Duration::from_secs(10));
        }

        // Analysis tools — 30s default
        for tool in &[
            "blast_radius",
            "flow_trace",
            "cross_repo_contracts",
            "contract_drift",
        ] {
            timeouts.insert(tool.to_string(), Duration::from_secs(30));
        }

        // Dead code is legitimately slow — 30s default, 120s cap
        timeouts.insert("dead_code".to_string(), Duration::from_secs(30));
        hard_caps.insert("dead_code".to_string(), Duration::from_secs(120));

        // Standard hard caps
        for tool in &["brain_search", "brain_context"] {
            hard_caps.insert(tool.to_string(), Duration::from_secs(60));
        }
        hard_caps.insert("regex_search".to_string(), Duration::from_secs(30));
        for tool in &["blast_radius", "flow_trace", "cross_repo_contracts"] {
            hard_caps.insert(tool.to_string(), Duration::from_secs(60));
        }

        // Depth limits
        let mut default_depths = HashMap::new();
        let mut max_depths = HashMap::new();
        default_depths.insert("blast_radius".to_string(), 3);
        max_depths.insert("blast_radius".to_string(), 10);
        default_depths.insert("flow_trace".to_string(), 5);
        max_depths.insert("flow_trace".to_string(), 15);
        default_depths.insert("investigate_expand".to_string(), 2);
        max_depths.insert("investigate_expand".to_string(), 5);

        // Result count limits
        let mut default_result_limits = HashMap::new();
        let mut max_result_limits = HashMap::new();

        // Search tools: 100 default, 5000 cap
        for tool in &["brain_search", "regex_search"] {
            default_result_limits.insert(tool.to_string(), 100);
            max_result_limits.insert(tool.to_string(), 5_000);
        }

        // Context tools: 500 default, 5000 cap
        for tool in &["brain_context", "project_context"] {
            default_result_limits.insert(tool.to_string(), 500);
            max_result_limits.insert(tool.to_string(), 5_000);
        }

        // Structural tools: 50 default, 500 cap
        for tool in &["hub_nodes", "clusters"] {
            default_result_limits.insert(tool.to_string(), 50);
            max_result_limits.insert(tool.to_string(), 500);
        }

        Self {
            timeouts,
            hard_caps,
            default_timeout: Duration::from_secs(10),
            default_hard_cap: Duration::from_secs(30),
            default_depths,
            max_depths,
            default_result_limits,
            max_result_limits,
        }
    }

    /// Returns a no-op safeguard config for local (non-server) mode.
    pub fn disabled() -> Self {
        Self {
            timeouts: HashMap::new(),
            hard_caps: HashMap::new(),
            default_timeout: Duration::from_secs(600),
            default_hard_cap: Duration::from_secs(600),
            default_depths: HashMap::new(),
            max_depths: HashMap::new(),
            default_result_limits: HashMap::new(),
            max_result_limits: HashMap::new(),
        }
    }

    /// Compute the effective timeout for a tool, respecting hard caps.
    /// Client can request a lower timeout but never exceed the hard cap.
    pub fn effective_timeout(&self, tool: &str, client_requested: Option<Duration>) -> Duration {
        let hard_cap = self
            .hard_caps
            .get(tool)
            .copied()
            .unwrap_or(self.default_hard_cap);
        let default = self
            .timeouts
            .get(tool)
            .copied()
            .unwrap_or(self.default_timeout);
        match client_requested {
            Some(req) => req.min(hard_cap),
            None => default,
        }
    }

    /// Returns the effective depth for a graph traversal tool, clamping to the
    /// hard cap when the client requests more. The returned `DepthResult`
    /// carries a `clamped` flag and the original requested value so callers can
    /// communicate the clamping in response metadata.
    pub fn effective_depth(&self, tool: &str, client_requested: Option<u32>) -> DepthResult {
        let default = self.default_depths.get(tool).copied().unwrap_or(3);
        let max = self.max_depths.get(tool).copied().unwrap_or(10);

        match client_requested {
            Some(req) if req > max => {
                tracing::info!(
                    tool,
                    requested = req,
                    clamped_to = max,
                    "depth clamped to hard cap"
                );
                DepthResult {
                    depth: max,
                    clamped: true,
                    original_depth: Some(req),
                }
            }
            Some(req) => DepthResult {
                depth: req,
                clamped: false,
                original_depth: None,
            },
            None => DepthResult {
                depth: default,
                clamped: false,
                original_depth: None,
            },
        }
    }

    /// Returns the effective result limit, clamped to the hard cap.
    pub fn effective_result_limit(&self, tool: &str, client_requested: Option<usize>) -> usize {
        let default = self.default_result_limits.get(tool).copied().unwrap_or(100);
        let max = self.max_result_limits.get(tool).copied().unwrap_or(5_000);
        match client_requested {
            Some(req) => req.min(max),
            None => default,
        }
    }
}

// ── Safeguard wrapper ──────────────────────────────────────────────────

/// Wraps a handler future with a per-tool timeout. Returns
/// `DEADLINE_EXCEEDED` if the handler does not complete in time.
/// Also logs slow queries at tiered thresholds.
///
/// **Cancellation note:** When the timeout fires, `tokio::select!` drops the
/// handler future. If the handler spawned a `spawn_blocking` task, that task
/// continues on its OS thread (tokio does not abort blocking threads). To
/// cooperate with cancellation, handlers should accept a
/// [`CancellationToken`](tokio_util::sync::CancellationToken) or
/// [`AtomicBool`](std::sync::atomic::AtomicBool) and check it periodically.
/// See [`with_safeguard_cancellable`] for a variant that manages a token.
pub async fn with_safeguard<F, T>(
    tool_name: &str,
    safeguards: &QuerySafeguards,
    client_timeout: Option<Duration>,
    handler: F,
) -> Result<T, Status>
where
    F: Future<Output = Result<T, Status>>,
{
    let timeout = safeguards.effective_timeout(tool_name, client_timeout);
    let start = Instant::now();

    let result = tokio::select! {
        result = handler => result,
        _ = tokio::time::sleep(timeout) => {
            tracing::warn!(
                tool = %tool_name,
                timeout_ms = timeout.as_millis(),
                "query exceeded timeout — note: any in-flight spawn_blocking \
                 tasks will continue running in the background"
            );
            Err(Status::deadline_exceeded(format!(
                "{} query exceeded {}s timeout",
                tool_name,
                timeout.as_secs()
            )))
        }
    };

    // Slow query logging at tiered thresholds.
    let elapsed = start.elapsed();
    log_slow_query(tool_name, elapsed, timeout);

    result
}

/// Like [`with_safeguard`], but provides a cancellation flag that is set
/// when the timeout fires. Handlers that run expensive work inside
/// `spawn_blocking` should check this flag periodically and bail early.
pub async fn with_safeguard_cancellable<F, T>(
    tool_name: &str,
    safeguards: &QuerySafeguards,
    client_timeout: Option<Duration>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    handler: F,
) -> Result<T, Status>
where
    F: Future<Output = Result<T, Status>>,
{
    let timeout = safeguards.effective_timeout(tool_name, client_timeout);
    let start = Instant::now();

    let result = tokio::select! {
        result = handler => result,
        _ = tokio::time::sleep(timeout) => {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            tracing::warn!(
                tool = %tool_name,
                timeout_ms = timeout.as_millis(),
                "query exceeded timeout — cancellation flag set for spawn_blocking"
            );
            Err(Status::deadline_exceeded(format!(
                "{} query exceeded {}s timeout",
                tool_name,
                timeout.as_secs()
            )))
        }
    };

    let elapsed = start.elapsed();
    log_slow_query(tool_name, elapsed, timeout);

    result
}

/// Log queries that exceed tiered thresholds of their timeout.
fn log_slow_query(tool_name: &str, elapsed: Duration, timeout: Duration) {
    let warn_threshold = timeout * 4 / 5; // 80%
    let info_threshold = Duration::from_secs(5);
    let debug_threshold = Duration::from_secs(2);
    let trace_threshold = Duration::from_millis(500);

    if elapsed > warn_threshold {
        tracing::warn!(
            tool = %tool_name,
            elapsed_ms = elapsed.as_millis(),
            timeout_ms = timeout.as_millis(),
            "slow query (>80% of timeout)"
        );
    } else if elapsed > info_threshold {
        tracing::info!(
            tool = %tool_name,
            elapsed_ms = elapsed.as_millis(),
            "slow query (>5s)"
        );
    } else if elapsed > debug_threshold {
        tracing::debug!(
            tool = %tool_name,
            elapsed_ms = elapsed.as_millis(),
            "slow query (>2s)"
        );
    } else if elapsed > trace_threshold {
        tracing::trace!(
            tool = %tool_name,
            elapsed_ms = elapsed.as_millis(),
            "query >500ms"
        );
    }
}

// ── Per-client rate limiting ───────────────────────────────────────────

/// Configuration for per-client rate limiting.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum sustained requests per minute. Default: 100.
    pub requests_per_minute: u32,
    /// Burst size — number of requests allowed in a single burst. Default: 20.
    pub burst: u32,
    /// Whether rate limiting is enabled. Default: true in server mode.
    pub enabled: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 100,
            burst: 20,
            enabled: true,
        }
    }
}

/// Per-client rate limiter registry. Each unique auth token gets its own
/// token bucket. Uses `governor` with `DashMap` for concurrent access.
pub struct ClientRateLimiters {
    limiter: Arc<RateLimiter<String, DashMapStateStore<String>, DefaultClock>>,
    enabled: bool,
}

impl ClientRateLimiters {
    /// Create a new rate limiter registry from config.
    pub fn new(config: &RateLimitConfig) -> Self {
        // governor's Quota is "N cells per period". We want `burst` cells
        // replenished at a rate of `requests_per_minute / 60` per second.
        let per_second = (config.requests_per_minute as f64 / 60.0).ceil() as u32;
        let per_second = per_second.max(1);
        let burst = NonZeroU32::new(config.burst.max(1)).unwrap();

        let quota = Quota::per_second(NonZeroU32::new(per_second).unwrap()).allow_burst(burst);

        let limiter = Arc::new(RateLimiter::dashmap(quota));

        Self {
            limiter,
            enabled: config.enabled,
        }
    }

    /// Check whether a request from the given client token is allowed.
    /// Returns `Ok(())` if allowed, or `RESOURCE_EXHAUSTED` if rate-limited.
    pub fn check(&self, client_token: &str) -> Result<(), Status> {
        if !self.enabled {
            return Ok(());
        }
        match self.limiter.check_key(&client_token.to_string()) {
            Ok(_) => Ok(()),
            Err(_) => Err(Status::resource_exhausted(
                "rate limit exceeded: too many requests per minute",
            )),
        }
    }

    /// Remove stale entries that haven't been used recently.
    /// Call periodically (e.g. every 10 minutes) to prevent memory leaks
    /// from rotating tokens.
    pub fn sweep_stale(&self) {
        self.limiter.retain_recent();
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_timeout_uses_default() {
        let sg = QuerySafeguards::default_server();
        let t = sg.effective_timeout("brain_search", None);
        assert_eq!(t, Duration::from_secs(10));
    }

    #[test]
    fn effective_timeout_client_can_lower() {
        let sg = QuerySafeguards::default_server();
        let t = sg.effective_timeout("brain_search", Some(Duration::from_secs(5)));
        assert_eq!(t, Duration::from_secs(5));
    }

    #[test]
    fn effective_timeout_client_cannot_exceed_hard_cap() {
        let sg = QuerySafeguards::default_server();
        // brain_search hard cap is 60s
        let t = sg.effective_timeout("brain_search", Some(Duration::from_secs(120)));
        assert_eq!(t, Duration::from_secs(60));
    }

    #[test]
    fn effective_timeout_unknown_tool_uses_defaults() {
        let sg = QuerySafeguards::default_server();
        let t = sg.effective_timeout("unknown_tool", None);
        assert_eq!(t, Duration::from_secs(10)); // default_timeout
    }

    #[test]
    fn effective_timeout_unknown_tool_hard_cap() {
        let sg = QuerySafeguards::default_server();
        let t = sg.effective_timeout("unknown_tool", Some(Duration::from_secs(60)));
        assert_eq!(t, Duration::from_secs(30)); // default_hard_cap
    }

    #[test]
    fn depth_limit_default() {
        let sg = QuerySafeguards::default_server();
        assert_eq!(sg.effective_depth("blast_radius", None).depth, 3);
        assert_eq!(sg.effective_depth("flow_trace", None).depth, 5);
        assert!(!sg.effective_depth("blast_radius", None).clamped);
    }

    #[test]
    fn depth_limit_clamps_oversize() {
        let sg = QuerySafeguards::default_server();
        let result = sg.effective_depth("blast_radius", Some(15));
        assert!(result.clamped);
        assert_eq!(result.depth, 10); // hard cap for blast_radius
        assert_eq!(result.original_depth, Some(15));
    }

    #[test]
    fn depth_limit_allows_within_cap() {
        let sg = QuerySafeguards::default_server();
        let result = sg.effective_depth("blast_radius", Some(8));
        assert_eq!(result.depth, 8);
        assert!(!result.clamped);
    }

    #[test]
    fn result_limit_default_and_cap() {
        let sg = QuerySafeguards::default_server();
        assert_eq!(sg.effective_result_limit("brain_search", None), 100);
        assert_eq!(
            sg.effective_result_limit("brain_search", Some(10_000)),
            5_000
        );
        assert_eq!(sg.effective_result_limit("hub_nodes", None), 50);
    }

    #[test]
    fn rate_limiter_allows_burst() {
        let config = RateLimitConfig {
            requests_per_minute: 100,
            burst: 5,
            enabled: true,
        };
        let rl = ClientRateLimiters::new(&config);
        // Should allow burst of 5
        for _ in 0..5 {
            assert!(rl.check("token-a").is_ok());
        }
    }

    #[test]
    fn rate_limiter_rejects_after_burst() {
        let config = RateLimitConfig {
            requests_per_minute: 60, // 1 per second
            burst: 3,
            enabled: true,
        };
        let rl = ClientRateLimiters::new(&config);
        // Drain the burst
        for _ in 0..3 {
            assert!(rl.check("token-b").is_ok());
        }
        // Next should be rejected (no time has passed)
        let err = rl.check("token-b").unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn rate_limiter_independent_tokens() {
        let config = RateLimitConfig {
            requests_per_minute: 60,
            burst: 2,
            enabled: true,
        };
        let rl = ClientRateLimiters::new(&config);
        // Drain token-a's burst
        for _ in 0..2 {
            assert!(rl.check("token-a").is_ok());
        }
        assert!(rl.check("token-a").is_err());
        // token-b should still work
        assert!(rl.check("token-b").is_ok());
    }

    #[test]
    fn rate_limiter_disabled_allows_all() {
        let config = RateLimitConfig {
            requests_per_minute: 1,
            burst: 1,
            enabled: false,
        };
        let rl = ClientRateLimiters::new(&config);
        for _ in 0..100 {
            assert!(rl.check("token-c").is_ok());
        }
    }

    #[tokio::test]
    async fn with_safeguard_timeout() {
        let sg = QuerySafeguards::default_server();
        let result: Result<(), Status> = with_safeguard(
            "brain_search",
            &sg,
            Some(Duration::from_millis(50)),
            async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(())
            },
        )
        .await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
    }

    #[tokio::test]
    async fn with_safeguard_success() {
        let sg = QuerySafeguards::default_server();
        let result = with_safeguard("brain_search", &sg, None, async { Ok(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn with_safeguard_cancellable_sets_flag_on_timeout() {
        let sg = QuerySafeguards::default_server();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result: Result<(), Status> = with_safeguard_cancellable(
            "brain_search",
            &sg,
            Some(Duration::from_millis(50)),
            cancel.clone(),
            async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(())
            },
        )
        .await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DeadlineExceeded);
        assert!(
            cancel.load(std::sync::atomic::Ordering::Acquire),
            "a timeout must set the cancellation flag so the spawn_blocking work bails"
        );
    }

    #[tokio::test]
    async fn with_safeguard_cancellable_success_leaves_flag_unset() {
        let sg = QuerySafeguards::default_server();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result =
            with_safeguard_cancellable("brain_search", &sg, None, cancel.clone(), async { Ok(42) })
                .await;
        assert_eq!(result.unwrap(), 42);
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
    }
}

//! Runtime handle for a single upstream NestWeaver server.
//!
//! Wraps a `tonic::Channel` with bearer token injection, health state,
//! timeout, routing mode, and repo glob matching.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::discovery::{RoutingMode, UpstreamConfig};

/// Smoothing factor for the latency EWMA. A small alpha keeps the estimate
/// stable against one-off spikes while still tracking sustained drift — the
/// whole point of an *adaptive* timeout (static thresholds rot as fleet
/// latency moves, per the InfoQ adaptive-hedging finding).
const LATENCY_EWMA_ALPHA: f64 = 0.2;

// ── Upstream health: passive mark-down + active recovery ──────────────────
//
// Health follows the circuit-breaker / outlier-detection pattern used by
// Envoy (outlier detection), HAProxy (passive checks paired with active
// re-probing) and clangd's remote index: a live query failure *ejects* an
// upstream (passive), and a background task periodically *re-probes* an
// ejected upstream and, on `RISE_THRESHOLD` consecutive successes, restores
// it to rotation (active recovery). Ejection duration escalates with the
// consecutive-ejection count (Envoy backoff) so a chronically-dead host is
// not hammered while a transient blip recovers fast. Passive marking is
// NEVER shipped without the active re-probe — that is the bug this fixes.

/// Process monotonic clock base for health scheduling. Ejection deadlines are
/// stored as milliseconds since this base so the passive mark-down on the
/// query path and the background probe loop share one timeline without
/// needing a wall clock (which can jump).
static CLOCK_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Current monotonic milliseconds since the process clock base.
pub fn now_ms() -> u64 {
    CLOCK_BASE.elapsed().as_millis() as u64
}

/// Base ejection window (Envoy `base_ejection_time`). Multiplied by the
/// consecutive-ejection count and capped at [`EJECTION_CAP_MS`].
pub const EJECTION_BASE_MS: u64 = 30_000;

/// Upper bound on the ejection window so a chronically-dead upstream is still
/// re-probed every few minutes rather than never.
pub const EJECTION_CAP_MS: u64 = 300_000;

/// Consecutive successful probes required to return an ejected upstream to
/// rotation (HAProxy `rise` — hysteresis against flapping).
pub const RISE_THRESHOLD: u32 = 2;

/// Never eject more than this percentage of upstreams at once (Envoy
/// `max_ejection_percent`). One correlated network blip must not force the
/// whole session local-only; at least one upstream may always be ejected.
pub const MAX_EJECTION_PERCENT: u32 = 50;

/// Ejection window for the `n`th consecutive ejection: `base * n`, capped.
pub fn ejection_backoff_ms(ejection_count: u32) -> u64 {
    let count = ejection_count.max(1) as u64;
    EJECTION_BASE_MS.saturating_mul(count).min(EJECTION_CAP_MS)
}

/// Maximum number of upstreams that may be ejected simultaneously given
/// `total` upstreams and a `max_percent` cap. Always at least 1 so a single
/// genuinely-dead upstream can still be removed.
pub fn ejection_cap(total: usize, max_percent: u32) -> usize {
    ((total.saturating_mul(max_percent as usize)) / 100).max(1)
}

/// Whether ejecting one more upstream stays within the blast-radius cap.
pub fn can_eject(total: usize, currently_ejected: usize, max_percent: u32) -> bool {
    currently_ejected < ejection_cap(total, max_percent)
}

/// Outcome of folding an active-probe result into a [`HealthState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Probe succeeded and the upstream is back in rotation.
    Recovered,
    /// Probe succeeded but more consecutive successes are needed (`rise`).
    Improving,
    /// Probe failed; the upstream stays ejected with an escalated deadline.
    StillDown,
}

/// Lock-free health state for a single upstream.
///
/// `healthy` is the flag routing reads. `ejected_until_ms` is a monotonic
/// deadline before which no active probe fires (backoff). `ejection_count`
/// drives the escalating backoff, and `rise` counts consecutive successful
/// probes toward recovery. All transitions are best-effort under concurrency
/// (a lost race just delays a verdict), which is acceptable for health.
#[derive(Debug)]
pub struct HealthState {
    healthy: AtomicBool,
    ejection_count: AtomicU32,
    rise: AtomicU32,
    ejected_until_ms: AtomicU64,
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthState {
    pub fn new() -> Self {
        Self {
            healthy: AtomicBool::new(true),
            ejection_count: AtomicU32::new(0),
            rise: AtomicU32::new(0),
            ejected_until_ms: AtomicU64::new(0),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn ejection_count(&self) -> u32 {
        self.ejection_count.load(Ordering::Relaxed)
    }

    pub fn ejected_until_ms(&self) -> u64 {
        self.ejected_until_ms.load(Ordering::Relaxed)
    }

    /// Passive mark-down: a live query to this upstream failed. Ejects it with
    /// an escalating backoff window. NEVER permanent — active recovery (the
    /// background probe) is what restores it.
    pub fn mark_down(&self, now_ms: u64) {
        self.healthy.store(false, Ordering::Relaxed);
        self.rise.store(0, Ordering::Relaxed);
        let count = self.ejection_count.fetch_add(1, Ordering::Relaxed) + 1;
        self.ejected_until_ms.store(
            now_ms.saturating_add(ejection_backoff_ms(count)),
            Ordering::Relaxed,
        );
    }

    /// Force the upstream healthy and reset all ejection accounting. Used on
    /// successful recovery and for administrative resets.
    pub fn mark_up(&self) {
        self.healthy.store(true, Ordering::Relaxed);
        self.ejection_count.store(0, Ordering::Relaxed);
        self.rise.store(0, Ordering::Relaxed);
        self.ejected_until_ms.store(0, Ordering::Relaxed);
    }

    /// Whether an active recovery probe is due now: the upstream is ejected
    /// and its backoff window has elapsed.
    pub fn probe_due(&self, now_ms: u64) -> bool {
        !self.is_healthy() && now_ms >= self.ejected_until_ms()
    }

    /// Fold an active-probe result. On [`RISE_THRESHOLD`] consecutive
    /// successes the upstream returns to rotation; on failure the backoff
    /// escalates and it stays ejected.
    pub fn apply_probe_result(&self, now_ms: u64, success: bool) -> ProbeOutcome {
        if success {
            let r = self.rise.fetch_add(1, Ordering::Relaxed) + 1;
            if r >= RISE_THRESHOLD {
                self.mark_up();
                ProbeOutcome::Recovered
            } else {
                ProbeOutcome::Improving
            }
        } else {
            self.rise.store(0, Ordering::Relaxed);
            let count = self.ejection_count.fetch_add(1, Ordering::Relaxed) + 1;
            self.ejected_until_ms.store(
                now_ms.saturating_add(ejection_backoff_ms(count)),
                Ordering::Relaxed,
            );
            ProbeOutcome::StillDown
        }
    }
}

/// Pure TLS decision for [`configure_endpoint_tls`] (nw-833). Takes the
/// system-roots probe result as input so it never reads the host trust store.
///
/// * `ca` set: trust ONLY that CA (pinning, exactly as before nw-833).
/// * no `ca`, native roots available: trust the system roots.
/// * no `ca`, no native roots: a clear error naming the fix.
fn build_tls(
    ca: Option<tonic::transport::Certificate>,
    native_roots_available: bool,
) -> Result<tonic::transport::ClientTlsConfig> {
    let tls = tonic::transport::ClientTlsConfig::new();
    match ca {
        Some(ca) => Ok(tls.ca_certificate(ca)),
        None if native_roots_available => Ok(tls.with_native_roots()),
        None => anyhow::bail!(
            "no system CA roots were found to verify this grpcs:// upstream; install your \
             platform's CA certificates, or pass --ca-cert <pem> (ca_cert in config, \
             NESTWEAVER_CA_CERT in the environment)"
        ),
    }
}

/// Configure TLS on `endpoint` (nw-833).
///
/// A `ca_cert` PEM is the ONLY trust anchor for that upstream (pinned, as
/// before). An `https://` endpoint with no `ca_cert` verifies against the
/// system roots, so `grpcs://host` works against a publicly trusted
/// certificate; if the host has no system roots that is an error naming
/// `--ca-cert`. Plain `http://` endpoints without a `ca_cert` are untouched.
///
/// Shared by [`UpstreamHandle::from_config`] and `nestweaver connect` so the
/// two never disagree about how a `grpcs://` URL is trusted.
pub fn configure_endpoint_tls(
    endpoint: tonic::transport::Endpoint,
    url: &str,
    ca_cert: Option<&std::path::Path>,
) -> Result<tonic::transport::Endpoint> {
    if ca_cert.is_none() && !url.starts_with("https://") {
        return Ok(endpoint);
    }
    let ca = ca_cert
        .map(|ca_path| {
            std::fs::read(ca_path)
                .with_context(|| format!("failed to read CA cert: {}", ca_path.display()))
                .map(tonic::transport::Certificate::from_pem)
        })
        .transpose()?;
    // Probe only when it matters: tonic loads the native store while building
    // the connector, so a trial config tells us whether any roots exist.
    let roots = ca.is_none()
        && endpoint
            .clone()
            .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
            .is_ok();
    endpoint
        .tls_config(build_tls(ca, roots)?)
        .context("TLS config failed")
}

/// Default cap on the `fallback`-mode upstream deadline. In Fallback mode the
/// local index is the fast path and the server is consulted only when local
/// results are sparse, so by default the upstream must not block the local
/// answer past the product's <200ms budget. Per-upstream `fallback_timeout`
/// overrides it (nw-834); `timeout` always bounds it.
pub const DEFAULT_FALLBACK_CAP: Duration = Duration::from_millis(200);

/// Latency EWMA plus the consecutive-ceiling-timeout streak. They share one
/// cell so a detached query future clears the streak and folds the sample in a
/// single call ([`record_latency_into`]).
#[derive(Default)]
pub struct LatencyCell {
    /// EWMA in ms as `f64` bits; `0` is the cold-start sentinel.
    ewma_bits: AtomicU64,
    /// Consecutive timeouts at the full mode ceiling (nw-905).
    streak: AtomicU32,
}

impl LatencyCell {
    pub fn ewma_ms(&self) -> Option<f64> {
        match self.ewma_bits.load(Ordering::Relaxed) {
            0 => None,
            bits => Some(f64::from_bits(bits)),
        }
    }
}

/// Runtime handle for a single upstream NestWeaver server.
pub struct UpstreamHandle {
    pub name: String,
    channel: Channel,
    connected: tokio::sync::Mutex<bool>,
    query_outcome: AtomicU8,
    pub vault_mappings: crate::dedup::VaultMappings,
    token: Option<String>,
    repo_globs: Vec<glob::Pattern>,
    pub mode: RoutingMode,
    pub timeout: Duration,
    /// Ceiling of the adaptive deadline in `fallback` mode (nw-834):
    /// `fallback_timeout` (default 200ms), never above `timeout`.
    fallback_cap: Duration,
    health: Arc<HealthState>,
    /// EWMA (in milliseconds) of observed *successful* upstream RPC latencies,
    /// encoded as `f64` bits inside an `AtomicU64` for lock-free interior
    /// mutability. A stored value of `0.0` (bits `0`) is the cold-start
    /// sentinel meaning "no samples yet". Feeds the mode-aware adaptive
    /// timeout (see [`crate::health::effective_timeout`]).
    latency_ewma: Arc<LatencyCell>,
    tool_latencies: Mutex<HashMap<String, Arc<LatencyCell>>>,
}

/// Connection establishment has its own bounded budget, independent of the
/// adaptive query deadline. TLS handshakes must not consume a 50ms warm RPC budget.
pub const COLD_CONNECT_BUDGET: Duration = Duration::from_secs(5);

/// Observable outcome of the latest routed query; skipped is distinct from
/// a completed empty answer, timeout, rejection, or transport failure.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum QueryOutcome {
    Skipped,
    Success,
    Timeout,
    Rejected,
    Unavailable,
}

impl UpstreamHandle {
    pub fn record_query_outcome(&self, outcome: QueryOutcome) {
        self.query_outcome.store(outcome as u8, Ordering::Relaxed);
    }

    pub fn query_outcome(&self) -> &'static str {
        match self.query_outcome.load(Ordering::Relaxed) {
            1 => "success",
            2 => "timeout",
            3 => "rejected",
            4 => "unavailable",
            _ => "skipped",
        }
    }

    /// Establish the lazy channel once under a separate connection budget.
    /// The lock prevents concurrent cold calls from multiplying handshakes.
    pub async fn ensure_connected(&self) -> Result<()> {
        tokio::time::timeout(COLD_CONNECT_BUDGET, async {
            let mut connected = self.connected.lock().await;
            if *connected {
                return Ok(());
            }
            let mut client = self.client();
            let mut req = tonic::Request::new(nestweaver_proto::HealthCheckRequest {});
            self.inject_auth(&mut req);
            client
                .health_check(req)
                .await
                .context("upstream cold-connect health check failed")?;
            *connected = true;
            Ok(())
        })
        .await
        .context("upstream cold-connect timed out")?
    }

    /// Create from a discovered config. Uses `connect_lazy` so the channel
    /// doesn't block during construction — the first RPC triggers the real
    /// TCP handshake.
    pub fn from_config(config: &UpstreamConfig) -> Result<Self> {
        // Normalize URL for tonic (needs http/https scheme).
        let url = normalize_url(&config.url);

        // Parsed once and reused for both the per-RPC deadline below and the
        // stored `timeout` (same input + fallback).
        let per_rpc_timeout = parse_duration(&config.timeout).unwrap_or(Duration::from_secs(1));
        let mut endpoint = Channel::from_shared(url.clone())
            .context("invalid upstream URL")?
            .connect_timeout(Duration::from_secs(5))
            // Transport backstop strictly ABOVE every client-side deadline
            // (the tokio timeout in the query paths never exceeds `timeout`),
            // so the client deadline always fires first and feeds
            // `health::handle_query_timeout` instead of racing a transport
            // `DeadlineExceeded` that would eject immediately (nw-905).
            .timeout(per_rpc_timeout.max(COLD_CONNECT_BUDGET) + Duration::from_secs(1));

        endpoint = configure_endpoint_tls(
            endpoint,
            &url,
            config.ca_cert.as_deref().map(std::path::Path::new),
        )?;

        let channel = endpoint.connect_lazy();

        let patterns: Vec<_> = config
            .repos
            .iter()
            .filter_map(|g| glob::Pattern::new(g).ok())
            .collect();

        let timeout = per_rpc_timeout;
        let default_cap = DEFAULT_FALLBACK_CAP;
        let fallback_cap = match config.fallback_timeout.as_deref() {
            None => default_cap,
            Some(raw) => parse_duration(raw).unwrap_or_else(|| {
                tracing::warn!(
                    fallback_timeout = raw,
                    "unparseable fallback_timeout; using the 200ms default"
                );
                default_cap
            }),
        }
        .min(timeout);

        Ok(Self {
            name: config
                .name
                .clone()
                .unwrap_or_else(|| "upstream".to_string()),
            channel,
            connected: tokio::sync::Mutex::new(false),
            query_outcome: AtomicU8::new(0),
            vault_mappings: config.vault_mappings.clone(),
            token: config.token.clone(),
            repo_globs: patterns,
            mode: config.mode,
            timeout,
            fallback_cap,
            health: Arc::new(HealthState::new()),
            latency_ewma: Arc::new(LatencyCell::default()),
            tool_latencies: Mutex::new(HashMap::new()),
        })
    }

    /// Returns a tonic client for this upstream (clones the channel cheaply).
    pub fn client(
        &self,
    ) -> nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient<Channel> {
        nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient::new(
            self.channel.clone(),
        )
        .max_decoding_message_size(256 * 1024 * 1024)
        .max_encoding_message_size(256 * 1024 * 1024)
    }

    /// Returns the bearer token for this upstream, if configured.
    pub fn auth_token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Inject bearer token into a tonic request.
    pub fn inject_auth<T>(&self, req: &mut tonic::Request<T>) {
        if let Some(ref token) = self.token
            && let Ok(val) = format!("Bearer {}", token).parse::<MetadataValue<_>>()
        {
            req.metadata_mut().insert("authorization", val);
        }
    }

    /// Check if this upstream should handle queries for the given repo URL.
    /// Empty globs = handle everything.
    pub fn matches_repo(&self, repo_url: &str) -> bool {
        if self.repo_globs.is_empty() {
            return true;
        }
        self.repo_globs.iter().any(|g| g.matches(repo_url))
    }

    pub fn is_healthy(&self) -> bool {
        self.health.is_healthy()
    }

    /// Passive mark-down (a live query failed). Ejects with escalating backoff;
    /// the background probe task restores it — this is never permanent.
    pub fn mark_unhealthy(&self) {
        if let Ok(mut connected) = self.connected.try_lock() {
            *connected = false;
        }
        self.health.mark_down(now_ms());
    }

    /// Get a clone of the shared health state for use in background tasks.
    pub fn health_ref(&self) -> Arc<HealthState> {
        Arc::clone(&self.health)
    }

    /// Fold an observed *successful* RPC duration into the latency EWMA.
    /// A success also clears the ceiling-timeout streak (nw-905).
    pub fn record_latency(&self, observed: Duration) {
        self.record_query_outcome(QueryOutcome::Success);
        record_latency_into(&self.latency_ewma, observed);
    }

    /// Latency for one tool only. An unseen tool has no learned deadline.
    pub fn tool_latency_ewma_ms(&self, tool: &str) -> Option<f64> {
        self.tool_latencies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(tool)
            .and_then(|cell| cell.ewma_ms())
    }

    fn tool_latency_cell(&self, tool: &str) -> Arc<LatencyCell> {
        Arc::clone(
            self.tool_latencies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(tool.to_owned())
                .or_default(),
        )
    }

    /// Success learns only this tool's cost, but resets the upstream-wide
    /// ceiling-timeout streak, preserving the any-success recovery policy.
    pub fn record_tool_latency(&self, tool: &str, observed: Duration) {
        record_latency_into(&self.tool_latency_cell(tool), observed);
        self.reset_timeout_streak();
        self.record_query_outcome(QueryOutcome::Success);
    }

    pub fn record_tool_timeout(&self, tool: &str, deadline: Duration, at_ceiling: bool) -> u32 {
        learn_timeout_into(&self.tool_latency_cell(tool), deadline);
        if at_ceiling {
            self.latency_ewma.streak.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            0
        }
    }

    /// Ceiling of the adaptive deadline in `fallback` mode (nw-834).
    pub fn fallback_cap(&self) -> Duration {
        self.fallback_cap
    }

    /// Clear the ceiling-timeout streak.
    pub fn reset_timeout_streak(&self) {
        self.latency_ewma.streak.store(0, Ordering::Relaxed);
    }

    /// Learn from a timeout (nw-905). A timed-out call only proves the true
    /// latency is AT LEAST `deadline`, so the estimate is raised to it (never
    /// lowered) — the next adaptive deadline widens by `TIMEOUT_K` instead of
    /// staying pinned at the floor. Returns the new count of consecutive
    /// timeouts that hit `at_ceiling` (a deadline that could not widen
    /// further); a miss below the ceiling returns 0 and never counts toward
    /// ejection.
    pub fn record_timeout(&self, deadline: Duration, at_ceiling: bool) -> u32 {
        learn_timeout_into(&self.latency_ewma, deadline);
        if at_ceiling {
            self.latency_ewma.streak.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            0
        }
    }

    /// Current latency EWMA in milliseconds, or `None` on a cold start
    /// (no successful samples observed yet).
    pub fn latency_ewma_ms(&self) -> Option<f64> {
        self.latency_ewma.ewma_ms()
    }

    /// Clone the latency cell so latency can be recorded from a detached query
    /// future that cannot hold a borrow of the handle (e.g. the parallel
    /// merge path). Update it with [`record_latency_into`].
    pub fn latency_ewma_ref(&self) -> Arc<LatencyCell> {
        Arc::clone(&self.latency_ewma)
    }
}

fn learn_timeout_into(cell: &LatencyCell, deadline: Duration) {
    let deadline_ms = deadline.as_secs_f64() * 1000.0;
    if deadline_ms > cell.ewma_ms().unwrap_or(0.0) {
        cell.ewma_bits
            .store(deadline_ms.to_bits(), Ordering::Relaxed);
    }
}

/// Fold an observed successful duration into the cell's EWMA and clear the
/// timeout streak. On a cold start the first observation seeds the estimate
/// directly; thereafter it is smoothed by [`LATENCY_EWMA_ALPHA`]. Lossy under
/// concurrent writers (a race just drops a sample), which is fine for a
/// latency estimate.
pub fn record_latency_into(cell: &LatencyCell, observed: Duration) {
    let sample_ms = observed.as_secs_f64() * 1000.0;
    let prev_bits = cell.ewma_bits.load(Ordering::Relaxed);
    let next = if prev_bits == 0 {
        sample_ms
    } else {
        let prev = f64::from_bits(prev_bits);
        LATENCY_EWMA_ALPHA * sample_ms + (1.0 - LATENCY_EWMA_ALPHA) * prev
    };
    cell.ewma_bits.store(next.to_bits(), Ordering::Relaxed);
    cell.streak.store(0, Ordering::Relaxed);
}

/// Parse a human-readable duration string like "1s", "500ms", or bare seconds.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix("s").filter(|r| !r.ends_with("m")) {
        secs.trim().parse::<f64>().ok().map(Duration::from_secs_f64)
    } else if let Some(ms) = s.strip_suffix("ms") {
        ms.trim().parse::<u64>().ok().map(Duration::from_millis)
    } else {
        s.parse::<u64>().ok().map(Duration::from_secs)
    }
}

/// Normalize a URL for tonic — needs http:// or https:// scheme.
fn normalize_url(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with("grpcs://") {
        url.replacen("grpcs://", "https://", 1)
    } else if url.starts_with("grpc://") {
        url.replacen("grpc://", "http://", 1)
    } else {
        format!("http://{}", url)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("3s"), Some(Duration::from_secs(3)));
    }

    #[test]
    fn parse_duration_millis() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("100ms"), Some(Duration::from_millis(100)));
    }

    #[test]
    fn parse_duration_bare_number() {
        assert_eq!(parse_duration("2"), Some(Duration::from_secs(2)));
    }

    #[test]
    fn parse_duration_invalid() {
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn parse_duration_fractional_seconds() {
        assert_eq!(parse_duration("0.5s"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("1.75s"), Some(Duration::from_millis(1750)));
    }

    #[test]
    fn parse_duration_small_millis() {
        // Values used as the adaptive-timeout floor / Fallback cap.
        assert_eq!(parse_duration("50ms"), Some(Duration::from_millis(50)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
    }

    #[test]
    fn record_latency_cold_start_seeds_directly() {
        let slot = LatencyCell::default();
        record_latency_into(&slot, Duration::from_millis(80));
        // First sample seeds the EWMA exactly (no smoothing yet).
        let ms = slot.ewma_ms().unwrap();
        assert!((ms - 80.0).abs() < 1e-9, "expected ~80ms, got {ms}");
    }

    #[test]
    fn record_latency_smooths_subsequent_samples() {
        let slot = LatencyCell::default();
        record_latency_into(&slot, Duration::from_millis(100));
        record_latency_into(&slot, Duration::from_millis(200));
        // EWMA = alpha*200 + (1-alpha)*100 = 0.2*200 + 0.8*100 = 120.
        let ms = slot.ewma_ms().unwrap();
        assert!((ms - 120.0).abs() < 1e-9, "expected ~120ms, got {ms}");
    }

    #[tokio::test]
    async fn cold_connect_has_a_separate_bounded_budget_and_no_rpc_latency_sample() {
        // A real TCP listener that never speaks HTTP/2 holds connection setup.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let config = UpstreamConfig {
            name: Some("cold".into()),
            url: format!("http://{}", listener.local_addr().unwrap()),
            token: None,
            mode: RoutingMode::Fallback,
            repos: vec![],
            timeout: "30ms".into(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let handle = UpstreamHandle::from_config(&config).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), handle.ensure_connected())
                .await
                .is_err(),
            "a 30ms RPC ceiling must not abort cold connection establishment"
        );
        let result = tokio::time::timeout(
            COLD_CONNECT_BUDGET + Duration::from_secs(1),
            handle.ensure_connected(),
        )
        .await;
        assert!(
            result.is_ok(),
            "cold connection must finish within its own finite budget"
        );
        assert!(result.unwrap().is_err());
        assert_eq!(
            handle.latency_ewma_ms(),
            None,
            "handshake latency is not query latency"
        );
        assert!(
            !*handle.connected.lock().await,
            "a failed handshake must remain cold"
        );
        assert_eq!(handle.query_outcome(), "skipped");
        handle.record_query_outcome(QueryOutcome::Timeout);
        assert_eq!(handle.query_outcome(), "timeout");
        handle.record_latency(Duration::from_millis(10));
        assert_eq!(handle.query_outcome(), "success");
    }

    #[tokio::test]
    async fn handle_latency_ewma_cold_then_warm() {
        let config = UpstreamConfig {
            name: Some("lat".to_string()),
            url: "http://127.0.0.1:19999".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "1s".to_string(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let handle = UpstreamHandle::from_config(&config).unwrap();
        // Cold start: no samples yet.
        assert_eq!(handle.latency_ewma_ms(), None);
        handle.record_latency(Duration::from_millis(60));
        assert_eq!(handle.latency_ewma_ms(), Some(60.0));
        // Recording through a cloned cell updates the same EWMA.
        record_latency_into(&handle.latency_ewma_ref(), Duration::from_millis(160));
        // EWMA = 0.2*160 + 0.8*60 = 80.
        let ms = handle.latency_ewma_ms().unwrap();
        assert!((ms - 80.0).abs() < 1e-9, "expected ~80ms, got {ms}");
    }

    #[tokio::test]
    async fn from_config_honors_explicit_timeout() {
        let config = UpstreamConfig {
            name: Some("slow".to_string()),
            url: "http://127.0.0.1:19999".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "200ms".to_string(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let handle = UpstreamHandle::from_config(&config).unwrap();
        assert_eq!(handle.timeout, Duration::from_millis(200));
    }

    #[test]
    fn glob_matching_wildcard() {
        let pattern = glob::Pattern::new("acme/*").unwrap();
        assert!(pattern.matches("acme/billing"));
        assert!(pattern.matches("acme/api"));
        assert!(!pattern.matches("other/repo"));
    }

    #[test]
    fn glob_matching_exact() {
        let pattern = glob::Pattern::new("acme/billing").unwrap();
        assert!(pattern.matches("acme/billing"));
        assert!(!pattern.matches("acme/api"));
    }

    #[test]
    fn glob_matching_star() {
        let pattern = glob::Pattern::new("*").unwrap();
        assert!(pattern.matches("anything"));
    }

    #[test]
    fn health_state_toggle() {
        let healthy = Arc::new(AtomicBool::new(true));
        assert!(healthy.load(Ordering::Relaxed));
        healthy.store(false, Ordering::Relaxed);
        assert!(!healthy.load(Ordering::Relaxed));
        healthy.store(true, Ordering::Relaxed);
        assert!(healthy.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn from_config_connect_lazy() {
        // connect_lazy doesn't actually connect — just creates the channel.
        let config = UpstreamConfig {
            name: Some("test".to_string()),
            url: "http://127.0.0.1:19999".to_string(),
            token: Some("nw_test".to_string()),
            repos: vec!["acme/*".to_string()],
            mode: RoutingMode::Fallback,
            timeout: "1s".to_string(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let handle = UpstreamHandle::from_config(&config).unwrap();
        assert_eq!(handle.name, "test");
        assert!(handle.is_healthy());
        assert_eq!(handle.timeout, Duration::from_secs(1));
        assert!(handle.matches_repo("acme/billing"));
        assert!(!handle.matches_repo("other/repo"));
    }

    #[tokio::test]
    async fn empty_globs_match_everything() {
        let config = UpstreamConfig {
            name: None,
            url: "http://127.0.0.1:19999".to_string(),
            token: None,
            repos: vec![],
            mode: RoutingMode::default(),
            timeout: "1s".to_string(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let handle = UpstreamHandle::from_config(&config).unwrap();
        assert!(handle.matches_repo("anything/at/all"));
    }

    // ── Health state machine (active recovery, Seam A) ────────────────

    #[test]
    fn single_failure_does_not_permanently_disable() {
        // A single passive mark-down must NOT latch the upstream unhealthy
        // forever. After the ejection window elapses, a successful active
        // probe (×rise) restores it to rotation.
        let state = HealthState::new();
        assert!(state.is_healthy());

        // One live-query failure ejects the upstream (passive mark-down).
        state.mark_down(0);
        assert!(!state.is_healthy(), "single failure ejects");

        // Before the backoff window elapses, no active probe is due.
        let backoff = ejection_backoff_ms(1);
        assert!(!state.probe_due(backoff - 1), "still in backoff window");
        assert!(state.probe_due(backoff), "probe due once window elapses");

        // A later successful probe brings it back (rise = 2 successes).
        assert_eq!(
            state.apply_probe_result(backoff, true),
            ProbeOutcome::Improving
        );
        assert!(
            !state.is_healthy(),
            "one success not enough (rise hysteresis)"
        );
        assert_eq!(
            state.apply_probe_result(backoff + 1, true),
            ProbeOutcome::Recovered
        );
        assert!(state.is_healthy(), "recovered after rise successes");
    }

    #[test]
    fn unhealthy_upstream_recovers_after_probe() {
        // Eject, then drive one background probe tick against a now-healthy
        // stub (success=true). After `rise` successes it returns to rotation.
        let state = HealthState::new();
        state.mark_down(1_000);
        assert!(!state.is_healthy());

        let due_at = state.ejected_until_ms();
        assert!(state.probe_due(due_at));
        // First tick: probe succeeds but rise threshold not yet met.
        state.apply_probe_result(due_at, true);
        assert!(!state.is_healthy());
        // Second tick: still eligible to probe (window already passed).
        assert!(state.probe_due(due_at));
        state.apply_probe_result(due_at, true);
        assert!(state.is_healthy(), "back in rotation after two good probes");
        // Recovery resets ejection accounting so the next blip starts fresh.
        assert_eq!(state.ejection_count(), 0);
    }

    #[test]
    fn failed_probe_keeps_upstream_ejected_and_reschedules() {
        let state = HealthState::new();
        state.mark_down(0);
        let first_deadline = state.ejected_until_ms();
        // A failed probe does not restore, and pushes the next deadline out.
        assert_eq!(
            state.apply_probe_result(first_deadline, false),
            ProbeOutcome::StillDown
        );
        assert!(!state.is_healthy());
        assert!(
            state.ejected_until_ms() > first_deadline,
            "failed probe escalates the backoff deadline"
        );
    }

    #[test]
    fn mark_healthy_resets_ejection_state() {
        let state = HealthState::new();
        state.mark_down(0);
        state.mark_down(0);
        assert!(state.ejection_count() >= 2);
        state.mark_up();
        assert!(state.is_healthy());
        assert_eq!(state.ejection_count(), 0);
        assert_eq!(state.ejected_until_ms(), 0);
    }

    // ── Ejection backoff + blast-radius cap (Seam C) ──────────────────

    #[test]
    fn ejection_backoff_escalates_and_caps() {
        // base(30s) * consecutive-ejection-count, capped at 300s.
        assert_eq!(ejection_backoff_ms(1), 30_000);
        assert_eq!(ejection_backoff_ms(2), 60_000);
        assert_eq!(ejection_backoff_ms(3), 90_000);
        assert_eq!(ejection_backoff_ms(10), 300_000); // 30*10 == cap
        assert_eq!(ejection_backoff_ms(11), 300_000); // capped
        assert_eq!(ejection_backoff_ms(1000), 300_000);
        // count 0 is treated as 1 (never a zero-length window).
        assert_eq!(ejection_backoff_ms(0), 30_000);

        // Repeated ejections escalate the scheduled deadline.
        let state = HealthState::new();
        state.mark_down(0);
        let d1 = state.ejected_until_ms();
        state.mark_down(0);
        let d2 = state.ejected_until_ms();
        assert!(d2 > d1, "second ejection escalates backoff ({d1} -> {d2})");
    }

    #[test]
    fn mass_ejection_is_capped() {
        // Envoy max_ejection_percent = 50%.
        assert_eq!(ejection_cap(4, 50), 2);
        assert_eq!(ejection_cap(2, 50), 1);
        assert_eq!(ejection_cap(1, 50), 1); // always at least one
        assert_eq!(ejection_cap(0, 50), 1);

        // A single upstream may always be ejected.
        assert!(can_eject(1, 0, 50));
        // Half already ejected → can't take the last one down.
        assert!(!can_eject(2, 1, 50));
        // Room remains below the cap.
        assert!(can_eject(4, 0, 50));
        assert!(can_eject(4, 1, 50));
        assert!(!can_eject(4, 2, 50)); // at the cap, no more
        assert!(!can_eject(4, 3, 50));
    }

    #[test]
    fn normalize_url_variants() {
        assert_eq!(normalize_url("http://foo:9378"), "http://foo:9378");
        assert_eq!(normalize_url("https://foo:9378"), "https://foo:9378");
        assert_eq!(normalize_url("grpcs://foo:9378"), "https://foo:9378");
        assert_eq!(normalize_url("grpc://foo:9378"), "http://foo:9378");
        assert_eq!(normalize_url("foo:9378"), "http://foo:9378");
    }

    #[tokio::test]
    async fn record_timeout_widens_estimate_and_counts_only_ceiling_misses() {
        // nw-905: a timeout is a censored sample; it raises the EWMA.
        let cfg = UpstreamConfig {
            name: Some("t".into()),
            url: "http://127.0.0.1:19999".into(),
            token: None,
            repos: vec![],
            mode: RoutingMode::Merge,
            timeout: "1s".into(),
            ca_cert: None,
            fallback_timeout: None,
            vault_mappings: Default::default(),
        };
        let h = UpstreamHandle::from_config(&cfg).unwrap();
        h.record_latency(Duration::from_millis(8));
        assert_eq!(h.record_timeout(Duration::from_millis(50), false), 0);
        assert_eq!(h.latency_ewma_ms(), Some(50.0));
        // A later, smaller deadline never lowers the estimate.
        h.record_timeout(Duration::from_millis(20), false);
        assert_eq!(h.latency_ewma_ms(), Some(50.0));
        assert_eq!(h.record_timeout(Duration::from_secs(1), true), 1);
        assert_eq!(h.record_timeout(Duration::from_secs(1), true), 2);
        // Any success clears the streak.
        h.record_latency(Duration::from_millis(30));
        assert_eq!(h.record_timeout(Duration::from_secs(1), true), 1);
    }

    #[tokio::test]
    async fn fallback_cap_defaults_to_200ms_and_honors_fallback_timeout() {
        // nw-834: "1s" is no longer a magic sentinel; the cap is its own key.
        let mk = |timeout: &str, cap: Option<&str>| {
            UpstreamHandle::from_config(&UpstreamConfig {
                name: None,
                url: "http://127.0.0.1:19999".into(),
                token: None,
                repos: vec![],
                mode: RoutingMode::Fallback,
                timeout: timeout.into(),
                ca_cert: None,
                fallback_timeout: cap.map(String::from),
                vault_mappings: Default::default(),
            })
            .unwrap()
            .fallback_cap()
        };
        assert_eq!(mk("1s", None), Duration::from_millis(200));
        assert_eq!(mk("1000ms", None), Duration::from_millis(200));
        assert_eq!(mk("100ms", None), Duration::from_millis(100));
        assert_eq!(mk("1s", Some("750ms")), Duration::from_millis(750));
        assert_eq!(mk("1s", Some("5s")), Duration::from_secs(1));
        assert_eq!(mk("1s", Some("garbage")), Duration::from_millis(200));
    }

    // ── nw-833 TLS decision (host trust store is never read) ─────────────

    fn some_ca() -> tonic::transport::Certificate {
        tonic::transport::Certificate::from_pem("not a real pem; never parsed here")
    }

    #[test]
    fn ca_cert_pins_even_when_no_system_roots_exist() {
        assert!(build_tls(Some(some_ca()), false).is_ok());
        assert!(build_tls(Some(some_ca()), true).is_ok());
    }

    #[test]
    fn https_without_ca_cert_uses_system_roots_when_available() {
        assert!(build_tls(None, true).is_ok());
    }

    #[test]
    fn https_without_ca_cert_and_no_roots_names_the_fix() {
        let err = build_tls(None, false).unwrap_err().to_string();
        assert!(err.contains("--ca-cert"), "{err}");
        assert!(err.contains("NESTWEAVER_CA_CERT"), "{err}");
    }

    #[test]
    fn plain_http_without_ca_cert_leaves_endpoint_untouched() {
        let endpoint = Channel::from_static("http://127.0.0.1:1");
        assert!(configure_endpoint_tls(endpoint, "http://127.0.0.1:1", None).is_ok());
    }

    #[test]
    fn unreadable_ca_cert_is_a_named_error() {
        let endpoint = Channel::from_static("https://127.0.0.1:1");
        let err = configure_endpoint_tls(
            endpoint,
            "https://127.0.0.1:1",
            Some(std::path::Path::new("/nonexistent/ca.pem")),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("failed to read CA cert"));
    }
}

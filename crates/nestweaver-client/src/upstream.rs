//! Runtime handle for a single upstream NestWeaver server.
//!
//! Wraps a `tonic::Channel` with bearer token injection, health state,
//! timeout, routing mode, and repo glob matching.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
pub(crate) fn now_ms() -> u64 {
    CLOCK_BASE.elapsed().as_millis() as u64
}

/// Base ejection window (Envoy `base_ejection_time`). Multiplied by the
/// consecutive-ejection count and capped at [`EJECTION_CAP_MS`].
pub(crate) const EJECTION_BASE_MS: u64 = 30_000;

/// Upper bound on the ejection window so a chronically-dead upstream is still
/// re-probed every few minutes rather than never.
pub(crate) const EJECTION_CAP_MS: u64 = 300_000;

/// Consecutive successful probes required to return an ejected upstream to
/// rotation (HAProxy `rise` — hysteresis against flapping).
pub(crate) const RISE_THRESHOLD: u32 = 2;

/// Never eject more than this percentage of upstreams at once (Envoy
/// `max_ejection_percent`). One correlated network blip must not force the
/// whole session local-only; at least one upstream may always be ejected.
pub(crate) const MAX_EJECTION_PERCENT: u32 = 50;

/// Ejection window for the `n`th consecutive ejection: `base * n`, capped.
pub(crate) fn ejection_backoff_ms(ejection_count: u32) -> u64 {
    let count = ejection_count.max(1) as u64;
    EJECTION_BASE_MS.saturating_mul(count).min(EJECTION_CAP_MS)
}

/// Maximum number of upstreams that may be ejected simultaneously given
/// `total` upstreams and a `max_percent` cap. Always at least 1 so a single
/// genuinely-dead upstream can still be removed.
pub(crate) fn ejection_cap(total: usize, max_percent: u32) -> usize {
    ((total.saturating_mul(max_percent as usize)) / 100).max(1)
}

/// Whether ejecting one more upstream stays within the blast-radius cap.
pub(crate) fn can_eject(total: usize, currently_ejected: usize, max_percent: u32) -> bool {
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

/// Runtime handle for a single upstream NestWeaver server.
pub struct UpstreamHandle {
    pub name: String,
    channel: Channel,
    token: Option<String>,
    repo_globs: Vec<glob::Pattern>,
    pub mode: RoutingMode,
    pub timeout: Duration,
    health: Arc<HealthState>,
    /// EWMA (in milliseconds) of observed *successful* upstream RPC latencies,
    /// encoded as `f64` bits inside an `AtomicU64` for lock-free interior
    /// mutability. A stored value of `0.0` (bits `0`) is the cold-start
    /// sentinel meaning "no samples yet". Feeds the mode-aware adaptive
    /// timeout (see `hybrid::effective_timeout`).
    latency_ewma: Arc<AtomicU64>,
}

impl UpstreamHandle {
    /// Create from a discovered config. Uses `connect_lazy` so the channel
    /// doesn't block during construction — the first RPC triggers the real
    /// TCP handshake.
    pub fn from_config(config: &UpstreamConfig) -> Result<Self> {
        // Normalize URL for tonic (needs http/https scheme).
        let url = normalize_url(&config.url);

        let per_rpc_timeout = parse_duration(&config.timeout).unwrap_or(Duration::from_secs(1));
        let mut endpoint = Channel::from_shared(url)
            .context("invalid upstream URL")?
            .connect_timeout(Duration::from_secs(5))
            .timeout(per_rpc_timeout);

        if let Some(ref ca_path) = config.ca_cert {
            let pem = std::fs::read(ca_path)
                .with_context(|| format!("failed to read CA cert: {ca_path}"))?;
            let ca = tonic::transport::Certificate::from_pem(pem);
            let tls = tonic::transport::ClientTlsConfig::new().ca_certificate(ca);
            endpoint = endpoint.tls_config(tls).context("TLS config failed")?;
        }

        let channel = endpoint.connect_lazy();

        let patterns: Vec<_> = config
            .repos
            .iter()
            .filter_map(|g| glob::Pattern::new(g).ok())
            .collect();

        let timeout = parse_duration(&config.timeout).unwrap_or(Duration::from_secs(1));

        Ok(Self {
            name: config
                .name
                .clone()
                .unwrap_or_else(|| "upstream".to_string()),
            channel,
            token: config.token.clone(),
            repo_globs: patterns,
            mode: config.mode,
            timeout,
            health: Arc::new(HealthState::new()),
            latency_ewma: Arc::new(AtomicU64::new(0)),
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
        self.health.mark_down(now_ms());
    }

    /// Administrative/manual restore to rotation. The background recovery path
    /// uses [`HealthState::apply_probe_result`] instead.
    pub fn mark_healthy(&self) {
        self.health.mark_up();
    }

    /// Get a clone of the shared health state for use in background tasks.
    pub fn health_ref(&self) -> Arc<HealthState> {
        Arc::clone(&self.health)
    }

    /// Get the token (if any) for use in background health checks.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Fold an observed *successful* RPC duration into the latency EWMA.
    pub fn record_latency(&self, observed: Duration) {
        record_latency_into(&self.latency_ewma, observed);
    }

    /// Current latency EWMA in milliseconds, or `None` on a cold start
    /// (no successful samples observed yet).
    pub fn latency_ewma_ms(&self) -> Option<f64> {
        let bits = self.latency_ewma.load(Ordering::Relaxed);
        if bits == 0 {
            None
        } else {
            Some(f64::from_bits(bits))
        }
    }

    /// Clone the EWMA cell so latency can be recorded from a detached query
    /// future that cannot hold a borrow of the handle (e.g. the parallel
    /// merge path). Update it with [`record_latency_into`].
    pub fn latency_ewma_ref(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.latency_ewma)
    }
}

/// Fold an observed duration (in `slot`, encoded as `f64` millis bits) into an
/// EWMA. On a cold start (`slot == 0.0`) the first observation seeds the
/// estimate directly; thereafter it is smoothed by [`LATENCY_EWMA_ALPHA`].
/// Lossy under concurrent writers (a race just drops a sample), which is fine
/// for a latency estimate.
pub(crate) fn record_latency_into(slot: &AtomicU64, observed: Duration) {
    let sample_ms = observed.as_secs_f64() * 1000.0;
    let prev_bits = slot.load(Ordering::Relaxed);
    let next = if prev_bits == 0 {
        sample_ms
    } else {
        let prev = f64::from_bits(prev_bits);
        LATENCY_EWMA_ALPHA * sample_ms + (1.0 - LATENCY_EWMA_ALPHA) * prev
    };
    slot.store(next.to_bits(), Ordering::Relaxed);
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
        let slot = AtomicU64::new(0);
        record_latency_into(&slot, Duration::from_millis(80));
        // First sample seeds the EWMA exactly (no smoothing yet).
        let ms = f64::from_bits(slot.load(Ordering::Relaxed));
        assert!((ms - 80.0).abs() < 1e-9, "expected ~80ms, got {ms}");
    }

    #[test]
    fn record_latency_smooths_subsequent_samples() {
        let slot = AtomicU64::new(0);
        record_latency_into(&slot, Duration::from_millis(100));
        record_latency_into(&slot, Duration::from_millis(200));
        // EWMA = alpha*200 + (1-alpha)*100 = 0.2*200 + 0.8*100 = 120.
        let ms = f64::from_bits(slot.load(Ordering::Relaxed));
        assert!((ms - 120.0).abs() < 1e-9, "expected ~120ms, got {ms}");
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
}

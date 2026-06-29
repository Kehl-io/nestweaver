//! Runtime handle for a single upstream NestWeaver server.
//!
//! Wraps a `tonic::Channel` with bearer token injection, health state,
//! timeout, routing mode, and repo glob matching.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::discovery::{RoutingMode, UpstreamConfig};

/// Smoothing factor for the latency EWMA. A small alpha keeps the estimate
/// stable against one-off spikes while still tracking sustained drift — the
/// whole point of an *adaptive* timeout (static thresholds rot as fleet
/// latency moves, per the InfoQ adaptive-hedging finding).
const LATENCY_EWMA_ALPHA: f64 = 0.2;

/// Runtime handle for a single upstream NestWeaver server.
pub struct UpstreamHandle {
    pub name: String,
    channel: Channel,
    token: Option<String>,
    repo_globs: Vec<glob::Pattern>,
    pub mode: RoutingMode,
    pub timeout: Duration,
    healthy: Arc<AtomicBool>,
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
            healthy: Arc::new(AtomicBool::new(true)),
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
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn mark_unhealthy(&self) {
        self.healthy.store(false, Ordering::Relaxed);
    }

    pub fn mark_healthy(&self) {
        self.healthy.store(true, Ordering::Relaxed);
    }

    /// Get a clone of the health flag for use in background tasks.
    pub fn healthy_ref(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.healthy)
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

    #[test]
    fn normalize_url_variants() {
        assert_eq!(normalize_url("http://foo:9378"), "http://foo:9378");
        assert_eq!(normalize_url("https://foo:9378"), "https://foo:9378");
        assert_eq!(normalize_url("grpcs://foo:9378"), "https://foo:9378");
        assert_eq!(normalize_url("grpc://foo:9378"), "http://foo:9378");
        assert_eq!(normalize_url("foo:9378"), "http://foo:9378");
    }
}

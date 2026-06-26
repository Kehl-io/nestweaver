//! Runtime handle for a single upstream NestWeaver server.
//!
//! Wraps a `tonic::Channel` with bearer token injection, health state,
//! timeout, routing mode, and repo glob matching.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::discovery::{RoutingMode, UpstreamConfig};

/// Runtime handle for a single upstream NestWeaver server.
pub struct UpstreamHandle {
    pub name: String,
    channel: Channel,
    token: Option<String>,
    repo_globs: Vec<glob::Pattern>,
    pub mode: RoutingMode,
    pub timeout: Duration,
    healthy: Arc<AtomicBool>,
}

impl UpstreamHandle {
    /// Create from a discovered config. Uses `connect_lazy` so the channel
    /// doesn't block during construction — the first RPC triggers the real
    /// TCP handshake.
    pub fn from_config(config: &UpstreamConfig) -> Result<Self> {
        // Normalize URL for tonic (needs http/https scheme).
        let url = normalize_url(&config.url);

        let channel = Channel::from_shared(url)
            .context("invalid upstream URL")?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .connect_lazy();

        let patterns: Vec<_> = config
            .repos
            .iter()
            .filter_map(|g| glob::Pattern::new(g).ok())
            .collect();

        let timeout = parse_duration(&config.timeout).unwrap_or(Duration::from_secs(1));

        Ok(Self {
            name: config.name.clone().unwrap_or_else(|| "upstream".to_string()),
            channel,
            token: config.token.clone(),
            repo_globs: patterns,
            mode: config.mode,
            timeout,
            healthy: Arc::new(AtomicBool::new(true)),
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

    /// Inject bearer token into a tonic request.
    pub fn inject_auth<T>(&self, req: &mut tonic::Request<T>) {
        if let Some(ref token) = self.token {
            if let Ok(val) = format!("Bearer {}", token).parse::<MetadataValue<_>>() {
                req.metadata_mut().insert("authorization", val);
            }
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

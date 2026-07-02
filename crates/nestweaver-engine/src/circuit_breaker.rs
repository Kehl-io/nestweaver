//! Per-host circuit breaker for remote git operations.
//!
//! Opens after `FAILURE_THRESHOLD` failures within `FAILURE_WINDOW` seconds,
//! then skips that host for `COOLDOWN` seconds. After cooldown, allows one
//! probe attempt (half-open state).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Number of failures before the circuit opens for a host.
const FAILURE_THRESHOLD: usize = 5;

/// Window in which failures are counted.
const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// How long the circuit stays open before allowing a probe.
const COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests flow through.
    Closed,
    /// Too many recent failures — requests are rejected.
    Open,
    /// Cooldown elapsed — one probe request is allowed.
    HalfOpen,
}

#[derive(Debug)]
struct HostState {
    /// Timestamps of recent failures (within the window).
    failures: Vec<Instant>,
    /// When the circuit was opened (None if closed). Re-stamped every time the
    /// circuit (re)opens, including on a failed half-open probe, so the
    /// cooldown timer always restarts.
    opened_at: Option<Instant>,
    /// True while a single half-open probe has been admitted and is in flight.
    /// Gates half-open to exactly one probe: concurrent callers beyond the
    /// first short-circuit until the probe resolves.
    probe_in_flight: bool,
}

impl HostState {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
            opened_at: None,
            probe_in_flight: false,
        }
    }

    fn state(&self, now: Instant) -> CircuitState {
        match self.opened_at {
            None => CircuitState::Closed,
            Some(opened) => {
                if now.duration_since(opened) >= COOLDOWN {
                    CircuitState::HalfOpen
                } else {
                    CircuitState::Open
                }
            }
        }
    }

    fn record_failure(&mut self, now: Instant) {
        // Evict failures outside the window.
        self.failures
            .retain(|t| now.duration_since(*t) < FAILURE_WINDOW);
        self.failures.push(now);

        // Any transition INTO open must (re)stamp `opened_at` unconditionally so
        // the cooldown timer restarts. This covers two cases:
        //   * a failed half-open probe (opened_at is already Some) — re-open and
        //     re-arm, otherwise the breaker would stay half-open forever and a
        //     dead host would be hammered on every attempt;
        //   * a closed circuit crossing the failure threshold.
        let reopening = self.opened_at.is_some();
        if reopening || self.failures.len() >= FAILURE_THRESHOLD {
            self.opened_at = Some(now);
            // The probe (if any) has resolved as a failure; release the gate so
            // the next cooldown window can admit a fresh probe.
            self.probe_in_flight = false;
            if reopening {
                tracing::warn!("circuit breaker re-opened after failed probe");
            } else {
                tracing::warn!(
                    failures = self.failures.len(),
                    "circuit breaker opened for host"
                );
            }
        }
    }

    fn record_success(&mut self) {
        // Any success resets the circuit to closed.
        if self.opened_at.is_some() {
            tracing::info!("circuit breaker closed after successful probe");
        }
        self.opened_at = None;
        self.probe_in_flight = false;
        self.failures.clear();
    }
}

/// Thread-safe registry of per-host circuit breakers.
#[derive(Debug)]
pub struct RemoteCircuitBreakers {
    hosts: Mutex<HashMap<String, HostState>>,
}

impl RemoteCircuitBreakers {
    pub fn new() -> Self {
        Self {
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Extract the host portion from a git remote URL.
    /// Handles `https://github.com/...`, `git@github.com:...`, `ssh://git@host/...`
    pub fn extract_host(url: &str) -> String {
        // SSH shorthand: git@github.com:owner/repo.git
        if let Some(rest) = url.strip_prefix("git@")
            && let Some(idx) = rest.find(':')
        {
            return rest[..idx].to_string();
        }
        // URL form: extract host from authority.
        if let Ok(parsed) = url::Url::parse(url)
            && let Some(host) = parsed.host_str()
        {
            return host.to_string();
        }
        // Fallback: use the whole URL as the key.
        url.to_string()
    }

    /// Query the circuit state for a host.
    pub fn state(&self, host: &str) -> CircuitState {
        let map = self.hosts.lock().unwrap();
        match map.get(host) {
            None => CircuitState::Closed,
            Some(hs) => hs.state(Instant::now()),
        }
    }

    /// Returns true if the host is available (closed or half-open).
    pub fn is_available(&self, host: &str) -> bool {
        self.state(host) != CircuitState::Open
    }

    /// Atomically decide whether to admit a request for the given host and, in
    /// half-open, reserve the single probe permit. Returns `true` if the caller
    /// may proceed:
    ///   * CLOSED — always admitted;
    ///   * OPEN — never admitted;
    ///   * HALF_OPEN — admitted only if no probe is already in flight (the first
    ///     caller wins; concurrent callers short-circuit until it resolves).
    fn try_admit(&self, host: &str) -> bool {
        let mut map = self.hosts.lock().unwrap();
        let hs = map.entry(host.to_string()).or_insert_with(HostState::new);
        match hs.state(Instant::now()) {
            CircuitState::Closed => true,
            CircuitState::Open => false,
            CircuitState::HalfOpen => {
                if hs.probe_in_flight {
                    false
                } else {
                    hs.probe_in_flight = true;
                    true
                }
            }
        }
    }

    /// Record a failure for the given host.
    pub fn record_failure(&self, host: &str) {
        let mut map = self.hosts.lock().unwrap();
        map.entry(host.to_string())
            .or_insert_with(HostState::new)
            .record_failure(Instant::now());
    }

    /// Record a success for the given host, resetting its circuit to closed.
    pub fn record_success(&self, host: &str) {
        let mut map = self.hosts.lock().unwrap();
        if let Some(hs) = map.get_mut(host) {
            hs.record_success();
        }
    }

    /// Execute a fallible operation through the circuit breaker.
    /// Returns `Err` with the original error if the operation fails, or a
    /// circuit-open error if the host is currently blocked.
    pub fn call<F, T, E>(&self, host: &str, f: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        // Atomically admit (and, in half-open, reserve the single probe permit).
        if !self.try_admit(host) {
            return Err(CircuitBreakerError::CircuitOpen(host.to_string()));
        }

        match f() {
            Ok(val) => {
                self.record_success(host);
                Ok(val)
            }
            Err(e) => {
                self.record_failure(host);
                Err(CircuitBreakerError::Inner(e))
            }
        }
    }
}

impl Default for RemoteCircuitBreakers {
    fn default() -> Self {
        Self::new()
    }
}

/// Error returned by `RemoteCircuitBreakers::call`.
#[derive(Debug)]
pub enum CircuitBreakerError<E> {
    /// The circuit is open — the operation was not attempted.
    CircuitOpen(String),
    /// The operation was attempted and returned this error.
    Inner(E),
}

impl<E: std::fmt::Display> std::fmt::Display for CircuitBreakerError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CircuitOpen(host) => write!(f, "circuit breaker open for host: {host}"),
            Self::Inner(e) => write!(f, "{e}"),
        }
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for CircuitBreakerError<E> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_by_default() {
        let cb = RemoteCircuitBreakers::new();
        assert_eq!(cb.state("github.com"), CircuitState::Closed);
        assert!(cb.is_available("github.com"));
    }

    #[test]
    fn opens_after_threshold_failures() {
        let cb = RemoteCircuitBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure("github.com");
        }
        assert_eq!(cb.state("github.com"), CircuitState::Open);
        assert!(!cb.is_available("github.com"));
    }

    #[test]
    fn stays_closed_under_threshold() {
        let cb = RemoteCircuitBreakers::new();
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            cb.record_failure("github.com");
        }
        assert_eq!(cb.state("github.com"), CircuitState::Closed);
        assert!(cb.is_available("github.com"));
    }

    #[test]
    fn success_resets_circuit() {
        let cb = RemoteCircuitBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure("github.com");
        }
        assert_eq!(cb.state("github.com"), CircuitState::Open);

        // Simulate the cooldown passing by directly manipulating state.
        {
            let mut map = cb.hosts.lock().unwrap();
            let hs = map.get_mut("github.com").unwrap();
            // Force opened_at to be old enough for half-open.
            hs.opened_at = Some(Instant::now() - COOLDOWN - Duration::from_secs(1));
        }
        assert_eq!(cb.state("github.com"), CircuitState::HalfOpen);
        assert!(cb.is_available("github.com"));

        // A success closes the circuit.
        cb.record_success("github.com");
        assert_eq!(cb.state("github.com"), CircuitState::Closed);
    }

    #[test]
    fn failed_half_open_probe_reopens_and_rearms_timer() {
        let cb = RemoteCircuitBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure("github.com");
        }
        assert_eq!(cb.state("github.com"), CircuitState::Open);

        // Advance past the cooldown so the breaker is HALF_OPEN.
        {
            let mut map = cb.hosts.lock().unwrap();
            let hs = map.get_mut("github.com").unwrap();
            hs.opened_at = Some(Instant::now() - COOLDOWN - Duration::from_secs(1));
        }
        assert_eq!(cb.state("github.com"), CircuitState::HalfOpen);

        // A FAILED half-open probe must re-open the breaker AND restart the
        // cooldown timer, so the host is blocked again for a fresh window.
        cb.record_failure("github.com");
        assert_eq!(
            cb.state("github.com"),
            CircuitState::Open,
            "a failed half-open probe must re-open the breaker"
        );
        assert!(
            !cb.is_available("github.com"),
            "re-opened breaker must block until a fresh cooldown elapses"
        );
    }

    #[test]
    fn half_open_admits_single_probe() {
        let cb = RemoteCircuitBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure("github.com");
        }

        // Advance past the cooldown so the breaker is HALF_OPEN.
        {
            let mut map = cb.hosts.lock().unwrap();
            let hs = map.get_mut("github.com").unwrap();
            hs.opened_at = Some(Instant::now() - COOLDOWN - Duration::from_secs(1));
        }
        assert_eq!(cb.state("github.com"), CircuitState::HalfOpen);

        // Simulate K callers racing to probe at once: exactly one is admitted,
        // the rest short-circuit.
        const K: usize = 8;
        let admitted = (0..K).filter(|_| cb.try_admit("github.com")).count();
        assert_eq!(
            admitted, 1,
            "half-open must admit exactly one probe; the rest short-circuit"
        );
    }

    #[test]
    fn call_rejects_when_open() {
        let cb = RemoteCircuitBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure("github.com");
        }

        let result = cb.call("github.com", || Ok::<_, &str>(42));
        assert!(matches!(result, Err(CircuitBreakerError::CircuitOpen(_))));
    }

    #[test]
    fn call_records_success_and_failure() {
        let cb = RemoteCircuitBreakers::new();

        // Successful call.
        let result = cb.call("gitlab.com", || Ok::<_, &str>(1));
        assert!(result.is_ok());

        // Failing calls.
        for _ in 0..FAILURE_THRESHOLD {
            let _ = cb.call("gitlab.com", || Err::<i32, _>("timeout"));
        }
        assert_eq!(cb.state("gitlab.com"), CircuitState::Open);
    }

    #[test]
    fn extract_host_https() {
        assert_eq!(
            RemoteCircuitBreakers::extract_host("https://github.com/owner/repo.git"),
            "github.com"
        );
    }

    #[test]
    fn extract_host_ssh_shorthand() {
        assert_eq!(
            RemoteCircuitBreakers::extract_host("git@github.com:owner/repo.git"),
            "github.com"
        );
    }

    #[test]
    fn extract_host_ssh_url() {
        assert_eq!(
            RemoteCircuitBreakers::extract_host("ssh://git@gitlab.com/owner/repo"),
            "gitlab.com"
        );
    }

    #[test]
    fn independent_hosts() {
        let cb = RemoteCircuitBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure("github.com");
        }
        assert!(!cb.is_available("github.com"));
        assert!(cb.is_available("gitlab.com")); // different host unaffected
    }
}

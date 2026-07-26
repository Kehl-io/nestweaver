//! MCP-over-HTTP endpoint.
//!
//! Provides a minimal axum HTTP server that accepts `POST /mcp` with
//! JSON-RPC 2.0 bodies.  Handles `initialize`, `tools/list`, and
//! `tools/call` — the latter delegates to the same `tools::dispatch`
//! function used by the stdio server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    routing::post,
};
use dashmap::DashMap;
use nestweaver_store::{GraphStore, TantivyIndex};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::protocol::{PROTOCOL_VERSION, error_code};
use crate::tools;

/// Tools that mutate server state and therefore require admin-level auth.
/// Query tokens may only invoke read-only tools; mutating operations require
/// the admin token when auth is configured.
///
/// This is the SINGLE canonical list of mutating MCP tool names. Both the
/// HTTP/MCP gate (this module) and the daemon's gRPC gate
/// (`nestweaver-daemon`) reference this const, so a new mutating tool cannot be
/// added to one surface's gate while silently leaving the other open.
pub const MUTATING_TOOLS: &[&str] = &[
    "brain_add_source",
    "brain_remove_source",
    "brain_memory_consolidate",
    "set_extension",
    "prune_stale",
];

const SERVER_NAME: &str = "nestweaver-brain";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a session can be idle before the sweeper removes it.
const SESSION_TTL_SECS: u64 = 3600; // 1 hour

/// How long a rate-limit bucket can be idle before the sweeper evicts it.
/// A bucket that has not been touched for this long has fully refilled and is
/// therefore indistinguishable from a fresh one, so evicting it loses no state.
/// This bounds the growth of [`HttpRateLimiter::buckets`], which would
/// otherwise retain one entry for every distinct client key ever observed.
const BUCKET_TTL_SECS: u64 = 600; // 10 minutes

/// How often the background sweeper runs.
const SWEEP_INTERVAL_SECS: u64 = 300; // 5 minutes

/// Default per-tool timeout for MCP HTTP requests.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 30;

/// Hard cap on graph traversal depth parameters.
const MAX_DEPTH: u64 = 15;

/// Hard cap on result count parameters (limit / max_results).
const MAX_RESULTS: u64 = 5_000;

/// Requests allowed per session per minute before rate limiting kicks in.
const RATE_LIMIT_PER_MIN: u64 = 120;

/// Per-client MCP session metadata.
#[derive(Debug)]
pub struct McpSession {
    pub id: String,
    pub created_at: Instant,
    pub last_active: Instant,
    pub request_count: u64,
    pub rate_window_start: Instant,
}

#[derive(Debug)]
struct HttpTokenBucket {
    tokens: f64,
    last_refill: Instant,
}

/// Simple per-client token bucket for stateless MCP-over-HTTP requests.
pub struct HttpRateLimiter {
    buckets: DashMap<String, HttpTokenBucket>,
    capacity: f64,
    refill_per_sec: f64,
    /// Source of the current time. Injectable so tests can freeze the clock
    /// and exercise refill behavior deterministically; production uses
    /// `Instant::now`.
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl std::fmt::Debug for HttpRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRateLimiter")
            .field("buckets", &self.buckets)
            .field("capacity", &self.capacity)
            .field("refill_per_sec", &self.refill_per_sec)
            .finish_non_exhaustive()
    }
}

impl HttpRateLimiter {
    fn new(requests_per_min: u64) -> Self {
        Self::new_with_clock(requests_per_min, Arc::new(Instant::now))
    }

    fn new_with_clock(
        requests_per_min: u64,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            buckets: DashMap::new(),
            capacity: requests_per_min as f64,
            refill_per_sec: requests_per_min as f64 / 60.0,
            clock,
        }
    }

    fn check(&self, client_key: &str) -> bool {
        let now = (self.clock)();
        let mut bucket = self
            .buckets
            .entry(client_key.to_string())
            .or_insert_with(|| HttpTokenBucket {
                tokens: self.capacity,
                last_refill: now,
            });

        let elapsed_secs = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed_secs * self.refill_per_sec).min(self.capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Evict buckets whose last activity (`last_refill`) is older than `ttl`
    /// relative to `now`. Pure over its inputs so it can be unit-tested with a
    /// frozen clock; the background sweeper supplies `(self.clock)()` for `now`.
    /// Returns the number of buckets removed.
    fn sweep_idle_buckets(&self, now: Instant, ttl: Duration) -> usize {
        let before = self.buckets.len();
        self.buckets
            .retain(|_key, bucket| now.duration_since(bucket.last_refill) < ttl);
        before - self.buckets.len()
    }
}

/// Shared state for the MCP HTTP handler.
///
/// Holds references to the graph store and search index so `tools/call`
/// can dispatch through the same path as the stdio server.
pub struct McpHttpState {
    pub lite: bool,
    pub store: Arc<GraphStore>,
    pub tantivy: Option<Arc<TantivyIndex>>,
    pub db_path: PathBuf,
    pub instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
    pub sessions: Arc<DashMap<String, McpSession>>,
    /// Live count of active MCP sessions, republished from `sessions.len()` on
    /// each insert and after each sweep. Shared with the daemon so the admin
    /// dashboard and the `MCP_SESSIONS` Prometheus gauge can read a connection
    /// count without depending on this crate's `DashMap`/`McpSession` types.
    pub mcp_session_gauge: Arc<AtomicU32>,
    /// Whether the daemon is running in server mode. Threaded into the tool
    /// dispatch thread-local so server-only code paths (e.g. `read_symbols`
    /// reading content via `git show` from blobless bare clones, `brain_status`
    /// reporting) behave correctly over HTTP, matching the gRPC handler.
    pub server_mode: bool,
    /// Whether the daemon serves a read-only snapshot replica. When true, the
    /// `tools/call` handler rejects every mutating tool before dispatch
    /// (regardless of auth/admin) — the MCP-HTTP counterpart to the gRPC
    /// `ReadOnlyGuard` chokepoint. A replica opens its store read-only, so a
    /// mutating tool would otherwise fail mid-stream at the storage layer.
    pub read_only: bool,
    /// Optional bearer token for MCP-over-HTTP authentication. When set,
    /// requests must include `Authorization: Bearer <token>` or receive 401.
    pub auth_token: Option<String>,
    /// Optional admin bearer token. When query auth is enabled, this token is
    /// also accepted so MCP and gRPC query auth share the same semantics.
    pub admin_token: Option<String>,
    /// Per-repo authorization policy (Blast Radius R9/R9b). Built from the
    /// instance config's `[authz]` section; absent config yields a *disabled*
    /// source that resolves every identity to [`VisibleRepos::All`], so
    /// blast-radius redaction is a no-op and behavior is unchanged unless an
    /// operator configures repo-scoping.
    pub permission_source: Arc<dyn nestweaver_engine::authz::PermissionSource>,
    pub client_rate_limiter: Arc<HttpRateLimiter>,
    /// Lazily-loaded embedding model for semantic search, shared with the
    /// daemon's gRPC path. Populated by a background task when the `embed`
    /// feature is enabled.
    pub embed_model: Arc<tokio::sync::RwLock<Option<Arc<dyn nestweaver_engine::EmbedQueryFn>>>>,
    /// Optional runtime snapshot provider. Daemon mode installs this so model
    /// reads are atomic with readiness publication; standalone MCP retains the
    /// mutable lock above for backward compatibility.
    pub embed_model_provider: Option<Arc<dyn nestweaver_engine::EmbedModelProvider>>,
    /// Daemon-side federation coordinator, built once from the instance
    /// config's `[[upstream]]` entries. `None` for the common single-node case
    /// (no upstreams configured) — the `/mcp` boundary then stamps the honest
    /// single-node provenance. Present only under the `daemon` feature.
    #[cfg(feature = "daemon")]
    pub federation: Option<Arc<crate::federation::FederationState>>,
}

/// Build the per-repo permission source from the instance config's `[authz]`
/// section. An absent config (or absent `[authz]`) yields a *disabled*
/// [`StaticConfigPermissionSource`] (empty rules) that resolves every identity
/// to [`VisibleRepos::All`] — redaction becomes a no-op, so behavior is
/// unchanged unless repo-scoping is configured.
fn build_permission_source(
    instance_cfg: &Option<Arc<nestweaver_engine::InstanceConfig>>,
) -> Arc<dyn nestweaver_engine::authz::PermissionSource> {
    match instance_cfg.as_ref().and_then(|c| c.authz.as_ref()) {
        Some(authz) => Arc::new(authz.build_permission_source()),
        None => Arc::new(nestweaver_engine::authz::StaticConfigPermissionSource::new(
            std::collections::HashMap::new(),
        )),
    }
}

/// Map a validated bearer to an authorization [`Identity`]. The admin token →
/// [`Identity::Admin`]; the query token value → [`Identity::Token`] keyed on
/// that value; no/unrecognized bearer → [`Identity::Anonymous`]. Comparisons are
/// constant-time, matching the bearer-validation path. When auth is unconfigured
/// (`admin_token`/`query_token` both `None`) every request resolves to
/// `Anonymous`, but the disabled permission source still returns `All`, so this
/// has no effect on visibility.
fn resolve_identity(
    provided_bearer: Option<&str>,
    admin_token: Option<&str>,
    query_token: Option<&str>,
) -> nestweaver_engine::authz::Identity {
    use nestweaver_engine::authz::Identity;
    use subtle::ConstantTimeEq;
    let Some(bearer) = provided_bearer else {
        return Identity::Anonymous;
    };
    if let Some(admin) = admin_token
        && bool::from(bearer.as_bytes().ct_eq(admin.as_bytes()))
    {
        return Identity::Admin;
    }
    if let Some(query) = query_token
        && bool::from(bearer.as_bytes().ct_eq(query.as_bytes()))
    {
        return Identity::Token(bearer.to_string());
    }
    Identity::Anonymous
}

impl McpHttpState {
    /// Create a new state with an empty session registry and no auth.
    pub fn new(
        lite: bool,
        store: Arc<GraphStore>,
        tantivy: Option<Arc<TantivyIndex>>,
        db_path: PathBuf,
        instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
        server_mode: bool,
    ) -> Self {
        let permission_source = build_permission_source(&instance_cfg);
        Self {
            lite,
            store,
            tantivy,
            db_path,
            instance_cfg,
            sessions: Arc::new(DashMap::new()),
            mcp_session_gauge: Arc::new(AtomicU32::new(0)),
            server_mode,
            read_only: false,
            auth_token: None,
            admin_token: None,
            permission_source,
            client_rate_limiter: Arc::new(HttpRateLimiter::new(RATE_LIMIT_PER_MIN)),
            embed_model: Arc::new(tokio::sync::RwLock::new(None)),
            embed_model_provider: None,
            #[cfg(feature = "daemon")]
            federation: None,
        }
    }

    /// Create a new state with bearer token authentication enabled.
    #[allow(clippy::too_many_arguments)]
    pub fn with_auth(
        lite: bool,
        store: Arc<GraphStore>,
        tantivy: Option<Arc<TantivyIndex>>,
        db_path: PathBuf,
        instance_cfg: Option<Arc<nestweaver_engine::InstanceConfig>>,
        server_mode: bool,
        auth_token: String,
        admin_token: Option<String>,
    ) -> Self {
        let permission_source = build_permission_source(&instance_cfg);
        Self {
            lite,
            store,
            tantivy,
            db_path,
            instance_cfg,
            sessions: Arc::new(DashMap::new()),
            mcp_session_gauge: Arc::new(AtomicU32::new(0)),
            server_mode,
            read_only: false,
            auth_token: Some(auth_token),
            admin_token,
            permission_source,
            client_rate_limiter: Arc::new(HttpRateLimiter::new(RATE_LIMIT_PER_MIN)),
            embed_model: Arc::new(tokio::sync::RwLock::new(None)),
            embed_model_provider: None,
            #[cfg(feature = "daemon")]
            federation: None,
        }
    }
}

/// Spawn a background task that removes sessions idle longer than `SESSION_TTL_SECS`.
/// Accepts a shutdown receiver; the loop exits when the shutdown signal fires.
pub fn spawn_session_sweeper(
    sessions: Arc<DashMap<String, McpSession>>,
    gauge: Arc<AtomicU32>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(SWEEP_INTERVAL_SECS);
        let ttl = std::time::Duration::from_secs(SESSION_TTL_SECS);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    let now = Instant::now();
                    sessions.retain(|_id, session| now.duration_since(session.last_active) < ttl);
                    // Republish the live count so an expiry is reflected too.
                    gauge.store(sessions.len() as u32, Ordering::Relaxed);
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });
}

/// Spawn a background task that evicts rate-limit buckets idle longer than
/// [`BUCKET_TTL_SECS`], mirroring [`spawn_session_sweeper`]. Without this the
/// limiter's `buckets` map grows without bound — it gains an entry for every
/// distinct client key ever seen and never releases one.
/// Accepts a shutdown receiver; the loop exits when the shutdown signal fires.
pub fn spawn_bucket_sweeper(
    limiter: Arc<HttpRateLimiter>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let interval = Duration::from_secs(SWEEP_INTERVAL_SECS);
        let ttl = Duration::from_secs(BUCKET_TTL_SECS);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    let now = (limiter.clock)();
                    limiter.sweep_idle_buckets(now, ttl);
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedLimit {
    param: &'static str,
    requested: u64,
    applied: u64,
}

/// Validate depth parameters and cap result-count parameters in tool arguments.
///
/// Depth above [`MAX_DEPTH`] is rejected because traversals become ambiguous
/// and expensive. Result counts above [`MAX_RESULTS`] are capped and disclosed
/// on the MCP response metadata so callers know the returned page was bounded.
fn apply_safeguard_params(arguments: &mut Value) -> Result<Vec<AppliedLimit>, String> {
    let mut limits = Vec::new();
    if let Some(obj) = arguments.as_object_mut() {
        for key in &["depth", "max_depth"] {
            if let Some(val) = obj.get_mut(*key)
                && let Some(n) = val.as_u64()
                && n > MAX_DEPTH
            {
                return Err(format!(
                    "invalid-argument: parameter '{key}' requested depth {n}, maximum allowed depth is {MAX_DEPTH}"
                ));
            }
        }
        for key in &["limit", "max_results"] {
            if let Some(val) = obj.get_mut(*key)
                && let Some(n) = val.as_u64()
                && n > MAX_RESULTS
            {
                tracing::warn!(
                    param = key,
                    requested = n,
                    capped = MAX_RESULTS,
                    "clamped parameter"
                );
                limits.push(AppliedLimit {
                    param: key,
                    requested: n,
                    applied: MAX_RESULTS,
                });
                *val = Value::Number(serde_json::Number::from(MAX_RESULTS));
            }
        }
    }
    Ok(limits)
}

fn add_limit_metadata(mut result: Value, limits: &[AppliedLimit]) -> Value {
    if limits.is_empty() {
        return result;
    }

    if let Some(obj) = result.as_object_mut() {
        let meta = obj.entry("_meta").or_insert_with(|| json!({}));
        if let Some(meta_obj) = meta.as_object_mut() {
            meta_obj.insert(
                "limits".to_string(),
                Value::Array(
                    limits
                        .iter()
                        .map(|limit| {
                            json!({
                                "param": limit.param,
                                "requested": limit.requested,
                                "applied": limit.applied,
                                "reason": "server_cap",
                            })
                        })
                        .collect(),
                ),
            );
        }
    }

    result
}

/// Inject provenance into the MCP result envelope so a raw client hitting this
/// daemon's `/mcp` endpoint gets in-band source metadata — the fan-out boundary should emit the
/// contract (federated-search norm: Elasticsearch `_clusters`, Trino coordinator; MCP `_meta` is
/// server-produced). Keys are domain-namespaced per the MCP `_meta` prefix rule.
///
/// When `upstream_source` is `Some(name)` the daemon federated a two-tier tool
/// against that upstream: scope is `"federated"` and the sources list carries
/// both the daemon and the contributing upstream. When `None` — no upstream
/// configured, an upstream that could not answer, or a non-two-tier tool — the
/// honest single-node stamp is used: a standalone daemon is ONE node and says
/// so, so a raw client learns in-band that the response was not federated.
///
/// `stale_repos` is stamped on EVERY result (empty when no upstream is
/// configured) so staleness reporting is uniform across all tools, not just the
/// federated ones — a caller can always read which repos the local index is
/// behind an upstream on.
fn add_provenance_metadata(
    mut result: Value,
    upstream_source: Option<&str>,
    stale_repos: &[String],
) -> Value {
    if let Some(obj) = result.as_object_mut() {
        let meta = obj.entry("_meta").or_insert_with(|| json!({}));
        if let Some(meta_obj) = meta.as_object_mut() {
            match upstream_source {
                Some(name) => {
                    meta_obj.insert("nestweaver.io/sources".to_string(), json!(["daemon", name]));
                    meta_obj.insert("nestweaver.io/scope".to_string(), json!("federated"));
                }
                None => {
                    meta_obj.insert("nestweaver.io/sources".to_string(), json!(["daemon"]));
                    meta_obj.insert("nestweaver.io/scope".to_string(), json!("single-node"));
                }
            }
            meta_obj.insert("nestweaver.io/stale_repos".to_string(), json!(stale_repos));
        }
    }
    result
}

/// Check per-session rate limit. Returns `true` if the request is allowed.
fn check_session_rate_limit(sessions: &DashMap<String, McpSession>, session_id: &str) -> bool {
    if let Some(mut entry) = sessions.get_mut(session_id) {
        let now = Instant::now();
        let elapsed = now.duration_since(entry.rate_window_start);
        if elapsed >= Duration::from_secs(60) {
            entry.request_count = 1;
            entry.rate_window_start = now;
            entry.last_active = now;
            return true;
        }
        if entry.request_count >= RATE_LIMIT_PER_MIN {
            return false;
        }
        entry.request_count += 1;
        entry.last_active = now;
        true
    } else {
        // Unknown session — allow (session tracking will create one on initialize).
        true
    }
}

/// Derive the rate-limit bucket key for a request.
///
/// `trust_session_id` MUST be false for `initialize` and any other pre-session
/// request: there the `mcp-session-id` header is attacker-controlled and not
/// yet bound to a real session. Keying on it in that case would let a caller
/// rotate the header on every `initialize` and mint a brand-new full bucket
/// each time, never getting throttled. Pre-session requests instead fall back
/// to a stable client identity — the peer IP when available, otherwise the
/// bearer identity, otherwise a single shared anonymous bucket — so the
/// identity gets throttled regardless of how the header is rotated.
fn http_client_rate_limit_key(
    provided_bearer: Option<&str>,
    session_id: Option<&str>,
    peer_ip: Option<std::net::IpAddr>,
    trust_session_id: bool,
    admin_bypass_rate_limit: bool,
) -> String {
    if admin_bypass_rate_limit {
        return "bearer:admin".to_string();
    }
    if trust_session_id && let Some(sid) = session_id {
        return format!("session:{sid}");
    }
    if let Some(ip) = peer_ip {
        return format!("ip:{ip}");
    }
    if provided_bearer.is_some() {
        return "bearer:query".to_string();
    }
    "anonymous-stateless".to_string()
}

/// Build an axum [`Router`] that serves `POST /mcp`.
pub fn router(state: Arc<McpHttpState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .with_state(state)
}

/// JSON-RPC request as received over HTTP (same shape as the stdio wire
/// format but parsed from the request body instead of a line).
#[derive(serde::Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[allow(dead_code)]
    #[serde(default)]
    params: Option<Value>,
}

/// Optional peer-address extractor.
///
/// `ConnectInfo<SocketAddr>` *rejects* the request when connection info is
/// absent (the TLS listener serves connections directly via hyper without it,
/// and `oneshot` in tests supplies none), and axum 0.8 does not implement
/// `OptionalFromRequestParts` for it. This wrapper reads the `ConnectInfo`
/// extension directly and yields `None` instead of failing, so the handler can
/// fall back to a different rate-limit identity.
struct OptionalPeerAddr(Option<SocketAddr>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for OptionalPeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        ))
    }
}

/// Build a JSON-RPC 2.0 error response tuple in the exact shape `handle_mcp`
/// returns, so its several early-rejection paths (session / rate-limit / parse /
/// auth / method-not-found) can't drift in envelope structure.
fn jsonrpc_error(
    status: axum::http::StatusCode,
    id: Value,
    code: i32,
    message: &str,
) -> (axum::http::StatusCode, HeaderMap, Json<Value>) {
    (
        status,
        HeaderMap::new(),
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        })),
    )
}

async fn handle_mcp(
    State(state): State<Arc<McpHttpState>>,
    // Peer address, populated by `into_make_service_with_connect_info` on the
    // plaintext listener; `None` over TLS (served directly via hyper) and in
    // unit tests (`oneshot`). When absent we fall back to the bearer identity
    // for keying.
    OptionalPeerAddr(peer_addr): OptionalPeerAddr,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> (axum::http::StatusCode, HeaderMap, Json<Value>) {
    let peer_ip = peer_addr.map(|addr| addr.ip());
    let provided_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let mut admin_bypass_rate_limit = false;

    // Validate bearer token when auth is configured.
    if let Some(ref expected) = state.auth_token {
        match provided_bearer {
            Some(t)
                if {
                    use subtle::ConstantTimeEq;
                    let query_match = bool::from(t.as_bytes().ct_eq(expected.as_bytes()));
                    let admin_match = state
                        .admin_token
                        .as_ref()
                        .map(|admin| bool::from(t.as_bytes().ct_eq(admin.as_bytes())))
                        .unwrap_or(false);
                    admin_bypass_rate_limit = admin_match;
                    query_match || admin_match
                } => {}
            _ => {
                return jsonrpc_error(
                    axum::http::StatusCode::UNAUTHORIZED,
                    Value::Null,
                    error_code::INVALID_REQUEST,
                    "unauthorized: valid Bearer token required",
                );
            }
        }
    }

    let id = req.id.clone().unwrap_or(Value::Null);

    // Track the session: look up an existing one or note that we need a new one.
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    // Reject unknown session IDs (except for `initialize` which creates one).
    // If a client sends a session ID that isn't in our DashMap, it likely
    // expired or belongs to a previous server instance — ask it to re-init.
    if let Some(ref sid) = session_id
        && req.method != "initialize"
        && !state.sessions.contains_key(sid)
    {
        return jsonrpc_error(
            axum::http::StatusCode::OK,
            id.clone(),
            error_code::INVALID_REQUEST,
            "unknown or expired session ID — please re-initialize",
        );
    }

    // The client-supplied session id is only a trustworthy rate-limit key once
    // it names an established session. `initialize` always mints a fresh
    // session, so its inbound id is never trusted — this is what closes the
    // rotate-the-header bypass (see `http_client_rate_limit_key`).
    let trust_session_id = req.method != "initialize"
        && session_id
            .as_deref()
            .map(|sid| state.sessions.contains_key(sid))
            .unwrap_or(false);

    // Per-session rate limiting (server mode only).
    if state.server_mode {
        let client_key = http_client_rate_limit_key(
            provided_bearer,
            session_id.as_deref(),
            peer_ip,
            trust_session_id,
            admin_bypass_rate_limit,
        );

        if !admin_bypass_rate_limit && !state.client_rate_limiter.check(&client_key) {
            return jsonrpc_error(
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                Value::Null,
                error_code::INVALID_REQUEST,
                "rate limit exceeded: too many requests per minute",
            );
        }

        // Admin tokens are never throttled by the per-session limiter either,
        // matching the client-IP limiter above and the gRPC interceptor
        // (auth.rs checks `if !is_admin` before rate limiting). The check
        // still runs so `last_active`/`request_count` bookkeeping stays fresh
        // for the session sweeper — only the rejection is skipped. Without
        // the bypass an admin client got 429 at request RATE_LIMIT_PER_MIN+1.
        if let Some(ref sid) = session_id
            && !check_session_rate_limit(&state.sessions, sid)
            && !admin_bypass_rate_limit
        {
            return jsonrpc_error(
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                Value::Null,
                error_code::INVALID_REQUEST,
                "rate limit exceeded: too many requests per session per minute",
            );
        }
    } else {
        // Non-server mode: just update last_active / request_count.
        if let Some(ref sid) = session_id
            && let Some(mut entry) = state.sessions.get_mut(sid)
        {
            entry.last_active = Instant::now();
            entry.request_count += 1;
        }
    }

    let response = match req.method.as_str() {
        "initialize" => {
            // Always create a fresh session on initialize.
            let new_id = Uuid::new_v4().to_string();
            let now = Instant::now();
            state.sessions.insert(
                new_id.clone(),
                McpSession {
                    id: new_id.clone(),
                    created_at: now,
                    last_active: now,
                    request_count: 1,
                    rate_window_start: now,
                },
            );
            state
                .mcp_session_gauge
                .store(state.sessions.len() as u32, Ordering::Relaxed);

            let mut resp_headers = HeaderMap::new();
            resp_headers.insert("mcp-session-id", new_id.parse().unwrap());

            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "version": SERVER_VERSION,
                    },
                    "instructions": crate::SERVER_INSTRUCTIONS,
                }
            });

            return (axum::http::StatusCode::OK, resp_headers, Json(body));
        }

        "notifications/initialized" | "initialized" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": null,
        }),

        "tools/list" => {
            let tool_list = tools::tool_list(state.lite);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": tool_list,
            })
        }

        "tools/call" => {
            let params = req.params.clone().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(Value::Object(serde_json::Map::new()));

            let Some(name) = name else {
                return jsonrpc_error(
                    axum::http::StatusCode::OK,
                    id.clone(),
                    error_code::INVALID_PARAMS,
                    "tools/call: 'name' is required",
                );
            };

            if let Err(error) = tools::validate_tool_arguments(&name, &arguments) {
                return (
                    axum::http::StatusCode::OK,
                    HeaderMap::new(),
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": tools::wrap_tool_error(&error.to_string()),
                    })),
                );
            }

            // Read-only snapshot replica: reject EVERY mutating tool before
            // dispatch, regardless of auth/admin. A replica opens its store
            // read-only, so a mutating tool would otherwise dispatch and fail
            // mid-stream at the storage layer (finding #10) on the very surface
            // a replica exists to serve. An admin token must NOT bypass this —
            // there is no writable store to mutate. Mirrors the gRPC
            // `ReadOnlyGuard` chokepoint.
            if state.read_only && MUTATING_TOOLS.contains(&name.as_str()) {
                return jsonrpc_error(
                    axum::http::StatusCode::OK,
                    id.clone(),
                    error_code::INVALID_REQUEST,
                    &format!(
                        "this daemon serves a read-only snapshot replica; \
                         tool '{}' is not available",
                        name
                    ),
                );
            }

            // C3: Mutating tools require admin token when auth is configured.
            if MUTATING_TOOLS.contains(&name.as_str())
                && state.auth_token.is_some()
                && !admin_bypass_rate_limit
            {
                return jsonrpc_error(
                    axum::http::StatusCode::FORBIDDEN,
                    id.clone(),
                    error_code::INVALID_REQUEST,
                    &format!("tool '{}' is mutating and requires the admin token", name),
                );
            }

            let store = state.store.clone();
            let tantivy = state.tantivy.clone();
            let db_path = state.db_path.clone();
            let instance_cfg = state.instance_cfg.clone();
            let lite = state.lite;
            let server_mode = state.server_mode;
            #[cfg(feature = "daemon")]
            let federation = state.federation.clone();

            // Resolve the caller's per-repo visibility (R9/R9b). A disabled policy
            // (no `[authz]`, the single-trust-domain default) short-circuits to
            // `All` with NO per-request repo listing — the hot path pays nothing.
            // Only an enabled policy maps the (already-validated) bearer to an
            // identity — admin token → Admin, query token → Token(<value>),
            // anything else → Anonymous — and lists repos to resolve visibility.
            // nw-043: a store ERROR while listing fails the request loudly
            // (JSON-RPC error, non-200, after one retry) instead of silently
            // redacting everything — a silent full redaction reads as a valid
            // empty result. Mirrors the daemon boundary; `Some(&All)` makes
            // blast_radius redaction a no-op.
            let visible = if state.permission_source.is_enabled() {
                use nestweaver_engine::authz::{AuthzRepoListing, classify_repo_listing};
                let identity = resolve_identity(
                    provided_bearer,
                    state.admin_token.as_deref(),
                    state.auth_token.as_deref(),
                );
                match classify_repo_listing(store.list_repos(None), || store.list_repos(None)) {
                    AuthzRepoListing::Resolve(repos) => {
                        state.permission_source.visible_repos(&identity, &repos)
                    }
                    AuthzRepoListing::FailLoud(msg) => {
                        return jsonrpc_error(
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            id.clone(),
                            error_code::INTERNAL_ERROR,
                            &msg,
                        );
                    }
                }
            } else {
                nestweaver_engine::authz::VisibleRepos::All
            };

            // Read the embed model Arc outside the blocking thread (matches the
            // gRPC handler pattern in server.rs), then drop the RwLock guard.
            let embed_arc = if let Some(provider) = &state.embed_model_provider {
                provider.current_model()
            } else {
                let guard = state.embed_model.read().await;
                guard.clone()
            };

            // Validate depth and cap result-count parameters to server caps so
            // MCP clients cannot request unbounded traversals or result sets.
            let mut arguments = arguments;
            let mut applied_limits = Vec::new();
            if server_mode {
                match apply_safeguard_params(&mut arguments) {
                    Ok(limits) => applied_limits = limits,
                    Err(message) => {
                        return jsonrpc_error(
                            axum::http::StatusCode::OK,
                            id.clone(),
                            error_code::INVALID_PARAMS,
                            &message,
                        );
                    }
                }
            }

            // The federation coordinator (below, after local dispatch) queries
            // the upstream with the SAME (post-safeguard) arguments the local
            // tier saw. Capture them now, before `arguments` is moved into the
            // blocking dispatch closure — but only when an upstream is actually
            // configured, so the common single-node daemon pays no clone. Keep
            // the caller's resolved visibility with those arguments: the local
            // dispatch consumes its copy on the blocking thread, while the
            // post-local federation gate must make its decision from the same
            // authorization verdict before any upstream I/O.
            #[cfg(feature = "daemon")]
            let fed_capture = federation
                .as_ref()
                .map(|_| (arguments.clone(), visible.clone()));

            // Run tool dispatch on a blocking thread — graph queries are
            // CPU-bound and must not starve the tokio runtime.
            // Wrap in a timeout to match the gRPC safeguard behaviour. The
            // cancel flag is shared with the blocking dispatch so a timeout
            // doesn't just abandon the await — cancellable walks (impact,
            // dead_code, flow_trace, vector search) observe the flag and stop.
            let timeout = Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS);
            let tool_name = name.clone();
            let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let dispatch_cancel = std::sync::Arc::clone(&cancel_flag);
            let result = tokio::time::timeout(
                timeout,
                tokio::task::spawn_blocking(move || {
                    tools::set_current_db_path(db_path);
                    tools::set_lite_mode(lite);
                    tools::set_current_instance_config(instance_cfg);
                    // Match the gRPC handler: server-only code paths (read_symbols
                    // via git, brain_status) key off this thread-local. Without it,
                    // HTTP requests in server mode read from an empty filesystem and
                    // return empty bodies.
                    tools::set_server_mode(server_mode);

                    tools::dispatch_cancellable(
                        &store,
                        tantivy.as_deref(),
                        &tool_name,
                        arguments,
                        embed_arc.as_deref(),
                        Some(&dispatch_cancel),
                        Some(&visible),
                    )
                }),
            )
            .await;

            match result {
                Ok(Ok(Ok(value))) => {
                    // Federation coordinator step: run AFTER the local dispatch
                    // and BEFORE envelope assembly. For a `TwoTier`-routed tool
                    // with a healthy upstream configured, this fans the local
                    // result out into a `{ local_impact, org_wide_impact }`
                    // envelope; otherwise the local value passes through
                    // untouched. `stale_repos` is the cached staleness verdict,
                    // stamped on every result (empty without an upstream).
                    #[cfg(feature = "daemon")]
                    let (value, upstream_source, stale_repos) = match (&federation, fed_capture) {
                        (Some(fed), Some((fed_args, fed_visible))) => {
                            let expose_staleness =
                                matches!(fed_visible, nestweaver_engine::authz::VisibleRepos::All);
                            let (v, src) = crate::federation::federate_two_tier(
                                fed,
                                &name,
                                &fed_args,
                                value,
                                &fed_visible,
                            )
                            .await;
                            let stale_repos = if expose_staleness {
                                fed.stale_repos()
                            } else {
                                // The cached verdict contains org-wide repo
                                // URLs and is not keyed by caller scope.
                                Vec::new()
                            };
                            (v, src, stale_repos)
                        }
                        _ => (value, None, Vec::new()),
                    };
                    #[cfg(not(feature = "daemon"))]
                    let (value, upstream_source, stale_repos): (
                        Value,
                        Option<String>,
                        Vec<String>,
                    ) = (value, None, Vec::new());

                    let result = add_provenance_metadata(
                        add_limit_metadata(tools::wrap_tool_result(value), &applied_limits),
                        upstream_source.as_deref(),
                        &stale_repos,
                    );
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    })
                }
                Ok(Ok(Err(e))) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_error(&e.to_string()),
                }),
                Ok(Err(e)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tools::wrap_tool_error(&format!("dispatch panicked: {e}")),
                }),
                Err(_elapsed) => {
                    // Trip the shared flag so the still-running blocking walk
                    // stops cooperatively instead of burning a blocking thread
                    // to completion after the response has already been sent.
                    // `Relaxed` on both sides: this is a lone standalone
                    // cancellation bool that publishes no other state, so it
                    // needs no release/acquire pairing — matching the BFS
                    // readers (store/traverse.rs, engine/dead_code.rs,
                    // mcp/tools.rs) that load it `Relaxed`.
                    cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(tool = %name, timeout_secs = DEFAULT_TOOL_TIMEOUT_SECS, "MCP tool dispatch timed out");
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": tools::wrap_tool_error(&format!(
                            "{name} query exceeded {DEFAULT_TOOL_TIMEOUT_SECS}s timeout"
                        )),
                    })
                }
            }
        }

        "ping" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {},
        }),

        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": error_code::METHOD_NOT_FOUND,
                "message": format!("method not implemented: {other}"),
            }
        }),
    };

    (axum::http::StatusCode::OK, HeaderMap::new(), Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn provenance_metadata_injects_namespaced_single_node_scope() {
        // A raw /mcp client must get in-band provenance so the schema (which mentions _meta)
        // is honest: a standalone daemon reports itself as the sole source + single-node scope.
        let out = add_provenance_metadata(json!({ "content": [], "isError": false }), None, &[]);
        let meta = &out["_meta"];
        assert_eq!(meta["nestweaver.io/scope"], json!("single-node"));
        assert_eq!(meta["nestweaver.io/sources"], json!(["daemon"]));
        // Staleness is stamped uniformly — empty when no upstream is configured.
        assert_eq!(meta["nestweaver.io/stale_repos"], json!([]));

        // Merges with pre-existing _meta (e.g. limits) rather than clobbering it.
        let with_limits = json!({ "content": [], "_meta": { "limits": [{"param": "depth"}] } });
        let out = add_provenance_metadata(with_limits, None, &[]);
        assert_eq!(out["_meta"]["nestweaver.io/scope"], json!("single-node"));
        assert!(out["_meta"]["limits"].is_array());

        // A federated result names the contributing upstream and flips scope.
        let fed = add_provenance_metadata(
            json!({ "content": [] }),
            Some("acme"),
            &["https://github.com/acme/api.git".to_string()],
        );
        assert_eq!(fed["_meta"]["nestweaver.io/scope"], json!("federated"));
        assert_eq!(
            fed["_meta"]["nestweaver.io/sources"],
            json!(["daemon", "acme"])
        );
        assert_eq!(
            fed["_meta"]["nestweaver.io/stale_repos"],
            json!(["https://github.com/acme/api.git"])
        );
    }

    /// Regression guard for the single shared mutating-tool list. Pins the
    /// known set (adding/removing a mutating tool is a deliberate edit) and
    /// asserts entries are unique — a duplicate would mask a typo. Both the
    /// HTTP gate here and the daemon's gRPC gate read this exact const.
    #[test]
    fn mutating_tools_list_is_the_known_set() {
        assert_eq!(
            MUTATING_TOOLS,
            &[
                "brain_add_source",
                "brain_remove_source",
                "brain_memory_consolidate",
                "set_extension",
                "prune_stale",
            ]
        );
        let unique: std::collections::HashSet<_> = MUTATING_TOOLS.iter().collect();
        assert_eq!(
            unique.len(),
            MUTATING_TOOLS.len(),
            "MUTATING_TOOLS must not contain duplicates"
        );
    }

    fn test_app() -> Router {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let state = Arc::new(McpHttpState::new(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            false,
        ));
        router(state)
    }

    fn test_server_app() -> Router {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let state = Arc::new(McpHttpState::new(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            true,
        ));
        router(state)
    }

    fn valid_mutating_arguments(tool: &str) -> Value {
        match tool {
            "brain_add_source" => json!({ "path": "/tmp/not-dispatched" }),
            "brain_remove_source" => json!({ "target": "repo:not-dispatched" }),
            "brain_memory_consolidate" => json!({}),
            "set_extension" => json!({
                "uid": "sym:not-dispatched",
                "key": "reviewed",
                "value": true,
            }),
            "prune_stale" => json!({}),
            other => panic!("missing schema-valid mutating fixture for {other}"),
        }
    }

    fn test_auth_app() -> Router {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let state = Arc::new(McpHttpState::with_auth(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            false,
            "query-token".to_string(),
            Some("admin-token".to_string()),
        ));
        router(state)
    }

    fn test_server_auth_app_with_limiter(requests_per_min: u64) -> Router {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let mut state = McpHttpState::with_auth(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            true,
            "shared-query-token".to_string(),
            Some("admin-token".to_string()),
        );
        let frozen = Instant::now();
        state.client_rate_limiter = Arc::new(HttpRateLimiter::new_with_clock(
            requests_per_min,
            Arc::new(move || frozen),
        ));
        let state = Arc::new(state);
        for sid in ["session-a", "session-b"] {
            state.sessions.insert(
                sid.to_string(),
                McpSession {
                    id: sid.to_string(),
                    created_at: frozen,
                    last_active: frozen,
                    request_count: 0,
                    rate_window_start: frozen,
                },
            );
        }
        router(state)
    }

    #[test]
    fn safeguard_rejects_over_depth_instead_of_clamping() {
        let mut args = json!({
            "depth": MAX_DEPTH + 1,
            "limit": 10,
        });

        let err = apply_safeguard_params(&mut args).unwrap_err();

        assert!(err.contains("invalid-argument"));
        assert!(err.contains("maximum allowed depth"));
        assert_eq!(args["depth"], MAX_DEPTH + 1);
    }

    #[test]
    fn safeguard_caps_result_limit_and_records_metadata() {
        let mut args = json!({
            "limit": MAX_RESULTS + 25,
        });

        let limits = apply_safeguard_params(&mut args).unwrap();
        let result = add_limit_metadata(tools::wrap_tool_result(json!({"ok": true})), &limits);

        assert_eq!(args["limit"], MAX_RESULTS);
        assert_eq!(result["_meta"]["limits"][0]["param"], "limit");
        assert_eq!(result["_meta"]["limits"][0]["requested"], MAX_RESULTS + 25);
        assert_eq!(result["_meta"]["limits"][0]["applied"], MAX_RESULTS);
        assert_eq!(result["structuredContent"]["ok"], true);
    }

    #[test]
    fn rate_limit_rejects_at_limit_within_window() {
        let sessions = DashMap::new();
        let now = Instant::now();
        sessions.insert(
            "sid".to_string(),
            McpSession {
                id: "sid".to_string(),
                created_at: now,
                last_active: now,
                request_count: RATE_LIMIT_PER_MIN,
                rate_window_start: now,
            },
        );

        assert!(!check_session_rate_limit(&sessions, "sid"));
    }

    #[test]
    fn rate_limit_resets_by_window_start_not_last_activity() {
        let sessions = DashMap::new();
        let now = Instant::now();
        sessions.insert(
            "sid".to_string(),
            McpSession {
                id: "sid".to_string(),
                created_at: now - Duration::from_secs(120),
                last_active: now,
                request_count: RATE_LIMIT_PER_MIN,
                rate_window_start: now - Duration::from_secs(61),
            },
        );

        assert!(check_session_rate_limit(&sessions, "sid"));
        let session = sessions.get("sid").unwrap();
        assert_eq!(session.request_count, 1);
    }

    #[test]
    fn http_rate_limiter_rejects_after_capacity_with_frozen_clock() {
        // Freeze the clock so the bucket never refills, making the rate-limit
        // boundary exact and independent of wall-clock timing.
        let frozen = Instant::now();
        let limiter = HttpRateLimiter::new_with_clock(RATE_LIMIT_PER_MIN, Arc::new(move || frozen));

        // The first RATE_LIMIT_PER_MIN requests for one client consume the
        // full bucket and succeed.
        for i in 0..RATE_LIMIT_PER_MIN {
            assert!(
                limiter.check("client-a"),
                "request {i} should be allowed within capacity"
            );
        }

        // The next request has no tokens left and is rejected (the 429 path).
        assert!(
            !limiter.check("client-a"),
            "request beyond capacity must be rate limited with a frozen clock"
        );

        // Rate limiting is per-client: a different key starts with a full bucket.
        assert!(
            limiter.check("client-b"),
            "a distinct client must not be limited by another client's usage"
        );
    }

    #[test]
    fn sweep_idle_buckets_evicts_entries_idle_past_ttl() {
        let frozen = Instant::now();
        let limiter = HttpRateLimiter::new_with_clock(RATE_LIMIT_PER_MIN, Arc::new(move || frozen));

        // Touch three distinct clients so each gets a bucket (last_refill = frozen).
        for key in ["a", "b", "c"] {
            assert!(limiter.check(key));
        }
        assert_eq!(limiter.buckets.len(), 3);

        // Sweeping at the frozen instant evicts nothing — every bucket is fresh.
        let ttl = Duration::from_secs(BUCKET_TTL_SECS);
        assert_eq!(limiter.sweep_idle_buckets(frozen, ttl), 0);
        assert_eq!(limiter.buckets.len(), 3);

        // Advancing `now` past the TTL makes every bucket idle and evictable,
        // bounding the map's growth instead of leaking an entry per client key.
        let later = frozen + ttl + Duration::from_secs(1);
        assert_eq!(limiter.sweep_idle_buckets(later, ttl), 3);
        assert_eq!(limiter.buckets.len(), 0);
    }

    #[test]
    fn rate_limit_key_ignores_client_session_id_for_pre_session_requests() {
        // Pre-session (initialize): a rotating, untrusted session id must not
        // change the key — it resolves to the stable bearer identity instead,
        // so the caller cannot mint a fresh full bucket per request.
        let k1 = http_client_rate_limit_key(Some("tok"), Some("rotating-1"), None, false, false);
        let k2 = http_client_rate_limit_key(Some("tok"), Some("rotating-2"), None, false, false);
        assert_eq!(k1, k2);
        assert_eq!(k1, "bearer:query");

        // With a peer IP available the pre-session key is the IP — still stable
        // across rotated session ids.
        let ip: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let k3 = http_client_rate_limit_key(None, Some("rotating-1"), Some(ip), false, false);
        let k4 = http_client_rate_limit_key(None, Some("rotating-2"), Some(ip), false, false);
        assert_eq!(k3, k4);
        assert_eq!(k3, "ip:203.0.113.7");

        // An established (trusted) session still keys per session, as before.
        let k5 = http_client_rate_limit_key(Some("tok"), Some("sess-x"), None, true, false);
        assert_eq!(k5, "session:sess-x");

        // Admin always bypasses to its own bucket regardless of the session id.
        let k6 = http_client_rate_limit_key(Some("tok"), Some("sess-x"), None, true, true);
        assert_eq!(k6, "bearer:admin");
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn tools_list_returns_tools() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 2);
        let tools = json["result"]["tools"].as_array().expect("tools array");
        assert!(tools.len() >= 30, "expected 30+ tools, got {}", tools.len());
    }

    #[tokio::test]
    async fn admin_token_is_accepted_for_mcp_http_query_auth() {
        for token in ["query-token", "admin-token"] {
            let app = test_auth_app();
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": token,
                "method": "tools/list",
            });
            let req = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(json["result"]["tools"].is_array(), "{json}");
        }
    }

    #[tokio::test]
    async fn bearer_auth_rate_limit_is_keyed_by_session_when_present() {
        let app = test_server_auth_app_with_limiter(1);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/list",
        });

        for sid in ["session-a", "session-b"] {
            let req = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("authorization", "Bearer shared-query-token")
                .header("mcp-session-id", sid)
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "session {sid} should not inherit another session's bearer bucket"
            );
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(json["result"]["tools"].is_array(), "{json}");
        }
    }

    #[tokio::test]
    async fn rotating_session_id_on_initialize_does_not_mint_fresh_buckets() {
        // Capacity of 1: the same identity gets exactly one request before
        // being throttled. Rotating the mcp-session-id header across repeated
        // `initialize` calls must NOT escape the throttle — the old code keyed
        // on the client-supplied session id and minted a fresh full bucket each
        // time, so it never throttled.
        let app = test_server_auth_app_with_limiter(1);

        let mut statuses = Vec::new();
        for sid in ["rotating-1", "rotating-2"] {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
            });
            let req = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("authorization", "Bearer shared-query-token")
                .header("mcp-session-id", sid)
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let resp = app.clone().oneshot(req).await.unwrap();
            statuses.push(resp.status());
        }

        assert_eq!(statuses[0], StatusCode::OK, "first initialize is allowed");
        assert_eq!(
            statuses[1],
            StatusCode::TOO_MANY_REQUESTS,
            "rotating the session id must not mint a fresh bucket — the identity stays throttled"
        );
    }

    #[tokio::test]
    async fn admin_token_bypasses_per_session_rate_limit() {
        // The admin bypass must cover the per-session limiter too — previously
        // only the client-IP limiter skipped admins, so an admin token still
        // got 429 at request RATE_LIMIT_PER_MIN+1 on one session. The gRPC
        // interceptor skips rate limiting for admins entirely; HTTP now
        // matches. Client-IP bucket capacity is 1000 so only the session
        // limiter can produce a 429 here.
        let app = test_server_auth_app_with_limiter(1000);
        let call = |token: &str, sid: &str, i: u64| {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": i,
                "method": "tools/list",
            });
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .header("mcp-session-id", sid)
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        // Admin: RATE_LIMIT_PER_MIN+1 requests on one session, never a 429.
        for i in 0..=RATE_LIMIT_PER_MIN {
            let resp = app
                .clone()
                .oneshot(call("admin-token", "session-a", i))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "admin request {} must bypass the session limiter",
                i + 1
            );
        }

        // Control: the query token IS throttled by the session limiter once it
        // crosses RATE_LIMIT_PER_MIN requests on its session.
        let mut last = StatusCode::OK;
        for i in 0..=RATE_LIMIT_PER_MIN {
            let resp = app
                .clone()
                .oneshot(call("shared-query-token", "session-b", i))
                .await
                .unwrap();
            last = resp.status();
            if last == StatusCode::TOO_MANY_REQUESTS {
                break;
            }
        }
        assert_eq!(
            last,
            StatusCode::TOO_MANY_REQUESTS,
            "query token must still hit the session limiter past RATE_LIMIT_PER_MIN"
        );
    }

    #[tokio::test]
    async fn malformed_brain_search_returns_in_band_schema_error_before_safeguards() {
        let app = test_server_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 40,
            "method": "tools/call",
            "params": {
                "name": "brain_search",
                "arguments": { "query": 17, "depth": MAX_DEPTH + 1 },
            },
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();

        assert!(json.get("error").is_none(), "{json}");
        assert_eq!(json["result"]["isError"], serde_json::json!(true));
        let result = json["result"].to_string();
        assert!(result.contains("invalid arguments for tool"), "{result}");
        assert!(result.contains("/query"), "{result}");
    }

    #[tokio::test]
    async fn malformed_mutating_tools_return_in_band_schema_errors_before_authorization() {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let mut state = McpHttpState::with_auth(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            true,
            "query-token".to_string(),
            Some("admin-token".to_string()),
        );
        state.read_only = true;
        let gate_cases = [
            (router(Arc::new(state)), "admin-token", "read-only"),
            (test_auth_app(), "query-token", "admin"),
        ];

        let malformed_calls = [
            ("brain_add_source", json!({ "path": 17 }), "/path"),
            ("brain_remove_source", json!({ "target": 17 }), "/target"),
            (
                "brain_memory_consolidate",
                json!({ "apply": "yes" }),
                "/apply",
            ),
            (
                "set_extension",
                json!({ "uid": 17, "key": "reviewed", "value": true }),
                "/uid",
            ),
            ("prune_stale", json!([]), "/"),
        ];

        for (tool, arguments, instance_path) in malformed_calls {
            for (app, token, gate) in &gate_cases {
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": tool,
                    "method": "tools/call",
                    "params": { "name": tool, "arguments": arguments },
                });
                let req = Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap();

                let resp = app.clone().oneshot(req).await.unwrap();
                assert_eq!(resp.status(), StatusCode::OK, "{tool} before {gate} gate");
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let json: Value = serde_json::from_slice(&bytes).unwrap();

                assert!(
                    json.get("error").is_none(),
                    "{tool} before {gate} gate: {json}"
                );
                assert_eq!(
                    json["result"]["isError"],
                    serde_json::json!(true),
                    "{tool} before {gate} gate: {json}"
                );
                let result = json["result"].to_string();
                assert!(
                    result.contains("invalid arguments for tool"),
                    "{tool} before {gate} gate: {result}"
                );
                assert!(
                    result.contains(instance_path),
                    "{tool} before {gate} gate must identify {instance_path}: {result}"
                );
            }
        }
    }

    /// A read-only snapshot replica must reject every mutating MCP tool over
    /// `tools/call` BEFORE dispatch — even when the caller presents the admin
    /// token — so the mutation never reaches the read-only store. This is the
    /// MCP-HTTP counterpart to the gRPC `ReadOnlyGuard`.
    #[tokio::test]
    async fn read_only_replica_rejects_mutating_mcp_tool_before_dispatch() {
        let store = Arc::new(GraphStore::in_memory().unwrap());
        let mut state = McpHttpState::with_auth(
            false,
            store,
            None,
            PathBuf::from("/tmp/test.lbug"),
            None,
            true, // server_mode
            "query-token".to_string(),
            Some("admin-token".to_string()),
        );
        state.read_only = true;
        let app = router(Arc::new(state));

        // Every mutating tool must be rejected, even with the admin token.
        for tool in MUTATING_TOOLS {
            let arguments = valid_mutating_arguments(tool);
            tools::validate_tool_arguments(tool, &arguments)
                .unwrap_or_else(|error| panic!("invalid test fixture for {tool}: {error}"));
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": tool,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments },
            });
            let req = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("authorization", "Bearer admin-token")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                json["error"]["code"],
                error_code::INVALID_REQUEST,
                "mutating tool {tool} on a read-only replica must return a JSON-RPC error: {json}"
            );
            let msg = json["error"]["message"].as_str().unwrap_or_default();
            assert!(
                msg.contains("read-only snapshot replica"),
                "mutating tool {tool} must be rejected as read-only (not dispatched): {json}"
            );
            // A dispatch would have produced a `result`, not an `error`.
            assert!(
                json.get("result").is_none(),
                "mutating tool {tool} must not reach dispatch on a read-only replica: {json}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "no/such/method",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 3);
        assert_eq!(json["error"]["code"], error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_session_id_rejected() {
        let app = test_app();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/list",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("mcp-session-id", "nonexistent-session-id")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["id"], 4);
        assert_eq!(json["error"]["code"], error_code::INVALID_REQUEST);
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("re-initialize"),
        );
    }
}

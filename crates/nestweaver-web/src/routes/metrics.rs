use axum::http::header;
use axum::response::IntoResponse;
use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use std::sync::LazyLock;

/// Global metrics registry. Using a dedicated registry avoids conflicts with
/// any default-registry usage elsewhere in the process.
pub static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

// ── Repo status ──────────────────────────────────────────────────────────

pub static REPOS_TOTAL: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let g = IntGaugeVec::new(
        Opts::new("nestweaver_repos_total", "Number of repos by status"),
        &["status"],
    )
    .unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

// ── Index queue ──────────────────────────────────────────────────────────

pub static QUEUE_DEPTH: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let g = IntGaugeVec::new(
        Opts::new(
            "nestweaver_index_queue_depth",
            "Job queue depth by priority",
        ),
        &["priority"],
    )
    .unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

// ── Index jobs ───────────────────────────────────────────────────────────

pub static JOBS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let c = IntCounterVec::new(
        Opts::new("nestweaver_index_jobs_total", "Total index jobs by result"),
        &["result"],
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static JOB_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    let h = HistogramVec::new(
        HistogramOpts::new(
            "nestweaver_index_job_duration_seconds",
            "Index job duration",
        )
        .buckets(vec![0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0]),
        &[],
    )
    .unwrap();
    REGISTRY.register(Box::new(h.clone())).unwrap();
    h
});

// ── Webhooks ─────────────────────────────────────────────────────────────

pub static WEBHOOKS_RECEIVED: LazyLock<IntCounter> = LazyLock::new(|| {
    let c = IntCounter::new(
        "nestweaver_webhooks_received_total",
        "Total webhooks received",
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static WEBHOOK_SIG_FAILURES: LazyLock<IntCounter> = LazyLock::new(|| {
    let c = IntCounter::new(
        "nestweaver_webhook_signature_failures_total",
        "Total webhook signature verification failures",
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

// ── Polling ──────────────────────────────────────────────────────────────

pub static POLL_CHECKS: LazyLock<IntCounter> = LazyLock::new(|| {
    let c = IntCounter::new(
        "nestweaver_poll_checks_total",
        "Total poll checks performed",
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static POLL_CHANGES_DETECTED: LazyLock<IntCounter> = LazyLock::new(|| {
    let c = IntCounter::new(
        "nestweaver_poll_changes_detected_total",
        "Total poll checks that detected changes",
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

// ── Connections ──────────────────────────────────────────────────────────

pub static GRPC_CONNECTIONS: LazyLock<IntGauge> = LazyLock::new(|| {
    let g = IntGauge::new(
        "nestweaver_grpc_connections_active",
        "Active gRPC connections",
    )
    .unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

pub static MCP_SESSIONS: LazyLock<IntGauge> = LazyLock::new(|| {
    let g = IntGauge::new("nestweaver_mcp_sessions_active", "Active MCP sessions").unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

pub static ACTIVE_READS: LazyLock<IntGauge> = LazyLock::new(|| {
    let g = IntGauge::new("nestweaver_active_reads", "Active read operations").unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

pub static ACTIVE_WRITES: LazyLock<IntGauge> = LazyLock::new(|| {
    let g = IntGauge::new("nestweaver_active_writes", "Active write operations").unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

// ── Queries ──────────────────────────────────────────────────────────────

pub static QUERY_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    let h = HistogramVec::new(
        HistogramOpts::new(
            "nestweaver_query_duration_seconds",
            "Query duration by tool",
        )
        .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0]),
        &["tool"],
    )
    .unwrap();
    REGISTRY.register(Box::new(h.clone())).unwrap();
    h
});

pub static SLOW_QUERIES: LazyLock<IntCounter> = LazyLock::new(|| {
    let c = IntCounter::new(
        "nestweaver_slow_queries_total",
        "Total queries exceeding 80% of timeout",
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static QUERY_ERRORS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let c = IntCounterVec::new(
        Opts::new(
            "nestweaver_query_errors_total",
            "Total query errors by tool",
        ),
        &["tool"],
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

// ── gRPC requests ────────────────────────────────────────────────────────

pub static GRPC_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let c = IntCounterVec::new(
        Opts::new(
            "nestweaver_grpc_requests_total",
            "Total gRPC requests by method",
        ),
        &["method"],
    )
    .unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

// ── Handler ──────────────────────────────────────────────────────────────

/// Initialize all metrics by touching the lazy statics. Call once at server
/// startup so the `/metrics` endpoint always shows the full set of metric
/// names, even before any events occur.
pub fn init_metrics() {
    let _ = &*REPOS_TOTAL;
    let _ = &*QUEUE_DEPTH;
    let _ = &*JOBS_TOTAL;
    let _ = &*JOB_DURATION;
    let _ = &*WEBHOOKS_RECEIVED;
    let _ = &*WEBHOOK_SIG_FAILURES;
    let _ = &*POLL_CHECKS;
    let _ = &*POLL_CHANGES_DETECTED;
    let _ = &*GRPC_CONNECTIONS;
    let _ = &*MCP_SESSIONS;
    let _ = &*ACTIVE_READS;
    let _ = &*ACTIVE_WRITES;
    let _ = &*QUERY_DURATION;
    let _ = &*SLOW_QUERIES;
    let _ = &*QUERY_ERRORS;
    let _ = &*GRPC_REQUESTS;

    REPOS_TOTAL.with_label_values(&["indexed"]).set(0);
    QUEUE_DEPTH.with_label_values(&["total"]).set(0);
    JOBS_TOTAL.with_label_values(&["succeeded"]);
    JOBS_TOTAL.with_label_values(&["failed"]);
    JOBS_TOTAL.with_label_values(&["dead_letter"]);
    JOBS_TOTAL.with_label_values(&["cancelled"]);
    JOB_DURATION.with_label_values(&[] as &[&str]);
    QUERY_ERRORS.with_label_values(&["unknown"]);
    GRPC_REQUESTS.with_label_values(&["unknown"]);
}

/// Render the Prometheus text-format body from the shared registry.
fn render_metrics() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = REGISTRY.gather();
    let mut buffer = String::new();
    encoder.encode_utf8(&metric_families, &mut buffer).unwrap();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        buffer,
    )
}

/// Prometheus text-format endpoint. No auth. Mount this ONLY on a loopback /
/// admin-port listener where network-level access control restricts scrapers
/// (e.g. the local UI server); the network-facing MCP listener must use
/// [`metrics_authenticated`] instead — see S.5.
pub async fn metrics_handler() -> impl IntoResponse {
    render_metrics()
}

/// Bearer tokens accepted by the authenticated `/metrics` route. `None`
/// `auth_token` means auth is not configured (loopback dev) and the endpoint
/// stays open for local scrape convenience.
#[derive(Clone)]
pub struct MetricsAuthState {
    pub auth_token: Option<String>,
    pub admin_token: Option<String>,
}

/// Authenticated Prometheus `/metrics` endpoint for the network-facing MCP
/// listener (S.5). Operational counters (repo counts, queue depth, success /
/// failure rates) are a metadata leak on a non-loopback deployment, so this
/// route requires a valid bearer token — either the query `auth_token` or the
/// `admin_token`, matching how the MCP `/mcp` handler validates bearers.
///
/// When `auth_token` is `None` (no auth configured, i.e. a loopback-only dev
/// bind) the endpoint stays open, preserving the existing local-scrape
/// convenience. `validate_bind_security` forces `--auth-token` for any
/// non-loopback bind, so on the network this route is always gated.
pub async fn metrics_authenticated(
    axum::extract::State(auth): axum::extract::State<MetricsAuthState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use subtle::ConstantTimeEq;

    if let Some(ref expected) = auth.auth_token {
        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let ok = match provided {
            Some(t) => {
                let query_match = bool::from(t.as_bytes().ct_eq(expected.as_bytes()));
                let admin_match = auth
                    .admin_token
                    .as_ref()
                    .map(|a| bool::from(t.as_bytes().ct_eq(a.as_bytes())))
                    .unwrap_or(false);
                query_match || admin_match
            }
            None => false,
        };
        if !ok {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                "unauthorized: valid Bearer token required",
            )
                .into_response();
        }
    }

    render_metrics().into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn app(auth_token: Option<&str>, admin_token: Option<&str>) -> Router {
        init_metrics();
        Router::new()
            .route("/metrics", get(metrics_authenticated))
            .with_state(MetricsAuthState {
                auth_token: auth_token.map(String::from),
                admin_token: admin_token.map(String::from),
            })
    }

    async fn get_metrics(app: Router, bearer: Option<&str>) -> StatusCode {
        let mut req = Request::builder().method("GET").uri("/metrics");
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// On the network listener (auth configured) an unauthenticated scrape is
    /// rejected; a valid query or admin bearer is accepted.
    #[tokio::test]
    async fn metrics_requires_auth_on_network_listener() {
        // No bearer → 401.
        assert_eq!(
            get_metrics(app(Some("query-tok"), Some("admin-tok")), None).await,
            StatusCode::UNAUTHORIZED
        );
        // Wrong bearer → 401.
        assert_eq!(
            get_metrics(app(Some("query-tok"), Some("admin-tok")), Some("nope")).await,
            StatusCode::UNAUTHORIZED
        );
        // Valid query token → 200.
        assert_eq!(
            get_metrics(app(Some("query-tok"), Some("admin-tok")), Some("query-tok")).await,
            StatusCode::OK
        );
        // Valid admin token → 200.
        assert_eq!(
            get_metrics(app(Some("query-tok"), Some("admin-tok")), Some("admin-tok")).await,
            StatusCode::OK
        );
    }

    /// With no auth configured (loopback-only dev bind) the endpoint stays open
    /// so local Prometheus scrapes keep working without a token.
    #[tokio::test]
    async fn metrics_open_without_auth_token() {
        assert_eq!(get_metrics(app(None, None), None).await, StatusCode::OK);
    }
}

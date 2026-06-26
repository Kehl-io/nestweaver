use axum::http::header;
use axum::response::IntoResponse;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
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
    let g = IntGauge::new(
        "nestweaver_mcp_sessions_active",
        "Active MCP sessions",
    )
    .unwrap();
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
    let _ = &*QUERY_DURATION;
    let _ = &*SLOW_QUERIES;
    let _ = &*GRPC_REQUESTS;
}

/// Prometheus text-format endpoint. No auth required (standard practice —
/// use network-level access control to restrict scraper access).
pub async fn metrics_handler() -> impl IntoResponse {
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

//! nw-682: the UI server binds 127.0.0.1, but a browser page served from any
//! site can still reach it through DNS rebinding (the attacker's hostname
//! re-resolves to 127.0.0.1, making the page same-origin with this server)
//! or post to it cross-site. No CORS header is the defence against
//! cross-origin *reads* only. So every request must name a loopback Host,
//! and any Origin a browser attaches must be a loopback origin too.
//!
//! An absent Host is admitted: HTTP/1.1 browsers always send one, and
//! local non-browser clients and in-process tests may not.
//! `NESTWEAVER_UI_ALLOWED_HOSTS` (comma-separated host names, no ports) adds
//! names for a deliberate local reverse proxy.

use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Wrap every route of `router` in the Host/Origin allowlist and the
/// security response headers. Call this on the fully-assembled router
/// (after any `.nest`), because axum layers apply only to routes that
/// already exist.
pub fn harden(router: Router) -> Router {
    router
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(loopback_only))
}

fn extra_allowed_hosts() -> Vec<String> {
    std::env::var("NESTWEAVER_UI_ALLOWED_HOSTS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The host part of an authority (`host[:port]`, `[v6]:port`), lowercased.
fn host_of(authority: &str) -> String {
    let authority = authority.trim().to_ascii_lowercase();
    if authority.starts_with('[') {
        return match authority.find(']') {
            Some(end) => authority[..=end].to_string(),
            None => authority,
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_string(),
        _ => authority,
    }
}

fn host_allowed(host: &str, extra: &[String]) -> bool {
    LOOPBACK_HOSTS.contains(&host) || extra.iter().any(|h| h == host)
}

fn request_allowed(headers: &HeaderMap) -> bool {
    let extra = extra_allowed_hosts();
    if let Some(host) = headers.get(header::HOST) {
        let Ok(host) = host.to_str() else {
            return false;
        };
        if !host_allowed(&host_of(host), &extra) {
            return false;
        }
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let Ok(origin) = origin.to_str() else {
            return false;
        };
        let Some(rest) = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
        else {
            return false; // `null`, file://, anything opaque
        };
        let authority = rest.split('/').next().unwrap_or_default();
        if !host_allowed(&host_of(authority), &extra) {
            return false;
        }
    }
    true
}

async fn loopback_only(request: Request, next: Next) -> Response {
    if request_allowed(request.headers()) {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"forbidden_host","message":"The NestWeaver UI only answers loopback hosts (localhost, 127.0.0.1, [::1]). Set NESTWEAVER_UI_ALLOWED_HOSTS to admit a local reverse-proxy name."}"#,
        )
            .into_response()
    }
}

async fn security_headers(request: Request, next: Next) -> Response {
    next.run(request).await // Task 2 fills this in; kept as a pass-through here.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_strips_ports_and_keeps_v6_brackets() {
        assert_eq!(host_of("127.0.0.1:9377"), "127.0.0.1");
        assert_eq!(host_of("LOCALHOST"), "localhost");
        assert_eq!(host_of("[::1]:9377"), "[::1]");
        assert_eq!(host_of("evil.example:9377"), "evil.example");
    }
}

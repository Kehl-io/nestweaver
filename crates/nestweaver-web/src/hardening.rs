//! nw-682: the UI server binds 127.0.0.1, but a browser page served from any
//! site can still reach it through DNS rebinding (the attacker's hostname
//! re-resolves to 127.0.0.1, making the page same-origin with this server)
//! or post to it cross-site. No CORS header is the defence against
//! cross-origin *reads* only. So every request must name a loopback Host
//! (or URI authority, for absolute-form requests), and any Origin a browser
//! attaches must match.
//!
//! An absent Host/authority is admitted: HTTP/1.1 browsers always send a
//! Host, and local non-browser clients and in-process tests may not send
//! either. `NESTWEAVER_UI_ALLOWED_HOSTS` (comma-separated host names, no
//! ports) adds names for a deliberate local reverse proxy.

use std::sync::Arc;

use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Sentinel returned by [`host_of`] for a malformed bracketed authority
/// (e.g. `[::1]evil`, `[::1]:x`). Contains a NUL byte, which
/// `HeaderValue::to_str()` never yields for a header we accepted, so this
/// value can never equal a legitimately parsed Host/Origin/URI authority
/// and therefore never passes [`host_allowed`].
const UNMATCHABLE_HOST: &str = "\u{0}unmatchable";

/// Wrap every route of `router` in the Host/Origin allowlist (reading
/// `NESTWEAVER_UI_ALLOWED_HOSTS` once) and the security response headers.
/// Call this on the fully-assembled router (after any `.nest`), because
/// axum layers apply only to routes that already exist.
pub fn harden(router: Router) -> Router {
    harden_with_allowed_hosts(router, extra_allowed_hosts_from_env())
}

/// Like [`harden`], but with the allowlist of extra loopback-equivalent
/// hostnames supplied directly instead of read from
/// `NESTWEAVER_UI_ALLOWED_HOSTS`. Exists so callers (and tests) can
/// exercise the allowlist path without mutating process-global env state.
pub fn harden_with_allowed_hosts(router: Router, extra: Vec<String>) -> Router {
    // Normalise once, up front: host_of() also strips a port, so an entry
    // like "proxy.local:8080" in the list still matches a bare Host.
    let extra: Arc<Vec<String>> = Arc::new(extra.iter().map(|h| host_of(h)).collect());

    router
        // loopback_only is added first (innermost, closest to the routes).
        // security_headers is added last -> outermost, so it wraps
        // loopback_only too and stamps its 403s with the same security
        // headers as every other response (Task 2 fills the headers in;
        // this file only fixes the layer order).
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let extra = Arc::clone(&extra);
            async move { loopback_only(req, next, extra).await }
        }))
        .layer(middleware::from_fn(security_headers))
}

fn extra_allowed_hosts_from_env() -> Vec<String> {
    std::env::var("NESTWEAVER_UI_ALLOWED_HOSTS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The host part of an authority (`host[:port]`, `[v6]:port`), lowercased.
/// A bracketed host with trailing text that is neither empty nor a valid
/// `:port` (e.g. `[::1]evil`) is rejected via [`UNMATCHABLE_HOST`] rather
/// than silently truncated to `[::1]`.
fn host_of(authority: &str) -> String {
    let authority = authority.trim().to_ascii_lowercase();
    if let Some(rest) = authority.strip_prefix('[') {
        return match rest.find(']') {
            Some(end) => {
                let after = &rest[end + 1..];
                let port_ok = after.is_empty()
                    || (after.starts_with(':')
                        && !after[1..].is_empty()
                        && after[1..].chars().all(|c| c.is_ascii_digit()));
                if port_ok {
                    format!("[{}]", &rest[..end])
                } else {
                    UNMATCHABLE_HOST.to_string()
                }
            }
            None => UNMATCHABLE_HOST.to_string(),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            host.to_string()
        }
        _ => authority,
    }
}

fn host_allowed(host: &str, extra: &[String]) -> bool {
    LOOPBACK_HOSTS.contains(&host) || extra.iter().any(|h| h == host)
}

/// Why a request was refused, and the header/authority value that triggered
/// it (used only for the `tracing::debug!` line — never echoed back to the
/// caller, to avoid reflecting attacker-controlled text into a response).
enum Refusal {
    Host(String),
    Origin(String),
}

impl Refusal {
    fn respond(self) -> Response {
        let (code, value) = match &self {
            Refusal::Host(v) => ("forbidden_host", v),
            Refusal::Origin(v) => ("forbidden_origin", v),
        };
        tracing::debug!(
            refused_value = %value,
            error = code,
            "nestweaver-web: refused non-loopback request (nw-682)"
        );
        let body = format!(
            r#"{{"error":"{code}","message":"The NestWeaver UI only answers loopback hosts (localhost, 127.0.0.1, [::1]) with a matching Origin, if one is sent. Set NESTWEAVER_UI_ALLOWED_HOSTS to admit a local reverse-proxy name."}}"#
        );
        (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    }
}

/// `uri_authority` is `request.uri().authority()` as a string, when the
/// request line used absolute-form (h2c, or a proxy-style request). It is
/// `None` for the normal relative-form requests browsers and in-process
/// tests send.
fn request_allowed(
    headers: &HeaderMap,
    uri_authority: Option<&str>,
    extra: &[String],
) -> Result<(), Refusal> {
    // The authority this request claims to be for: the Host header if
    // present, else the URI's own authority (absolute-form). When both are
    // present, each is checked independently — a request must not be able
    // to pass by putting a loopback name in one and smuggling a different
    // authority into the other.
    let mut host_authority: Option<String> = None;

    if let Some(host) = headers.get(header::HOST) {
        let Ok(host) = host.to_str() else {
            return Err(Refusal::Host("<unparseable>".to_string()));
        };
        if !host_allowed(&host_of(host), extra) {
            return Err(Refusal::Host(host.to_string()));
        }
        host_authority = Some(host.to_string());
    }

    if let Some(uri_authority) = uri_authority {
        if !host_allowed(&host_of(uri_authority), extra) {
            return Err(Refusal::Host(uri_authority.to_string()));
        }
        if host_authority.is_none() {
            host_authority = Some(uri_authority.to_string());
        }
    }

    if let Some(origin) = headers.get(header::ORIGIN) {
        let Ok(origin) = origin.to_str() else {
            return Err(Refusal::Origin("<unparseable>".to_string()));
        };
        let Some(rest) = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
        else {
            return Err(Refusal::Origin(origin.to_string())); // `null`, file://, anything opaque
        };
        let origin_authority = rest.split('/').next().unwrap_or_default();

        match &host_authority {
            Some(host_authority) => {
                // Both an authoritative Host (header or URI) and an Origin
                // are present: require them to name the exact same
                // authority (host+port), not just "some loopback name" --
                // otherwise a rebound page listening on :3000 could pass an
                // Origin whose port differs from the Host it is actually
                // talking to.
                if origin_authority.trim().to_ascii_lowercase()
                    != host_authority.trim().to_ascii_lowercase()
                {
                    return Err(Refusal::Origin(origin.to_string()));
                }
            }
            None => {
                // No Host header and no URI authority (in-process test, or
                // a non-browser client that only sent Origin): fall back to
                // the loopback allowlist for the Origin alone.
                if !host_allowed(&host_of(origin_authority), extra) {
                    return Err(Refusal::Origin(origin.to_string()));
                }
            }
        }
    }

    Ok(())
}

async fn loopback_only(request: Request, next: Next, extra: Arc<Vec<String>>) -> Response {
    let uri_authority = request.uri().authority().map(|a| a.as_str().to_string());
    match request_allowed(request.headers(), uri_authority.as_deref(), &extra) {
        Ok(()) => next.run(request).await,
        Err(refusal) => refusal.respond(),
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

    #[test]
    fn host_of_rejects_malformed_bracketed_authority() {
        // Trailing garbage after `]` that isn't a valid `:port` must not be
        // silently truncated down to a loopback-looking host.
        assert!(!host_allowed(&host_of("[::1]evil"), &[]));
        assert!(!host_allowed(&host_of("[::1]:x"), &[]));
        assert!(host_allowed(&host_of("[::1]:9377"), &[]));
    }
}

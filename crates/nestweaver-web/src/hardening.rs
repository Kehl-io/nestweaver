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
//! either. `NESTWEAVER_UI_ALLOWED_HOSTS` (comma-separated hostnames) adds
//! names for a deliberate local reverse proxy; a `:port` on an entry is
//! stripped (ports are never part of the comparison), so `proxy.local:8080`
//! and `proxy.local` are equivalent. Because Origin must equal Host exactly
//! when both are present, that proxy must forward the browser's original
//! Host header unchanged rather than rewriting it to its own upstream
//! address -- e.g. nginx needs `proxy_set_header Host $host;`.

use std::sync::Arc;

use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

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
    // like "proxy.local:8080" in the list still matches a bare Host. A
    // malformed entry is dropped rather than mapped to a shared
    // placeholder: collapsing distinct malformed strings into one
    // "unmatchable" value would make them match *each other* through this
    // very allowlist, which defeats the point of rejecting them.
    let extra: Arc<Vec<String>> = Arc::new(
        extra
            .iter()
            .filter_map(|h| match host_of(h) {
                Some(normalized) => Some(normalized),
                None => {
                    tracing::warn!(
                        raw_entry = %h,
                        "nestweaver-web: ignoring malformed NESTWEAVER_UI_ALLOWED_HOSTS entry (nw-682)"
                    );
                    None
                }
            })
            .collect(),
    );

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
/// `None` means the authority is malformed and must be refused outright: an
/// unclosed `[`, or a bracketed host followed by trailing text that is
/// neither empty nor a valid `:port` (e.g. `[::1]evil`, `[::1]:x`). Such
/// input is never mapped to a placeholder string -- two different
/// malformed authorities must never compare equal to each other.
fn host_of(authority: &str) -> Option<String> {
    let authority = authority.trim().to_ascii_lowercase();
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let after = &rest[end + 1..];
        let port_ok = after.is_empty()
            || (after.starts_with(':')
                && !after[1..].is_empty()
                && after[1..].chars().all(|c| c.is_ascii_digit()));
        return port_ok.then(|| format!("[{}]", &rest[..end]));
    }
    Some(match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            host.to_string()
        }
        _ => authority,
    })
}

fn host_allowed(host: &str, extra: &[String]) -> bool {
    LOOPBACK_HOSTS.contains(&host) || extra.iter().any(|h| h == host)
}

/// Parse `authority` and check it against the loopback/extra allowlist in
/// one step. A malformed authority (`host_of` returns `None`) is always
/// refused -- it never falls back to the allowlist for `None` itself, only
/// for a successfully parsed host.
fn authority_allowed(authority: &str, extra: &[String]) -> bool {
    match host_of(authority) {
        Some(host) => host_allowed(&host, extra),
        None => false,
    }
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
            r#"{{"error":"{code}","message":"The NestWeaver UI only answers loopback hosts (localhost, 127.0.0.1, [::1]) with a matching Origin, if one is sent. Set NESTWEAVER_UI_ALLOWED_HOSTS to admit a local reverse-proxy name -- the proxy must forward the browser's original Host header unchanged (e.g. nginx's `proxy_set_header Host $host;`), since Origin must equal Host exactly."}}"#
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
        if !authority_allowed(host, extra) {
            return Err(Refusal::Host(host.to_string()));
        }
        host_authority = Some(host.to_string());
    }

    if let Some(uri_authority) = uri_authority {
        if !authority_allowed(uri_authority, extra) {
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
                if !authority_allowed(origin_authority, extra) {
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
        assert_eq!(host_of("127.0.0.1:9377"), Some("127.0.0.1".to_string()));
        assert_eq!(host_of("LOCALHOST"), Some("localhost".to_string()));
        assert_eq!(host_of("[::1]:9377"), Some("[::1]".to_string()));
        assert_eq!(
            host_of("evil.example:9377"),
            Some("evil.example".to_string())
        );
    }

    #[test]
    fn host_of_returns_none_for_malformed_authority() {
        // Trailing garbage after `]` that isn't a valid `:port`, or an
        // unclosed `[`, must not be silently truncated down to a
        // loopback-looking host -- and must not collapse to a shared
        // placeholder either, since that would make two different
        // malformed authorities compare equal to each other.
        assert_eq!(host_of("[bad"), None);
        assert_eq!(host_of("[::1]evil"), None);
        assert_eq!(host_of("[::1]:x"), None);
        assert_eq!(host_of("[::1]:9377"), Some("[::1]".to_string()));
    }

    #[test]
    fn authority_allowed_refuses_malformed_authorities_outright() {
        assert!(!authority_allowed("[::1]evil", &[]));
        assert!(!authority_allowed("[::1]:x", &[]));
        assert!(authority_allowed("[::1]:9377", &[]));
    }
}

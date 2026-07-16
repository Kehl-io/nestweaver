//! Bearer token authentication interceptor for TCP transport.
//!
//! When `--auth-token` is set, the interceptor rejects TCP requests that
//! lack a valid `Authorization: Bearer <token>` header with
//! `UNAUTHENTICATED`. UDS remains unauthenticated for backward
//! compatibility.
//!
//! When rate limiters are configured, the interceptor also checks per-client
//! rate limits after token validation. Admin tokens bypass rate limiting.

use std::sync::Arc;

use tonic::{Request, Status};

use crate::safeguards::ClientRateLimiters;

/// Marker inserted into request extensions by the auth interceptor.
/// Handlers can extract this to gate destructive operations.
#[derive(Debug, Clone, Copy)]
pub struct IsAdmin(pub bool);

/// Constant-time byte comparison for authentication tokens.
/// Prevents timing side-channel attacks (CWE-208).
/// Note: returns false immediately for different-length inputs (length is not
/// constant-time). This is acceptable for high-entropy tokens where length is
/// not sensitive.
pub fn secure_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

fn rate_limit_key(req: &Request<()>, bearer: &str) -> String {
    req.remote_addr()
        .map(|addr| format!("peer:{}", addr.ip()))
        .unwrap_or_else(|| bearer.to_string())
}

/// Derive the caller's authorization [`Identity`] for an already-validated
/// bearer (R9/R9b — per-repo Blast Radius scoping).
///
/// By the time this runs the bearer has already matched either the admin token
/// or the query token — a non-match returns `UNAUTHENTICATED` earlier in the
/// interceptor, so the result is never `Anonymous`. `is_admin` is the
/// constant-time admin-match already computed by the interceptor (not
/// recomputed here). Admin ⇒ [`Identity::Admin`]; otherwise the query-token
/// value keys [`Identity::Token`], matching the MCP-HTTP `resolve_identity`
/// contract so both transports scope visibility identically.
pub fn derive_identity(bearer: &str, is_admin: bool) -> nestweaver_engine::authz::Identity {
    use nestweaver_engine::authz::Identity;
    if is_admin {
        Identity::Admin
    } else {
        Identity::Token(bearer.to_string())
    }
}

/// Returns a tonic interceptor that validates bearer tokens and enforces
/// per-client rate limits.
///
/// * `expected_token = None` — all requests pass through (no auth).
/// * `expected_token = Some(token)` — requests must carry
///   `Authorization: Bearer <token>` or they are rejected.
/// * `admin_token` — if the request's token matches the admin token,
///   rate limiting is bypassed.
/// * `rate_limiters` — when `Some`, per-client rate limiting is enforced.
pub fn bearer_auth_interceptor(
    expected_token: Option<String>,
    admin_token: Option<String>,
    rate_limiters: Option<Arc<ClientRateLimiters>>,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |req: Request<()>| {
        let Some(ref token) = expected_token else {
            return Ok(req);
        };
        let auth = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        match auth {
            Some(value) if value.starts_with("Bearer ") => {
                let bearer = &value[7..];

                // Constant-time token comparison to prevent timing side-channel attacks.
                use subtle::ConstantTimeEq;
                let token_match: bool = bearer.as_bytes().ct_eq(token.as_bytes()).into();
                let is_admin = admin_token
                    .as_ref()
                    .map(|admin| bool::from(bearer.as_bytes().ct_eq(admin.as_bytes())))
                    .unwrap_or(false);

                if !token_match && !is_admin {
                    return Err(Status::unauthenticated("missing or invalid bearer token"));
                }

                // Rate limit check — admin tokens are exempt.
                if !is_admin && let Some(ref rl) = rate_limiters {
                    rl.check(&rate_limit_key(&req, bearer))?;
                }

                // R9/R9b: derive the caller's authorization identity while
                // `bearer` still borrows the request metadata (before the move
                // below). The bearer already matched (admin or query token) — a
                // non-match returned above — so this is never `Anonymous`.
                let identity = derive_identity(bearer, is_admin);

                let mut req = req;
                req.extensions_mut().insert(IsAdmin(is_admin));
                // Attach the identity so handlers can scope Blast Radius output
                // to the caller's visible repos. `IsAdmin` (the mutation gate)
                // is left untouched.
                req.extensions_mut().insert(identity);
                Ok(req)
            }
            _ => Err(Status::unauthenticated("missing or invalid bearer token")),
        }
    }
}

/// Interceptor for Unix domain socket connections.
///
/// UDS connections are implicitly trusted: the OS enforces file-system
/// permissions on the socket, so only local processes running as the same
/// user can connect. This interceptor unconditionally grants admin access
/// so that CLI operations (shutdown, indexing, backup, etc.) work without
/// requiring a bearer token.
pub fn uds_admin_interceptor(mut req: Request<()>) -> Result<Request<()>, Status> {
    req.extensions_mut().insert(IsAdmin(true));
    Ok(req)
}

/// Returns a simple bearer-token interceptor without rate limiting.
/// Used for backward compatibility in non-server mode.
pub fn bearer_auth_interceptor_simple(
    expected_token: Option<String>,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    bearer_auth_interceptor(expected_token, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safeguards::RateLimitConfig;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tonic::metadata::MetadataValue;
    use tonic::transport::server::TcpConnectInfo;

    fn request_with_token(token: &str) -> Request<()> {
        let mut req = Request::new(());
        req.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {}", token)).unwrap(),
        );
        req
    }

    fn request_with_token_from_ip(token: &str, ip: [u8; 4]) -> Request<()> {
        let mut req = request_with_token(token);
        req.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(ip)),
                50_000 + u16::from(ip[3]),
            )),
        });
        req
    }

    #[test]
    fn no_auth_passes_all() {
        let f = bearer_auth_interceptor(None, None, None);
        assert!(f(Request::new(())).is_ok());
    }

    #[test]
    fn valid_token_passes() {
        let f = bearer_auth_interceptor(Some("secret".into()), None, None);
        assert!(f(request_with_token("secret")).is_ok());
    }

    #[test]
    fn wrong_token_rejected() {
        let f = bearer_auth_interceptor(Some("secret".into()), None, None);
        let r = f(request_with_token("wrong"));
        assert_eq!(r.unwrap_err().code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn missing_header_rejected() {
        let f = bearer_auth_interceptor(Some("secret".into()), None, None);
        assert!(f(Request::new(())).is_err());
    }

    #[test]
    fn admin_token_bypasses_rate_limit() {
        let config = RateLimitConfig {
            requests_per_minute: 60,
            burst: 1,
            enabled: true,
        };
        let rl = Arc::new(ClientRateLimiters::new(&config));
        let f = bearer_auth_interceptor(
            Some("query-token".into()),
            Some("admin-token".into()),
            Some(rl),
        );
        // Admin token should always pass (no rate limit)
        for _ in 0..50 {
            assert!(f(request_with_token("admin-token")).is_ok());
        }
    }

    #[test]
    fn admin_token_attaches_admin_identity() {
        use nestweaver_engine::authz::Identity;
        let f =
            bearer_auth_interceptor(Some("query-token".into()), Some("admin-token".into()), None);
        let req = f(request_with_token("admin-token")).unwrap();
        assert_eq!(
            req.extensions().get::<Identity>(),
            Some(&Identity::Admin),
            "admin bearer must resolve to Identity::Admin"
        );
        // The mutation gate (IsAdmin) must still be set correctly.
        assert!(matches!(
            req.extensions().get::<IsAdmin>(),
            Some(IsAdmin(true))
        ));
    }

    #[test]
    fn query_token_attaches_token_identity() {
        use nestweaver_engine::authz::Identity;
        let f =
            bearer_auth_interceptor(Some("query-token".into()), Some("admin-token".into()), None);
        let req = f(request_with_token("query-token")).unwrap();
        assert_eq!(
            req.extensions().get::<Identity>(),
            Some(&Identity::Token("query-token".to_string())),
            "query bearer must resolve to Identity::Token(<value>)"
        );
        // Query token is not admin — the mutation gate stays false.
        assert!(matches!(
            req.extensions().get::<IsAdmin>(),
            Some(IsAdmin(false))
        ));
    }

    #[test]
    fn no_auth_attaches_no_identity() {
        use nestweaver_engine::authz::Identity;
        // When auth is disabled the early-return path inserts no extensions;
        // handlers treat the absent identity as Anonymous.
        let f = bearer_auth_interceptor(None, None, None);
        let req = f(Request::new(())).unwrap();
        assert!(req.extensions().get::<Identity>().is_none());
        assert!(req.extensions().get::<IsAdmin>().is_none());
    }

    #[test]
    fn derive_identity_maps_admin_and_token() {
        use nestweaver_engine::authz::Identity;
        assert_eq!(super::derive_identity("anything", true), Identity::Admin);
        assert_eq!(
            super::derive_identity("query-token", false),
            Identity::Token("query-token".to_string())
        );
    }

    #[test]
    fn secure_eq_matches_identical() {
        assert!(super::secure_eq(b"secret-token", b"secret-token"));
    }

    #[test]
    fn secure_eq_rejects_different() {
        assert!(!super::secure_eq(b"secret-token", b"wrong-token"));
    }

    #[test]
    fn secure_eq_rejects_different_lengths() {
        assert!(!super::secure_eq(b"short", b"longer-token"));
    }

    #[test]
    fn rate_limit_applied_to_query_token() {
        let config = RateLimitConfig {
            requests_per_minute: 60,
            burst: 2,
            enabled: true,
        };
        let rl = Arc::new(ClientRateLimiters::new(&config));
        let f = bearer_auth_interceptor(
            Some("query-token".into()),
            Some("admin-token".into()),
            Some(rl),
        );
        // First 2 should pass (burst)
        assert!(f(request_with_token("query-token")).is_ok());
        assert!(f(request_with_token("query-token")).is_ok());
        // Third should be rate-limited
        let err = f(request_with_token("query-token")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn rate_limit_is_keyed_by_remote_peer_when_available() {
        let config = RateLimitConfig {
            requests_per_minute: 60,
            burst: 1,
            enabled: true,
        };
        let rl = Arc::new(ClientRateLimiters::new(&config));
        let f = bearer_auth_interceptor(
            Some("shared-query-token".into()),
            Some("admin-token".into()),
            Some(rl),
        );

        assert!(
            f(request_with_token_from_ip(
                "shared-query-token",
                [10, 0, 0, 1]
            ))
            .is_ok()
        );
        let err = f(request_with_token_from_ip(
            "shared-query-token",
            [10, 0, 0, 1],
        ))
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);

        assert!(
            f(request_with_token_from_ip(
                "shared-query-token",
                [10, 0, 0, 2]
            ))
            .is_ok(),
            "a second TCP peer using the same shared bearer token must get an independent bucket"
        );
    }
}

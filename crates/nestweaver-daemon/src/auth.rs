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

                // Check if this is the admin token — admin bypasses rate limiting.
                let is_admin = admin_token
                    .as_ref()
                    .is_some_and(|admin| bearer == admin.as_str());

                // Validate: accept if token matches auth token OR admin token.
                if bearer != token.as_str() && !is_admin {
                    return Err(Status::unauthenticated("missing or invalid bearer token"));
                }

                // Rate limit check — admin tokens are exempt.
                if !is_admin {
                    if let Some(ref rl) = rate_limiters {
                        rl.check(bearer)?;
                    }
                }

                Ok(req)
            }
            _ => Err(Status::unauthenticated("missing or invalid bearer token")),
        }
    }
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
    use tonic::metadata::MetadataValue;

    fn request_with_token(token: &str) -> Request<()> {
        let mut req = Request::new(());
        req.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {}", token)).unwrap(),
        );
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
}

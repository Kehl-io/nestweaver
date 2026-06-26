//! Bearer token authentication interceptor for TCP transport.
//!
//! When `--auth-token` is set, the interceptor rejects TCP requests that
//! lack a valid `Authorization: Bearer <token>` header with
//! `UNAUTHENTICATED`. UDS remains unauthenticated for backward
//! compatibility.

use tonic::{Request, Status};

/// Returns a tonic interceptor that validates bearer tokens.
///
/// * `expected_token = None` — all requests pass through (no auth).
/// * `expected_token = Some(token)` — requests must carry
///   `Authorization: Bearer <token>` or they are rejected.
pub fn bearer_auth_interceptor(
    expected_token: Option<String>,
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
            Some(value) if value.starts_with("Bearer ") && &value[7..] == token.as_str() => {
                Ok(req)
            }
            _ => Err(Status::unauthenticated("missing or invalid bearer token")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let f = bearer_auth_interceptor(None);
        assert!(f(Request::new(())).is_ok());
    }

    #[test]
    fn valid_token_passes() {
        let f = bearer_auth_interceptor(Some("secret".into()));
        assert!(f(request_with_token("secret")).is_ok());
    }

    #[test]
    fn wrong_token_rejected() {
        let f = bearer_auth_interceptor(Some("secret".into()));
        let r = f(request_with_token("wrong"));
        assert_eq!(r.unwrap_err().code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn missing_header_rejected() {
        let f = bearer_auth_interceptor(Some("secret".into()));
        assert!(f(Request::new(())).is_err());
    }
}

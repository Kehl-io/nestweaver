//! Webhook endpoint for GitHub/GitLab push events.
//!
//! Verifies HMAC-SHA256 signatures, extracts the repo URL from the payload,
//! and enqueues a job at webhook priority. Supports dual-secret rotation.
//!
//! The handler never does indexing work — it returns 200 immediately and lets
//! the worker pool handle the actual index update.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use nestweaver_engine::jobs::{JobQueue, JobTrigger};

/// Configuration for webhook signature verification.
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// Current webhook secret (checked first).
    pub secret: String,
    /// Previous webhook secret (checked as fallback during rotation).
    pub secret_old: Option<String>,
}

/// Shared state for the webhook handler.
pub struct WebhookState {
    pub config: WebhookConfig,
    pub job_queue: Arc<Mutex<JobQueue>>,
    /// When `Some`, only repos whose canonical ID is in this set get enqueued.
    /// Repos with `poll = "manual"` or not in the instance config are excluded.
    /// When `None`, all validly-signed webhooks are accepted (backwards-compat).
    /// Wrapped in Arc<RwLock> so `/admin/api/reload` can update it without restart.
    pub allowed_repos: Arc<RwLock<Option<HashSet<String>>>>,
    /// Configured branch per repo (canonical_id → branch). When a webhook
    /// fires for a repo with a configured branch, the job carries that branch
    /// so the worker indexes the correct ref instead of defaulting to HEAD.
    /// Wrapped in Arc<RwLock> so `/admin/api/reload` can update it without restart.
    pub repo_branches: Arc<RwLock<HashMap<String, String>>>,
}

/// POST /webhook — receives push events from GitHub/GitLab.
///
/// 1. Verifies the HMAC-SHA256 signature against configured secret(s).
/// 2. Extracts the repo URL from the JSON payload.
/// 3. Enqueues a job with webhook priority (the worker discovers HEAD itself).
/// 4. Returns 200 immediately.
pub async fn handle_webhook(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Verify webhook authentication.
    // GitLab uses X-Gitlab-Token with a plain secret comparison (no HMAC).
    // GitHub uses X-Hub-Signature-256 with HMAC-SHA256.
    let gitlab_token = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok());
    let sig_header = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());

    // Track all incoming webhook requests.
    nestweaver_web::routes::metrics::WEBHOOKS_RECEIVED.inc();

    if let Some(token) = gitlab_token {
        let current_match =
            crate::auth::secure_eq(token.as_bytes(), state.config.secret.as_bytes());
        let old_match = state
            .config
            .secret_old
            .as_deref()
            .is_some_and(|old| crate::auth::secure_eq(token.as_bytes(), old.as_bytes()));
        if !current_match && !old_match {
            nestweaver_web::routes::metrics::WEBHOOK_SIG_FAILURES.inc();
            return (StatusCode::UNAUTHORIZED, "invalid token");
        }
        if old_match && !current_match {
            tracing::warn!("GitLab webhook matched old secret — rotate to new secret");
        }
    } else if !verify_signature(&body, sig_header, &state.config) {
        nestweaver_web::routes::metrics::WEBHOOK_SIG_FAILURES.inc();
        return (StatusCode::UNAUTHORIZED, "invalid signature");
    }

    // 2. Parse payload to extract repo URL.
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON"),
    };

    let Some(url) = extract_repo_url(&payload) else {
        return (StatusCode::BAD_REQUEST, "no repo URL in payload");
    };

    // 2b. Check whether this repo is in the allowed set.
    let repo_id = nestweaver_engine::jobs::canonical_repo_id(&url);
    if let Ok(allowed_guard) = state.allowed_repos.read()
        && let Some(ref allowed) = *allowed_guard
        && !allowed.contains(&repo_id)
    {
        tracing::info!(%url, "webhook ignored: repo not in allowed set");
        return (StatusCode::OK, "ignored");
    }

    // 3. Enqueue job with the configured branch (if any).
    let branch: Option<String> = state
        .repo_branches
        .read()
        .ok()
        .and_then(|g| g.get(&repo_id).cloned());
    let enqueue_result = {
        let queue = state.job_queue.lock().expect("job queue lock poisoned");
        queue.upsert(&repo_id, &url, JobTrigger::Webhook, branch.as_deref())
    };

    if let Err(e) = enqueue_result {
        tracing::error!(%repo_id, error = %e, "failed to enqueue webhook job");
        return (StatusCode::INTERNAL_SERVER_ERROR, "enqueue failed");
    }

    tracing::info!(%repo_id, %url, "webhook enqueued job");
    (StatusCode::OK, "accepted")
}

/// Verify the HMAC-SHA256 signature from the `X-Hub-Signature-256` header.
///
/// Tries the current secret first, then falls back to the old secret for
/// dual-secret rotation. Logs a deprecation warning when the old secret matches.
fn verify_signature(body: &[u8], sig_header: Option<&str>, config: &WebhookConfig) -> bool {
    let Some(sig_str) = sig_header else {
        return false;
    };
    let sig_hex = sig_str.strip_prefix("sha256=").unwrap_or(sig_str);

    // Try current secret first.
    if verify_hmac(body, sig_hex, &config.secret) {
        return true;
    }

    // Fall back to old secret (dual-secret rotation).
    if let Some(ref old) = config.secret_old
        && verify_hmac(body, sig_hex, old)
    {
        tracing::warn!("webhook matched old secret — rotate to new secret");
        return true;
    }

    false
}

/// Check whether `expected_hex` is a valid HMAC-SHA256 of `body` using `secret`.
fn verify_hmac(body: &[u8], expected_hex: &str, secret: &str) -> bool {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let Ok(expected) = hex::decode(expected_hex) else {
        return false;
    };
    mac.verify_slice(&expected).is_ok()
}

/// Extract the clone URL from a webhook payload.
///
/// Supports:
/// - GitHub: `repository.clone_url`
/// - GitLab: `project.git_http_url`
fn extract_repo_url(payload: &serde_json::Value) -> Option<String> {
    payload["repository"]["clone_url"]
        .as_str()
        .or_else(|| payload["project"]["git_http_url"].as_str())
        .map(|s| s.to_string())
}

// ── Unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Compute a valid HMAC-SHA256 signature for the given body and secret.
    fn sign(body: &[u8], secret: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
        mac.update(body);
        let result = mac.finalize().into_bytes();
        format!("sha256={}", hex::encode(result))
    }

    #[test]
    fn verify_valid_github_signature() {
        let secret = "test-secret-123";
        let body = b"hello world";
        let sig = sign(body, secret);
        let config = WebhookConfig {
            secret: secret.to_string(),
            secret_old: None,
        };
        assert!(verify_signature(body, Some(&sig), &config));
    }

    #[test]
    fn verify_invalid_signature() {
        let config = WebhookConfig {
            secret: "correct-secret".to_string(),
            secret_old: None,
        };
        let body = b"payload";
        let wrong_sig = sign(body, "wrong-secret");
        assert!(!verify_signature(body, Some(&wrong_sig), &config));
    }

    #[test]
    fn verify_missing_header_rejected() {
        let config = WebhookConfig {
            secret: "secret".to_string(),
            secret_old: None,
        };
        assert!(!verify_signature(b"body", None, &config));
    }

    #[test]
    fn verify_dual_secret_old_matches() {
        let body = b"some payload";
        let old_secret = "old-secret";
        let new_secret = "new-secret";
        let config = WebhookConfig {
            secret: new_secret.to_string(),
            secret_old: Some(old_secret.to_string()),
        };
        // Sign with the old secret — should still verify.
        let sig = sign(body, old_secret);
        assert!(verify_signature(body, Some(&sig), &config));
    }

    #[test]
    fn verify_dual_secret_new_takes_precedence() {
        let body = b"some payload";
        let old_secret = "old-secret";
        let new_secret = "new-secret";
        let config = WebhookConfig {
            secret: new_secret.to_string(),
            secret_old: Some(old_secret.to_string()),
        };
        // Sign with the new secret — should verify without the deprecation path.
        let sig = sign(body, new_secret);
        assert!(verify_signature(body, Some(&sig), &config));
    }

    #[test]
    fn verify_malformed_hex_rejected() {
        let config = WebhookConfig {
            secret: "secret".to_string(),
            secret_old: None,
        };
        assert!(!verify_signature(
            b"body",
            Some("sha256=not_valid_hex!!!"),
            &config
        ));
    }

    #[test]
    fn extract_github_repo_url() {
        let payload = serde_json::json!({
            "repository": {
                "clone_url": "https://github.com/acme/api-service.git"
            }
        });
        assert_eq!(
            extract_repo_url(&payload).as_deref(),
            Some("https://github.com/acme/api-service.git")
        );
    }

    #[test]
    fn extract_gitlab_repo_url() {
        let payload = serde_json::json!({
            "project": {
                "git_http_url": "https://gitlab.com/acme/api-service.git"
            }
        });
        assert_eq!(
            extract_repo_url(&payload).as_deref(),
            Some("https://gitlab.com/acme/api-service.git")
        );
    }

    #[test]
    fn extract_missing_repo_url() {
        let payload = serde_json::json!({"action": "created"});
        assert!(extract_repo_url(&payload).is_none());
    }

    #[test]
    fn extract_github_preferred_over_gitlab() {
        // When both fields exist, GitHub's clone_url wins.
        let payload = serde_json::json!({
            "repository": { "clone_url": "https://github.com/a/b.git" },
            "project": { "git_http_url": "https://gitlab.com/a/b.git" }
        });
        assert_eq!(
            extract_repo_url(&payload).as_deref(),
            Some("https://github.com/a/b.git")
        );
    }
}

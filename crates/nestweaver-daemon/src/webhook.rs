//! Webhook endpoint for GitHub/GitLab/Gitea push events.
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
    /// Raw configured clone URL per repo (canonical_id → url, nw-909). A matched
    /// webhook enqueues THIS url (never the payload's), so the worker clones
    /// with the configured transport/credentials. Refreshed with
    /// `allowed_repos` on `/admin/api/reload` and repo add/remove.
    pub repo_urls: Arc<RwLock<HashMap<String, String>>>,
}

/// POST /webhook — receives push events from GitHub/GitLab/Gitea.
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
    // Gitea uses X-Gitea-Signature with raw HMAC-SHA256 hex.
    let gitlab_token = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok());
    let sig_header = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    let gitea_sig_header = headers
        .get("x-gitea-signature")
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
    } else if !verify_signature(&body, sig_header, &state.config)
        && !verify_gitea_signature(&body, gitea_sig_header, &state.config)
    {
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

    // 2a. SSRF gate: reject a payload-supplied URL pointing at an internal/
    // private target (or a disallowed scheme) before it can be enqueued and
    // cloned. Synchronous checks only — the worker's clone/fetch-time guard
    // (`ssrf::guard_git_url`) does the resolve-time pinning.
    if nestweaver_engine::ssrf::validate_repo_url(&url).is_err() {
        tracing::warn!(%url, "webhook rejected: repo URL failed SSRF validation");
        return (StatusCode::BAD_REQUEST, "rejected repo URL");
    }

    // 2b. Check whether this repo is in the allowed set. A forge reports the
    // https clone URL even when the repo is configured over ssh (nw-909), so
    // fall back to comparing normalized host/path identities; the payload URL
    // is only ever COMPARED against configured entries. On a match the job is
    // keyed and cloned by the CONFIGURED repo (its canonical id and raw
    // configured URL), never by the payload's URL.
    let mut repo_id = nestweaver_engine::jobs::canonical_repo_id(&url);
    let mut url = url;
    if let Ok(allowed_guard) = state.allowed_repos.read()
        && let Some(ref allowed) = *allowed_guard
    {
        let urls = state.repo_urls.read().ok();
        match resolve_configured_repo(allowed, urls.as_deref(), &url) {
            Some(configured_id) => {
                if let Some(raw) = urls.as_deref().and_then(|m| m.get(&configured_id)) {
                    url = raw.clone();
                } else {
                    url = configured_id.clone();
                }
                repo_id = configured_id;
            }
            None => {
                tracing::info!(%url, "webhook ignored: repo not in allowed set");
                return (StatusCode::OK, "ignored");
            }
        }
    }

    // 3. Enqueue job with the configured branch (if any).
    let branch: Option<String> = state
        .repo_branches
        .read()
        .ok()
        .and_then(|g| g.get(&repo_id).cloned());
    let enqueue_result = {
        let queue = state.job_queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.upsert(&repo_id, &url, JobTrigger::Webhook, branch.as_deref())
    };

    if let Err(e) = enqueue_result {
        tracing::error!(%repo_id, error = %e, "failed to enqueue webhook job");
        return (StatusCode::INTERNAL_SERVER_ERROR, "enqueue failed");
    }

    tracing::info!(%repo_id, %url, "webhook enqueued job");
    (StatusCode::OK, "accepted")
}

/// Find the configured repo id (an element of `allowed`) that `url` names.
///
/// An exact [`canonical_repo_id`](nestweaver_engine::jobs::canonical_repo_id)
/// match wins. Otherwise the payload URL's [`webhook_identity_key`] is compared
/// with `normalized_repo_key` of each configured repo's RAW URL (`urls`, falling
/// back to the id itself). With several equivalent configured spellings the
/// smallest id is chosen so the result is deterministic.
fn resolve_configured_repo(
    allowed: &HashSet<String>,
    urls: Option<&HashMap<String, String>>,
    url: &str,
) -> Option<String> {
    let exact = nestweaver_engine::jobs::canonical_repo_id(url);
    if allowed.contains(&exact) {
        return Some(exact);
    }
    let identity = webhook_identity_key(url)?;
    allowed
        .iter()
        .filter(|id| {
            let configured = urls.and_then(|m| m.get(*id)).unwrap_or(id);
            nestweaver_schema::repo_url::normalized_repo_key(configured) == identity
        })
        .min()
        .cloned()
}

/// Identity key of a payload-supplied clone URL, or `None` to fail closed.
///
/// The key is `nestweaver_schema::repo_url::normalized_repo_key` (the single
/// normalizer: scheme, userinfo, `.git`, trailing `/` and case folded; a
/// non-default port stays part of the identity), computed from a parse that
/// agrees with the SSRF validator's `url::Url`. Anything where a naive reader
/// and `url::Url` could disagree about the host or path is rejected: a
/// backslash, ANY percent-escape, dot-segments, or an authority that differs
/// from the parsed `[userinfo@]host[:port]` (IDN, `host\@evil`, ...). scp-like
/// `user@host:path` is accepted only with no `/` before the first `:`.
fn webhook_identity_key(raw: &str) -> Option<String> {
    if raw.contains('\\') || raw.contains('%') {
        return None;
    }
    let has_dot_segment = |path: &str| {
        path.split('/')
            .any(|segment| segment == "." || segment == "..")
    };
    if let Some((_, rest)) = raw.split_once("://") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (raw_authority, tail) = rest.split_at(end);
        let raw_path = tail.split(['?', '#']).next().unwrap_or("");
        if has_dot_segment(raw_path) {
            return None;
        }
        let parsed = url::Url::parse(raw).ok()?;
        let host = parsed.host_str()?;
        let mut authority = String::new();
        if !parsed.username().is_empty() {
            authority.push_str(parsed.username());
            if let Some(password) = parsed.password() {
                authority.push(':');
                authority.push_str(password);
            }
            authority.push('@');
        }
        authority.push_str(host);
        if let Some(port) = parsed.port() {
            authority.push(':');
            authority.push_str(&port.to_string());
        }
        if !authority.eq_ignore_ascii_case(raw_authority) {
            return None;
        }
        return Some(nestweaver_schema::repo_url::normalized_repo_key(
            parsed.as_str(),
        ));
    }
    // scp-like `user@host:path`.
    let (before, path) = raw.split_once(':')?;
    let (user, host) = before.split_once('@')?;
    if before.contains('/')
        || user.is_empty()
        || host.is_empty()
        || host.contains('@')
        || has_dot_segment(path)
    {
        return None;
    }
    Some(nestweaver_schema::repo_url::normalized_repo_key(raw))
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

/// Verify the raw HMAC-SHA256 signature from Gitea's `X-Gitea-Signature` header.
fn verify_gitea_signature(body: &[u8], sig_header: Option<&str>, config: &WebhookConfig) -> bool {
    let Some(sig_hex) = sig_header else {
        return false;
    };

    if verify_hmac(body, sig_hex, &config.secret) {
        return true;
    }

    if let Some(ref old) = config.secret_old
        && verify_hmac(body, sig_hex, old)
    {
        tracing::warn!("Gitea webhook matched old secret — rotate to new secret");
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
/// - Gitea: `repository.clone_url`
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

    /// Compute a valid Gitea HMAC-SHA256 signature for the given body and secret.
    fn sign_gitea(body: &[u8], secret: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
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
    fn verify_valid_gitea_signature() {
        let secret = "test-secret-123";
        let body = br#"{"repository":{"clone_url":"https://gitea.example.com/acme/api.git"}}"#;
        let sig = sign_gitea(body, secret);
        let config = WebhookConfig {
            secret: secret.to_string(),
            secret_old: None,
        };
        assert!(verify_gitea_signature(body, Some(&sig), &config));
    }

    #[test]
    fn verify_invalid_gitea_signature() {
        let config = WebhookConfig {
            secret: "correct-secret".to_string(),
            secret_old: None,
        };
        let body = b"payload";
        let wrong_sig = sign_gitea(body, "wrong-secret");
        assert!(!verify_gitea_signature(body, Some(&wrong_sig), &config));
    }

    #[test]
    fn verify_gitea_dual_secret_old_matches() {
        let body = b"some payload";
        let old_secret = "old-secret";
        let new_secret = "new-secret";
        // Sign with the old secret — rotation should still verify it.
        let sig = sign_gitea(body, old_secret);
        let config = WebhookConfig {
            secret: new_secret.to_string(),
            secret_old: Some(old_secret.to_string()),
        };
        assert!(verify_gitea_signature(body, Some(&sig), &config));

        // Without the old secret configured, the same signature is rejected.
        let config_no_old = WebhookConfig {
            secret: new_secret.to_string(),
            secret_old: None,
        };
        assert!(!verify_gitea_signature(body, Some(&sig), &config_no_old));
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
    fn extract_gitea_repo_url() {
        let payload = serde_json::json!({
            "repository": {
                "html_url": "https://gitea.example.com/acme/api-service",
                "ssh_url": "git@gitea.example.com:acme/api-service.git",
                "clone_url": "https://gitea.example.com/acme/api-service.git"
            }
        });
        assert_eq!(
            extract_repo_url(&payload).as_deref(),
            Some("https://gitea.example.com/acme/api-service.git")
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

    #[tokio::test]
    async fn webhook_rejects_internal_repo_url() {
        use axum::response::IntoResponse;

        let secret = "wh-secret";
        let tmp = tempfile::tempdir().expect("tempdir");
        let queue = JobQueue::open(&tmp.path().join("jobs.sqlite")).expect("open job queue");
        let state = Arc::new(WebhookState {
            config: WebhookConfig {
                secret: secret.to_string(),
                secret_old: None,
            },
            job_queue: Arc::new(Mutex::new(queue)),
            allowed_repos: Arc::new(RwLock::new(None)),
            repo_branches: Arc::new(RwLock::new(HashMap::new())),
            repo_urls: Arc::new(RwLock::new(HashMap::new())),
        });

        // A validly-signed payload whose clone_url targets the cloud metadata IP
        // must be rejected with 400 before it can be enqueued.
        let body = br#"{"repository":{"clone_url":"http://169.254.169.254/latest/meta-data"}}"#;
        let sig = sign(body, secret);
        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature-256", sig.parse().unwrap());

        let resp = handle_webhook(State(state), headers, Bytes::from_static(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `configured` are RAW configured URLs; allowed/urls/branches are keyed by
    /// canonical id exactly as the daemon builds them.
    fn state_with_configured(
        secret: &str,
        configured: &[(&str, Option<&str>)],
    ) -> (Arc<WebhookState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let queue = JobQueue::open(&tmp.path().join("jobs.sqlite")).expect("open job queue");
        let id = |u: &str| nestweaver_engine::jobs::canonical_repo_id(u);
        let state = Arc::new(WebhookState {
            config: WebhookConfig {
                secret: secret.to_string(),
                secret_old: None,
            },
            job_queue: Arc::new(Mutex::new(queue)),
            allowed_repos: Arc::new(RwLock::new(Some(
                configured.iter().map(|(u, _)| id(u)).collect(),
            ))),
            repo_branches: Arc::new(RwLock::new(
                configured
                    .iter()
                    .filter_map(|(u, b)| b.map(|b| (id(u), b.to_string())))
                    .collect(),
            )),
            repo_urls: Arc::new(RwLock::new(
                configured
                    .iter()
                    .map(|(u, _)| (id(u), u.to_string()))
                    .collect(),
            )),
        });
        (state, tmp)
    }

    async fn post_clone_url(state: &Arc<WebhookState>, secret: &str, clone_url: &str) -> String {
        use axum::response::IntoResponse;
        let body = serde_json::json!({"repository": {"clone_url": clone_url}}).to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            sign(body.as_bytes(), secret).parse().unwrap(),
        );
        let resp = handle_webhook(State(state.clone()), headers, Bytes::from(body))
            .await
            .into_response();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        // 400 is the SSRF gate rejecting the payload: still "not accepted".
        assert!(
            status == StatusCode::OK || status == StatusCode::BAD_REQUEST,
            "{clone_url}: {status} {text}"
        );
        text
    }

    /// nw-909: a forge's clone URL in any ssh/scp/https spelling must reach the
    /// configured repo, and the job must carry the RAW configured URL (here
    /// with its `.git` suffix) and the configured branch.
    #[tokio::test]
    async fn webhook_matches_equivalent_spellings_and_enqueues_configured_url() {
        let configured = "ssh://git@github.com/Org/repo.git";
        let (state, _tmp) = state_with_configured("s", &[(configured, Some("release"))]);
        for payload in [
            "https://github.com/Org/repo.git",
            "https://GitHub.com/Org/repo.git/",
            "https://github.com/org/repo",
            "ssh://git@github.com/Org/repo",
            "https://u:p@github.com/Org/repo",
        ] {
            assert_eq!(
                post_clone_url(&state, "s", payload).await,
                "accepted",
                "{payload}"
            );
        }
        let queue = state.job_queue.lock().unwrap();
        let id = nestweaver_engine::jobs::canonical_repo_id(configured);
        let job = queue
            .job_for_repo(&id)
            .unwrap()
            .expect("job keyed by configured id");
        assert_eq!(
            job.repo_url, configured,
            "raw configured URL, not payload/stripped id"
        );
        assert_eq!(job.branch.as_deref(), Some("release"));
        assert_eq!(queue.queue_depth().unwrap().pending, 1, "one deduped job");
    }

    /// scp-like `user@host:path` never survives the SSRF gate (it is not a
    /// URL), but the matcher still handles it, and only in its strict form.
    #[test]
    fn identity_key_handles_scp_form_strictly() {
        let https = webhook_identity_key("https://github.com/Org/repo.git/").unwrap();
        assert_eq!(
            webhook_identity_key("git@github.com:Org/repo.git"),
            Some(https.clone())
        );
        assert_eq!(
            webhook_identity_key("ssh://git@github.com/Org/repo"),
            Some(https.clone())
        );
        assert_eq!(
            webhook_identity_key("https://GITHUB.com/org/repo"),
            Some(https)
        );
        for bad in [
            "git@github.com/x:Org/repo",
            "@github.com:Org/repo",
            "git@:Org/repo",
            "git@github.com:Org/../repo",
            "github.com:Org/repo",
            "/srv/git/repo",
        ] {
            assert_eq!(webhook_identity_key(bad), None, "{bad}");
        }
    }

    /// An https-configured repo is matched by scp/ssh/https payloads too.
    #[tokio::test]
    async fn webhook_matches_https_configured_repo_from_scp_and_ssh() {
        let (state, _tmp) = state_with_configured("s", &[("https://github.com/Org/repo", None)]);
        for payload in [
            "ssh://git@github.com/Org/repo",
            "https://github.com/Org/repo.git/",
        ] {
            assert_eq!(
                post_clone_url(&state, "s", payload).await,
                "accepted",
                "{payload}"
            );
        }
    }

    /// nw-909 counterweight: identity matching must not widen the allowlist or
    /// be fooled by URL-parser differentials. Every payload here must be
    /// ignored (or rejected) and enqueue nothing.
    #[tokio::test]
    async fn webhook_identity_match_fails_closed() {
        let (state, _tmp) = state_with_configured("s", &[("https://github.com/Org/repo", None)]);
        for url in [
            // different repos / hosts
            "https://github.com/Org/other.git",
            "https://gitlab.com/Org/repo.git",
            "https://github.com/evil/repo.git",
            // parser differentials
            "https://evil.example\\@github.com/Org/repo",
            "https://evil.example%5c@github.com/Org/repo",
            "https://evil.example%5C@github.com/Org/repo",
            "https://github.com/Org/repo/../other",
            "https://github.com/Org/repo/./",
            "https://github.com/Org/%2e%2e/repo",
            "https://github.com/Org/%72epo",
            "https://github.com@evil.example/Org/repo",
            // host oddities fail closed
            "https://github.com./Org/repo",
            "https://g\u{ef}thub.com/Org/repo",
            "https://github.com:8443/Org/repo",
            // scp oddities
            "git@github.com/x:Org/repo",
            "@github.com:Org/repo",
        ] {
            let out = post_clone_url(&state, "s", url).await;
            assert_ne!(out, "accepted", "{url}");
        }
        // A non-default port is part of identity: an ssh repo on :2222 is a
        // different identity from the https one.
        assert_ne!(
            post_clone_url(&state, "s", "ssh://git@github.com:2222/Org/repo").await,
            "accepted"
        );
        assert_eq!(
            state
                .job_queue
                .lock()
                .unwrap()
                .queue_depth()
                .unwrap()
                .pending,
            0
        );
    }

    /// A configured ssh repo on a non-default port does NOT match the https
    /// clone URL (documented limit), and an IDN host never matches ASCII.
    #[tokio::test]
    async fn webhook_non_default_port_is_part_of_identity() {
        let (state, _tmp) =
            state_with_configured("s", &[("ssh://git@host.example:2222/org/repo", None)]);
        assert_ne!(
            post_clone_url(&state, "s", "https://host.example/org/repo.git").await,
            "accepted"
        );
        assert_eq!(
            post_clone_url(&state, "s", "ssh://git@host.example:2222/org/repo").await,
            "accepted"
        );
    }
}

//! `nestweaver connect` — validate and register an upstream server.

use anyhow::{Context, Result};
use tonic::transport::Channel;

use crate::discovery::{RoutingMode, UpstreamConfig, save_upstream};

// ── Device-flow authentication (OAuth 2.0 Device Grant, RFC 8628) ──────

/// Default seconds between token polls when the server omits `interval`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Default grant lifetime when the server omits `expires_in`.
const DEFAULT_EXPIRES_IN_SECS: u64 = 600;

fn default_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

fn default_expires_in() -> u64 {
    DEFAULT_EXPIRES_IN_SECS
}

/// Response from `POST /auth/device` (RFC 8628 §3.2).
#[derive(Debug, serde::Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

/// Derive the HTTP(S) base URL for the device-flow auth endpoints from a gRPC
/// connect URL.
///
/// The MCP/admin HTTP listener runs on the gRPC port + 1, using `http` for
/// plaintext gRPC (`grpc://`, `http://`, or no scheme) and `https` for TLS
/// gRPC (`grpcs://`, `https://`). An explicit port is required so we can derive
/// the HTTP port.
fn device_http_base(url: &str) -> Result<String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("grpcs://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else if let Some(r) = url.strip_prefix("grpc://") {
        (false, r)
    } else {
        (false, url)
    };

    // We only need the authority — drop any path/query.
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let (host, grpc_port) = authority
        .rsplit_once(':')
        .context("device flow requires an explicit port in the server URL")?;
    let grpc_port: u16 = grpc_port
        .parse()
        .with_context(|| format!("invalid port in server URL: {grpc_port}"))?;
    let http_port = grpc_port
        .checked_add(1)
        .context("server port too high to derive the HTTP port")?;

    let scheme = if tls { "https" } else { "http" };
    Ok(format!("{scheme}://{host}:{http_port}"))
}

/// Build an HTTP client, trusting an optional self-signed CA (matching the
/// `--ca-cert` used for the gRPC connection).
fn build_http_client(ca_cert: Option<&std::path::Path>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(ca_path) = ca_cert {
        let pem = std::fs::read(ca_path)
            .with_context(|| format!("failed to read CA cert: {}", ca_path.display()))?;
        let cert = reqwest::Certificate::from_pem(&pem).context("invalid CA certificate PEM")?;
        builder = builder.add_root_certificate(cert);
    }
    builder.build().context("failed to build HTTP client")
}

/// Run the device-authorization grant against the server's `/auth` endpoints
/// and return the granted access token.
///
/// Prints the user code and opens the verification URL in a browser (gh-style),
/// then polls `/auth/token` honoring `authorization_pending`, `slow_down`, and
/// `expired_token`. The access token is never logged.
///
/// The initial `/auth/device` request is made *before* any user-facing output,
/// so callers can cleanly fall back to a token-less connect when the endpoint
/// is unavailable (e.g. a server without auth configured).
pub async fn device_flow_authenticate(
    url: &str,
    ca_cert: Option<&std::path::Path>,
) -> Result<String> {
    let base = device_http_base(url)?;
    let http = build_http_client(ca_cert)?;

    // 1. Request a device + user code.
    let resp = http
        .post(format!("{base}/auth/device"))
        .send()
        .await
        .context("failed to reach the device authorization endpoint")?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "device authorization endpoint returned HTTP {}",
            resp.status()
        );
    }
    let auth: DeviceAuthResponse = resp
        .json()
        .await
        .context("invalid device authorization response")?;

    // 2. Prompt the developer and open the verification page.
    eprintln!();
    eprintln!("To authenticate, open this URL in your browser:");
    eprintln!("  {}", auth.verification_uri);
    eprintln!();
    eprintln!("And enter the code:  {}", auth.user_code);
    eprintln!();
    if open::that(&auth.verification_uri_complete).is_ok() {
        eprintln!(
            "(opened your browser to {})",
            auth.verification_uri_complete
        );
    }
    eprintln!("Waiting for approval...");

    // 3. Poll the token endpoint until approved, expired, or timed out.
    let mut interval = std::time::Duration::from_secs(auth.interval.max(1));
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(auth.expires_in.max(1));
    let token_url = format!("{base}/auth/token");
    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("device authorization timed out before approval — please try again");
        }
        tokio::time::sleep(interval).await;

        let resp = http
            .post(&token_url)
            .json(&serde_json::json!({ "device_code": auth.device_code }))
            .send()
            .await
            .context("failed to poll the token endpoint")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

        if status.is_success() {
            if let Some(token) = body.get("access_token").and_then(|v| v.as_str()) {
                eprintln!("Authentication successful.");
                return Ok(token.to_string());
            }
            anyhow::bail!("token endpoint returned success without an access token");
        }

        match body.get("error").and_then(|v| v.as_str()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                // RFC 8628 §3.5: back off by 5s and keep polling.
                interval += std::time::Duration::from_secs(5);
            }
            Some("expired_token") => {
                anyhow::bail!("device code expired before approval — please run connect again")
            }
            Some(other) => anyhow::bail!("device authorization failed: {other}"),
            None => anyhow::bail!("device authorization failed (HTTP {status})"),
        }
    }
}

/// Connect to an upstream server, validate with HealthCheck, save config.
///
/// Prints status to stderr so callers can pipe stdout if needed.
pub async fn connect_upstream(
    url: &str,
    token: Option<&str>,
    name: Option<&str>,
    mode: RoutingMode,
    ca_cert: Option<&std::path::Path>,
) -> Result<UpstreamConfig> {
    // Normalize URL — tonic needs an http(s) scheme.
    let grpc_url = if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with("grpcs://") {
        url.replacen("grpcs://", "https://", 1)
    } else if url.starts_with("grpc://") {
        url.replacen("grpc://", "http://", 1)
    } else {
        format!("http://{}", url)
    };

    // Connect and validate via HealthCheck.
    let mut endpoint = Channel::from_shared(grpc_url.clone()).context("invalid upstream URL")?;

    if let Some(ca_path) = ca_cert {
        let pem = std::fs::read(ca_path)
            .with_context(|| format!("failed to read CA cert: {}", ca_path.display()))?;
        let ca = tonic::transport::Certificate::from_pem(pem);
        let tls = tonic::transport::ClientTlsConfig::new().ca_certificate(ca);
        endpoint = endpoint.tls_config(tls).context("TLS config failed")?;
    }

    let channel = endpoint
        .connect()
        .await
        .context("failed to connect to upstream server")?;

    let mut client =
        nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient::new(channel)
            .max_decoding_message_size(256 * 1024 * 1024)
            .max_encoding_message_size(256 * 1024 * 1024);

    // Build HealthCheck request with optional auth.
    let mut req = tonic::Request::new(nestweaver_proto::HealthCheckRequest {});
    if let Some(t) = token {
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", t)
                .parse()
                .context("invalid token format")?,
        );
    }

    let resp = client
        .health_check(req)
        .await
        .context("server health check failed — check URL and token")?
        .into_inner();

    // Query RepoStates to show available repos.
    let mut req = tonic::Request::new(nestweaver_proto::RepoStatesRequest {});
    if let Some(t) = token
        && let Ok(val) = format!("Bearer {}", t).parse()
    {
        req.metadata_mut().insert("authorization", val);
    }
    let repo_count = client
        .repo_states(req)
        .await
        .map(|r| r.into_inner().repos.len())
        .unwrap_or(0);

    let config = UpstreamConfig {
        name: Some(name.unwrap_or("upstream").to_string()),
        url: grpc_url,
        token: token.map(|t| t.to_string()),
        repos: vec![],
        mode,
        timeout: "1s".to_string(),
        ca_cert: ca_cert.map(|p| p.display().to_string()),
    };

    let saved_path = save_upstream(&config)?;

    eprintln!("Connected to {} (v{})", url, resp.version);
    eprintln!("  {} repos indexed", repo_count);
    eprintln!("  Config saved to {}", saved_path.display());

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_http_base_derives_http_port_and_scheme() {
        // Plaintext schemes → http on grpc_port + 1.
        assert_eq!(
            device_http_base("grpc://host:9378").unwrap(),
            "http://host:9379"
        );
        assert_eq!(
            device_http_base("http://host:9378").unwrap(),
            "http://host:9379"
        );
        // No scheme defaults to plaintext.
        assert_eq!(
            device_http_base("nestweaver.acme.com:9378").unwrap(),
            "http://nestweaver.acme.com:9379"
        );
        // TLS schemes → https.
        assert_eq!(
            device_http_base("grpcs://host:9378").unwrap(),
            "https://host:9379"
        );
        assert_eq!(
            device_http_base("https://host:9378").unwrap(),
            "https://host:9379"
        );
    }

    #[test]
    fn device_http_base_ignores_trailing_path() {
        assert_eq!(
            device_http_base("grpcs://host:9378/some/path").unwrap(),
            "https://host:9379"
        );
    }

    #[test]
    fn device_http_base_requires_explicit_port() {
        assert!(device_http_base("grpc://host").is_err());
        assert!(device_http_base("host").is_err());
    }

    #[test]
    fn device_http_base_rejects_non_numeric_port() {
        assert!(device_http_base("grpc://host:abc").is_err());
    }

    #[test]
    fn device_auth_response_parses_and_defaults() {
        // Server-provided values are used as-is.
        let full: DeviceAuthResponse = serde_json::from_str(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD1234",
                "verification_uri": "http://host:9379/admin",
                "verification_uri_complete": "http://host:9379/admin?user_code=ABCD1234",
                "expires_in": 300,
                "interval": 10
            }"#,
        )
        .unwrap();
        assert_eq!(full.device_code, "dc");
        assert_eq!(full.user_code, "ABCD1234");
        assert_eq!(full.expires_in, 300);
        assert_eq!(full.interval, 10);

        // Missing expires_in / interval fall back to sane defaults.
        let partial: DeviceAuthResponse = serde_json::from_str(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD1234",
                "verification_uri": "http://host:9379/admin",
                "verification_uri_complete": "http://host:9379/admin?user_code=ABCD1234"
            }"#,
        )
        .unwrap();
        assert_eq!(partial.expires_in, DEFAULT_EXPIRES_IN_SECS);
        assert_eq!(partial.interval, DEFAULT_POLL_INTERVAL_SECS);
    }
}

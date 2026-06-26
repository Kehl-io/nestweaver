//! `nestweaver connect` — validate and register an upstream server.

use anyhow::{Context, Result};
use tonic::transport::Channel;

use crate::discovery::{RoutingMode, UpstreamConfig, save_upstream};

/// Connect to an upstream server, validate with HealthCheck, save config.
///
/// Prints status to stderr so callers can pipe stdout if needed.
pub async fn connect_upstream(
    url: &str,
    token: Option<&str>,
    name: Option<&str>,
    mode: RoutingMode,
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
    let channel = Channel::from_shared(grpc_url.clone())
        .context("invalid upstream URL")?
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
    if let Some(t) = token {
        if let Ok(val) = format!("Bearer {}", t).parse() {
            req.metadata_mut().insert("authorization", val);
        }
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
    };

    let saved_path = save_upstream(&config)?;

    eprintln!("Connected to {} (v{})", url, resp.version);
    eprintln!("  {} repos indexed", repo_count);
    eprintln!("  Config saved to {}", saved_path.display());

    Ok(config)
}

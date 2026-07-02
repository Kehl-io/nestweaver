//! ACME (Let's Encrypt) automatic TLS via [`tokio_rustls_acme`], using the
//! TLS-ALPN-01 challenge (RFC 8737). Only compiled with the `acme` feature.
//!
//! One [`AcmeState`] drives provisioning + renewal; its
//! [`ResolvesServerCertAcme`](tokio_rustls_acme::ResolvesServerCertAcme) is
//! shared by both listeners via a single rustls [`ServerConfig`]. The config
//! advertises `h2` + `http/1.1` for real traffic and `acme-tls/1` for
//! challenges — the resolver serves the challenge cert when a ClientHello
//! negotiates `acme-tls/1`, and the real cert otherwise, so a plain
//! [`tokio_rustls::TlsAcceptor`] over this config handles both.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use rustls::ServerConfig;
use tokio_rustls_acme::caches::DirCache;
use tokio_rustls_acme::{AcmeConfig, AcmeState};

/// The ACME cache uses `std::io::Error` for both cert and account cache errors.
pub type DaemonAcmeState = AcmeState<std::io::Error, std::io::Error>;

/// The RFC 8737 challenge ALPN protocol.
const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// Build the shared rustls [`ServerConfig`] (ACME cert resolver + ALPN for h2,
/// http/1.1, and the acme-tls/1 challenge) plus the [`AcmeState`] that drives
/// provisioning/renewal. STAGING by default — production is `!staging`, since
/// a mistaken production loop can hit Let's Encrypt rate-limit bans.
///
/// This does no network I/O: the `AcmeState` only contacts the ACME directory
/// once it is polled (see [`drive`]). Uses the ring provider explicitly so it
/// is unambiguous even when another dependency also compiles aws-lc-rs.
pub fn build_server_config(
    domain: &str,
    email: Option<&str>,
    staging: bool,
    cache_dir: PathBuf,
) -> anyhow::Result<(Arc<ServerConfig>, DaemonAcmeState)> {
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("create ACME cache dir {}", cache_dir.display()))?;

    // ACME's internal directory client builds a rustls ClientConfig from the
    // process-default crypto provider. With both ring and aws-lc-rs compiled
    // (aws-lc-rs arrives transitively via reqwest) there is no unambiguous
    // default, so install ring explicitly first. Idempotent — a prior install
    // (e.g. the manual --tls-cert path) makes this a harmless no-op.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut config = AcmeConfig::new([domain])
        .cache(DirCache::new(cache_dir))
        .directory_lets_encrypt(!staging);
    if let Some(email) = email {
        config = config.contact_push(format!("mailto:{email}"));
    }
    let state = config.state();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configure rustls protocol versions for ACME")?
        .with_no_client_auth()
        .with_cert_resolver(state.resolver());
    server_config.alpn_protocols =
        vec![b"h2".to_vec(), b"http/1.1".to_vec(), ACME_TLS_ALPN.to_vec()];

    Ok((Arc::new(server_config), state))
}

/// Drive the ACME state machine forever: initial provisioning, then renewal
/// well before expiry. Errors are logged and the state machine retries with
/// backoff — a transient ACME failure must NEVER crash the daemon (launchd
/// `KeepAlive` would respawn it into a Let's Encrypt failed-validation ban).
pub async fn drive(mut state: DaemonAcmeState) {
    use futures::StreamExt;
    loop {
        match state.next().await {
            Some(Ok(ok)) => tracing::info!("ACME event: {ok:?}"),
            Some(Err(err)) => tracing::error!("ACME error (state machine will retry): {err:?}"),
            None => {
                tracing::error!("ACME state stream ended; certificate renewal has stopped");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_advertises_challenge_and_real_alpn() {
        let dir = tempfile::tempdir().unwrap();
        // No network: state is not polled here.
        let (cfg, _state) = build_server_config(
            "example.com",
            Some("admin@example.com"),
            true,
            dir.path().join("acme-cache"),
        )
        .unwrap();
        assert!(
            cfg.alpn_protocols.contains(&b"h2".to_vec()),
            "must advertise h2 for gRPC"
        );
        assert!(
            cfg.alpn_protocols.contains(&b"http/1.1".to_vec()),
            "must advertise http/1.1 for MCP HTTP"
        );
        assert!(
            cfg.alpn_protocols.contains(&b"acme-tls/1".to_vec()),
            "must advertise the TLS-ALPN-01 challenge protocol"
        );
    }

    #[test]
    fn build_creates_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("nested").join("acme");
        let _ = build_server_config("example.com", None, true, cache.clone()).unwrap();
        assert!(cache.is_dir(), "ACME cache dir must be created");
    }
}

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
use async_trait::async_trait;
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use tokio_rustls_acme::{AccountCache, AcmeConfig, AcmeState, CertCache};

/// The ACME cache uses `std::io::Error` for both cert and account cache errors.
pub type DaemonAcmeState = AcmeState<std::io::Error, std::io::Error>;

/// The RFC 8737 challenge ALPN protocol.
const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// Directory-backed ACME cache with ATOMIC, private key writes (B4).
///
/// The upstream [`DirCache`](tokio_rustls_acme::caches::DirCache) writes cached
/// account/cert keys with a plain `fs::write`: a crash mid-write can leave a
/// truncated (corrupt) key, and the file inherits the process umask, so it may
/// be world-readable. This cache instead writes to a unique temp file created
/// with mode 0600, fsyncs it, then atomically renames it into place — so a
/// reader never sees a partial key and the key is never group/other-readable.
///
/// Filenames are namespaced (`cached_cert_` / `cached_account_`) and hashed over
/// the domains/contact + directory URL so distinct identities never collide.
pub struct AtomicDirCache {
    dir: PathBuf,
}

impl AtomicDirCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn cert_file_name(domains: &[String], directory_url: &str) -> String {
        Self::hashed_name("cached_cert", domains, directory_url)
    }

    fn account_file_name(contact: &[String], directory_url: &str) -> String {
        Self::hashed_name("cached_account", contact, directory_url)
    }

    fn hashed_name(prefix: &str, parts: &[String], directory_url: &str) -> String {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
        hasher.update(directory_url.as_bytes());
        format!("{prefix}_{}", hex::encode(hasher.finalize()))
    }

    async fn read_if_exists(&self, name: &str) -> std::io::Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.dir.join(name)).await {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Write `data` to `name` atomically with 0600 permissions: temp file
    /// (mode 0600) → write → fsync → rename. Runs the blocking file I/O on a
    /// blocking thread so the async runtime is never stalled.
    async fn write_atomic(&self, name: String, data: Vec<u8>) -> std::io::Result<()> {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            std::fs::create_dir_all(&dir)?;
            let final_path = dir.join(&name);
            // Unique temp name (pid-scoped) so concurrent writers don't clash.
            let tmp_path = dir.join(format!(".{name}.{}.tmp", std::process::id()));

            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp_path)?;
            file.write_all(&data)?;
            file.sync_all()?;
            drop(file);

            // Belt-and-suspenders: enforce 0600 even if the file pre-existed with
            // looser perms (create is racy across runs).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
            }

            match std::fs::rename(&tmp_path, &final_path) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp_path);
                    Err(e)
                }
            }
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

#[async_trait]
impl CertCache for AtomicDirCache {
    type EC = std::io::Error;

    async fn load_cert(
        &self,
        domains: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EC> {
        self.read_if_exists(&Self::cert_file_name(domains, directory_url))
            .await
    }

    async fn store_cert(
        &self,
        domains: &[String],
        directory_url: &str,
        cert: &[u8],
    ) -> Result<(), Self::EC> {
        self.write_atomic(Self::cert_file_name(domains, directory_url), cert.to_vec())
            .await
    }
}

#[async_trait]
impl AccountCache for AtomicDirCache {
    type EA = std::io::Error;

    async fn load_account(
        &self,
        contact: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EA> {
        self.read_if_exists(&Self::account_file_name(contact, directory_url))
            .await
    }

    async fn store_account(
        &self,
        contact: &[String],
        directory_url: &str,
        account: &[u8],
    ) -> Result<(), Self::EA> {
        self.write_atomic(
            Self::account_file_name(contact, directory_url),
            account.to_vec(),
        )
        .await
    }
}

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
        .cache(AtomicDirCache::new(cache_dir))
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

    #[tokio::test]
    async fn atomic_dir_cache_writes_private_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AtomicDirCache::new(dir.path().to_path_buf());
        let url = "https://acme.example/directory";

        // Store + load a cert blob.
        cache
            .store_cert(&["example.com".into()], url, b"CERTDATA")
            .await
            .unwrap();
        let loaded = cache.load_cert(&["example.com".into()], url).await.unwrap();
        assert_eq!(loaded.as_deref(), Some(&b"CERTDATA"[..]));

        // A different domain set must not collide.
        assert!(
            cache
                .load_cert(&["other.com".into()], url)
                .await
                .unwrap()
                .is_none()
        );

        // Store + load an account blob (separate namespace).
        cache
            .store_account(&["mailto:a@example.com".into()], url, b"ACCTDATA")
            .await
            .unwrap();
        let acct = cache
            .load_account(&["mailto:a@example.com".into()], url)
            .await
            .unwrap();
        assert_eq!(acct.as_deref(), Some(&b"ACCTDATA"[..]));

        // Cached key files must be private (0600) and no temp file left behind.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let cert_name = AtomicDirCache::cert_file_name(&["example.com".into()], url);
            let mode = std::fs::metadata(dir.path().join(&cert_name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "cached key must be 0600, not world-readable"
            );
        }
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftover.is_empty(),
            "no temp files should remain after atomic writes"
        );
    }

    #[tokio::test]
    async fn atomic_dir_cache_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AtomicDirCache::new(dir.path().to_path_buf());
        let url = "https://acme.example/directory";
        cache
            .store_cert(&["a.com".into()], url, b"v1")
            .await
            .unwrap();
        cache
            .store_cert(&["a.com".into()], url, b"v2-longer")
            .await
            .unwrap();
        let loaded = cache.load_cert(&["a.com".into()], url).await.unwrap();
        assert_eq!(loaded.as_deref(), Some(&b"v2-longer"[..]));
    }

    #[test]
    fn build_creates_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("nested").join("acme");
        let _ = build_server_config("example.com", None, true, cache.clone()).unwrap();
        assert!(cache.is_dir(), "ACME cache dir must be created");
    }
}

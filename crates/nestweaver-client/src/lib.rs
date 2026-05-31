//! NestWeaver daemon client — connect over Unix domain socket with auto-start.

pub mod autostart;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hyper_util::rt::TokioIo;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use tracing::{info, warn};

use nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient;

/// Client for the NestWeaver daemon.
///
/// Wraps a tonic gRPC channel connected over a Unix domain socket.
/// Use [`DaemonClient::connect`] to auto-start the daemon if needed
/// and establish a connection with version verification.
pub struct DaemonClient {
    inner: NestWeaverDaemonClient<Channel>,
}

impl DaemonClient {
    /// Connect to the daemon for the given database, auto-starting if needed.
    ///
    /// After connecting, performs a version check. If the running daemon's
    /// version doesn't match this binary's version, it stops the old daemon
    /// and restarts with the current binary.
    pub async fn connect(db_path: &Path) -> Result<Self> {
        let sock_path = autostart::ensure_daemon(db_path)?;
        let mut client = Self::connect_to_socket(&sock_path).await?;

        // Version check.
        let resp = client
            .inner
            .health_check(nestweaver_proto::HealthCheckRequest {})
            .await
            .context("health check failed")?
            .into_inner();

        let our_version = env!("CARGO_PKG_VERSION");
        if resp.version != our_version {
            warn!(
                daemon_version = %resp.version,
                client_version = %our_version,
                "version mismatch — restarting daemon"
            );

            // Stop the old daemon.
            Self::stop_old_daemon(db_path)?;

            // Re-start and reconnect.
            let sock_path = autostart::ensure_daemon(db_path)?;
            client = Self::connect_to_socket(&sock_path).await?;

            info!("reconnected after daemon restart");
        }

        Ok(client)
    }

    /// Connect to an existing socket without auto-start or version check.
    async fn connect_to_socket(sock_path: &PathBuf) -> Result<Self> {
        let path = sock_path.clone();
        let channel = Endpoint::try_from("http://[::]:50051")
            .context("failed to create endpoint")?
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .with_context(|| {
                format!("failed to connect to daemon at {}", sock_path.display())
            })?;

        Ok(Self {
            inner: NestWeaverDaemonClient::new(channel)
                .max_decoding_message_size(64 * 1024 * 1024)
                .max_encoding_message_size(64 * 1024 * 1024),
        })
    }

    /// Stop an old daemon by sending SIGTERM to its PID.
    fn stop_old_daemon(db_path: &Path) -> Result<()> {
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
        let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);

        if let Some(pid) = autostart::read_pid(&pidfile) {
            if autostart::is_process_alive(pid) {
                info!(pid, "sending SIGTERM to old daemon");
                unsafe { libc::kill(pid, libc::SIGTERM) };

                // Wait up to 1s for it to exit.
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_secs(1) {
                    if !autostart::is_process_alive(pid) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                if autostart::is_process_alive(pid) {
                    warn!(pid, "daemon did not exit after SIGTERM, continuing anyway");
                }
            }
        }

        // Clean up stale socket.
        let sock = nestweaver_daemon::lifecycle::socket_path(&instance_id);
        if sock.exists() {
            let _ = std::fs::remove_file(&sock);
        }

        Ok(())
    }

    /// Returns a mutable reference to the underlying gRPC client.
    pub fn inner_mut(&mut self) -> &mut NestWeaverDaemonClient<Channel> {
        &mut self.inner
    }

    /// Returns a reference to the underlying gRPC client.
    pub fn inner(&self) -> &NestWeaverDaemonClient<Channel> {
        &self.inner
    }

    /// Consumes self and returns the underlying gRPC client.
    pub fn into_inner(self) -> NestWeaverDaemonClient<Channel> {
        self.inner
    }
}

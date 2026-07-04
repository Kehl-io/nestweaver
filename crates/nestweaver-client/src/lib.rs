//! NestWeaver daemon client — connect over Unix domain socket with auto-start.

pub mod autostart;
pub mod connect;
pub mod dedup;
pub mod discovery;
pub mod hybrid;
pub mod merge;
pub mod repo_identity;
pub mod routing;
pub mod upstream;

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
    /// version doesn't match this binary's version, it asks the daemon to
    /// gracefully drain active writes and shut down, then restarts.
    pub async fn connect(db_path: &Path, config_path: Option<&Path>) -> Result<Self> {
        let sock_path = autostart::ensure_daemon(db_path, config_path)?;
        let mut client = Self::connect_to_socket(&sock_path).await?;

        // Version check. Bounded so a connected-but-unresponsive daemon can't hang connect().
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client
                .inner
                .health_check(nestweaver_proto::HealthCheckRequest {}),
        )
        .await
        .context("health check timed out — daemon connected but unresponsive")?
        .context("health check failed")?
        .into_inner();

        let our_version = env!("CARGO_PKG_VERSION");
        if resp.version != our_version {
            warn!(
                daemon_version = %resp.version,
                client_version = %our_version,
                "version mismatch — requesting graceful daemon restart"
            );

            // Ask the daemon to drain active writes and shut down.
            let _ = client
                .inner
                .shutdown(nestweaver_proto::ShutdownRequest {})
                .await;

            // Wait for the daemon process to exit.
            Self::wait_for_exit(db_path).await?;

            // Re-start and reconnect.
            let sock_path = autostart::ensure_daemon(db_path, config_path)?;
            client = Self::connect_to_socket(&sock_path).await?;

            info!("reconnected after daemon restart");
        }

        Ok(client)
    }

    /// Connect to an already-running daemon for this database without
    /// auto-starting a new process.
    pub async fn connect_existing(db_path: &Path) -> Result<Self> {
        let canonical_db = std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(&canonical_db);
        let sock_path = nestweaver_daemon::lifecycle::socket_path(&instance_id);
        if !sock_path.exists() {
            anyhow::bail!("daemon socket not found at {}", sock_path.display());
        }
        Self::connect_to_socket(&sock_path).await
    }

    /// Connect to an existing socket without auto-start or version check.
    #[allow(clippy::ptr_arg)]
    async fn connect_to_socket(sock_path: &PathBuf) -> Result<Self> {
        let path = sock_path.clone();
        let channel = Endpoint::try_from("http://[::]:50051")
            .context("failed to create endpoint")?
            // Bound connection establishment so a wedged/half-open socket can't hang the
            // client forever. Per-RPC timeouts are applied by callers (query paths) rather
            // than here, so long-running RPCs (index/embed/backup) aren't capped.
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .with_context(|| format!("failed to connect to daemon at {}", sock_path.display()))?;

        Ok(Self {
            inner: NestWeaverDaemonClient::new(channel)
                .max_decoding_message_size(256 * 1024 * 1024)
                .max_encoding_message_size(256 * 1024 * 1024),
        })
    }

    /// Wait for the daemon to exit after a Shutdown RPC was sent.
    /// Falls back to SIGKILL after the drain ceiling + buffer. Async so the drain wait
    /// (up to the drain ceiling) yields the tokio worker instead of blocking it.
    async fn wait_for_exit(db_path: &Path) -> Result<()> {
        let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
        let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);

        let ceiling = std::env::var("NESTWEAVER_DRAIN_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(660);

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(ceiling + 5);

        while start.elapsed() < timeout {
            match autostart::read_pid(&pidfile) {
                Some(pid) if autostart::is_process_alive(pid) => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                _ => break,
            }
        }

        // If still alive after ceiling + buffer, force kill.
        if let Some(pid) = autostart::read_pid(&pidfile)
            && autostart::is_process_alive(pid)
        {
            warn!(
                pid,
                ceiling, "daemon did not exit after drain timeout — sending SIGKILL"
            );
            unsafe { libc::kill(pid, libc::SIGKILL) };
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        // Clean up socket.
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

    /// Materialize projects from config, streaming progress.
    pub async fn materialize_projects(
        &mut self,
        config_path: &str,
        instance_id: &str,
    ) -> Result<tonic::Streaming<nestweaver_proto::IndexProgress>> {
        let resp = self
            .inner
            .materialize_projects(nestweaver_proto::MaterializeProjectsRequest {
                config_path: config_path.to_string(),
                instance_id: instance_id.to_string(),
            })
            .await
            .context("materialize_projects RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Remove a vault and all its notes, headings, sections, tags.
    pub async fn remove_vault(
        &mut self,
        vault_uid: &str,
    ) -> Result<nestweaver_proto::RemoveVaultResponse> {
        let resp = self
            .inner
            .remove_vault(nestweaver_proto::RemoveVaultRequest {
                vault_uid: vault_uid.to_string(),
            })
            .await
            .context("remove_vault RPC failed")?;
        Ok(resp.into_inner())
    }

    pub async fn remove_repo(
        &mut self,
        repo_uid: &str,
    ) -> Result<nestweaver_proto::RemoveRepoResponse> {
        let resp = self
            .inner
            .remove_repo(nestweaver_proto::RemoveRepoRequest {
                repo_uid: repo_uid.to_string(),
            })
            .await
            .context("remove_repo RPC failed")?;
        Ok(resp.into_inner())
    }

    pub async fn remove_project(
        &mut self,
        project_uid: &str,
    ) -> Result<nestweaver_proto::RemoveProjectResponse> {
        let resp = self
            .inner
            .remove_project(nestweaver_proto::RemoveProjectRequest {
                project_uid: project_uid.to_string(),
            })
            .await
            .context("remove_project RPC failed")?;
        Ok(resp.into_inner())
    }

    pub async fn prune_stale(&mut self) -> Result<nestweaver_proto::PruneStaleResponse> {
        let resp = self
            .inner
            .prune_stale(nestweaver_proto::PruneStaleRequest {})
            .await
            .context("prune_stale RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Merge one instance's data into another.
    pub async fn merge_instance(
        &mut self,
        from_id: &str,
        to_id: &str,
    ) -> Result<nestweaver_proto::MergeInstanceResponse> {
        let resp = self
            .inner
            .merge_instance(nestweaver_proto::MergeInstanceRequest {
                from_id: from_id.to_string(),
                to_id: to_id.to_string(),
            })
            .await
            .context("merge_instance RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Tell the daemon to serve the web UI on the given port.
    pub async fn serve_ui(
        &mut self,
        port: u16,
        open_browser: bool,
        watch: bool,
        watch_repo_path: &str,
        watch_instance_id: &str,
    ) -> Result<nestweaver_proto::ServeUiResponse> {
        let resp = self
            .inner
            .serve_ui(nestweaver_proto::ServeUiRequest {
                port: port as u32,
                open_browser,
                watch,
                watch_repo_path: watch_repo_path.to_string(),
                watch_instance_id: watch_instance_id.to_string(),
            })
            .await
            .context("serve_ui RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Tell the daemon to start a code watcher for a repository.
    pub async fn watch_code(
        &mut self,
        repo_path: &str,
        instance_id: &str,
    ) -> Result<nestweaver_proto::WatchCodeResponse> {
        let resp = self
            .inner
            .watch_code(nestweaver_proto::WatchCodeRequest {
                repo_path: repo_path.to_string(),
                instance_id: instance_id.to_string(),
            })
            .await
            .context("watch_code RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Run bulk embedding on the daemon using its Metal-accelerated model.
    pub async fn embed(
        &mut self,
        scope: &str,
        force: bool,
        batch_size: u32,
    ) -> Result<nestweaver_proto::EmbedResponse> {
        let resp = self
            .inner
            .embed(nestweaver_proto::EmbedRequest {
                scope: scope.to_string(),
                force,
                batch_size,
            })
            .await
            .context("embed RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Purge all data for an instance, streaming progress.
    pub async fn purge_instance(
        &mut self,
        instance_id: &str,
    ) -> Result<tonic::Streaming<nestweaver_proto::IndexProgress>> {
        let resp = self
            .inner
            .purge_instance(nestweaver_proto::PurgeInstanceRequest {
                instance_id: instance_id.to_string(),
            })
            .await
            .context("purge_instance RPC failed")?;
        Ok(resp.into_inner())
    }
}

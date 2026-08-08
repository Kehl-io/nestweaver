//! NestWeaver daemon client — connect over Unix domain socket with auto-start.

pub mod autostart;
pub mod connect;
pub mod hybrid;
pub mod repo_identity;

// These modules moved to the `nestweaver-federation` crate (nw-017 Phase B,
// 5a). Re-export them at their old paths so `nestweaver_client::discovery`,
// `::upstream`, `::routing`, `::merge`, and `::dedup` keep working for
// existing callers (main binary, e2e tests).
pub use nestweaver_federation::{dedup, discovery, merge, routing, upstream};

use std::fs;
use std::os::unix::io::AsRawFd;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartConfig {
    Configured(PathBuf),
    CompiledDefaults,
}

impl RestartConfig {
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::Configured(path) => Some(path),
            Self::CompiledDefaults => None,
        }
    }
}

#[derive(Debug)]
pub struct PreparedRestart {
    config: RestartConfig,
    daemon_pid: u32,
    /// The exact inode whose held flock and contents were cross-checked with
    /// HealthCheck during PREPARE. COMMIT never reopens or rereads the path.
    pidfile: fs::File,
    owns_pidfile_lock: bool,
}

impl PreparedRestart {
    pub fn config(&self) -> &RestartConfig {
        &self.config
    }

    pub async fn wait_for_owner_release(&mut self) -> Result<()> {
        let ceiling = nestweaver_schema::drain_ceiling_from_env();
        self.wait_for_owner_release_for(std::time::Duration::from_secs(ceiling.saturating_add(5)))
            .await
    }

    async fn wait_for_owner_release_for(&mut self, timeout: std::time::Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;

        while std::time::Instant::now() < deadline {
            if try_acquire_pidfile_lock(&self.pidfile)? {
                self.owns_pidfile_lock = true;
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        anyhow::bail!(
            "daemon PID {} did not release its captured pidfile lock within {:.1}s; refusing to signal an uncertain PID or spawn a replacement. Stop the daemon manually, verify its identity, and retry",
            self.daemon_pid,
            timeout.as_secs_f64()
        )
    }

    /// Release the old owner's now-acquired pidfile lock immediately before a
    /// guarded replacement spawn. The caller must still hold [`autostart::SpawnLock`].
    pub fn release_pidfile_lock(&mut self) {
        if self.owns_pidfile_lock {
            unsafe {
                libc::flock(self.pidfile.as_raw_fd(), libc::LOCK_UN);
            }
            self.owns_pidfile_lock = false;
        }
    }
}

impl Drop for PreparedRestart {
    fn drop(&mut self) {
        self.release_pidfile_lock();
    }
}

fn try_acquire_pidfile_lock(file: &fs::File) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error).context("check captured daemon pidfile lock")
    }
}

fn validate_restart_config(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize --config {}", path.display()))?;
    anyhow::ensure!(
        canonical.to_str().is_some(),
        "canonical --config path is not valid UTF-8: {}",
        canonical.display()
    );
    nestweaver_engine::InstanceConfig::from_file(&canonical)
        .with_context(|| format!("invalid --config {}", canonical.display()))?;
    Ok(canonical)
}

fn select_restart_config(
    explicit_config: Option<&Path>,
    binding: impl FnOnce() -> Result<
        nestweaver_daemon::lifecycle::EffectiveConfigBinding,
        nestweaver_daemon::lifecycle::EffectiveConfigBindingError,
    >,
) -> Result<RestartConfig> {
    if let Some(path) = explicit_config {
        return validate_restart_config(path).map(RestartConfig::Configured);
    }

    let binding = binding().context("read live daemon effective-config binding")?;
    match binding.effective_config {
        nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured { path } => {
            anyhow::ensure!(
                !path.is_empty(),
                "live daemon binding contains an empty config path"
            );
            let recorded = PathBuf::from(&path);
            anyhow::ensure!(
                recorded.is_absolute(),
                "live daemon binding contains a non-absolute config path: {path}"
            );
            let canonical = validate_restart_config(&recorded)?;
            anyhow::ensure!(
                canonical == recorded,
                "live daemon binding config path is not canonical: {}",
                recorded.display()
            );
            Ok(RestartConfig::Configured(canonical))
        }
        nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults => {
            Ok(RestartConfig::CompiledDefaults)
        }
    }
}

fn open_verified_live_pidfile(instance_id: &str, health_pid: u32) -> Result<fs::File> {
    let path = nestweaver_daemon::lifecycle::pidfile_path(instance_id);
    open_verified_live_pidfile_at(&path, health_pid)
}

fn open_verified_live_pidfile_at(path: &Path, health_pid: u32) -> Result<fs::File> {
    anyhow::ensure!(health_pid != 0, "daemon HealthCheck returned PID 0");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(&path)
        .with_context(|| format!("open existing daemon pidfile {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect daemon pidfile {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "daemon pidfile is not a regular file: {}",
        path.display()
    );

    if try_acquire_pidfile_lock(&file)? {
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        anyhow::bail!(
            "daemon pidfile lock is not held at {}; the HealthCheck identity is no longer live",
            path.display()
        );
    }

    use std::io::{Read, Seek};
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut contents = String::new();
    file.by_ref().take(64).read_to_string(&mut contents)?;
    let pidfile_pid = contents.trim().parse::<u32>().with_context(|| {
        format!(
            "daemon pidfile {} does not contain a valid PID",
            path.display()
        )
    })?;
    anyhow::ensure!(
        pidfile_pid == health_pid,
        "daemon pidfile PID {pidfile_pid} does not match HealthCheck PID {health_pid}"
    );
    Ok(file)
}

/// Capture trustworthy live daemon ownership and configuration before any
/// shutdown request. Callers must not shut down when this returns an error.
pub fn prepare_restart(
    db_path: &Path,
    health: &nestweaver_proto::HealthCheckResponse,
    explicit_config: Option<&Path>,
) -> Result<PreparedRestart> {
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let prepared = (|| {
        anyhow::ensure!(
            health.instance_id == instance_id,
            "daemon HealthCheck instance {} does not match requested instance {instance_id}",
            health.instance_id
        );
        let pidfile = open_verified_live_pidfile(&instance_id, health.pid)?;
        let config = select_restart_config(explicit_config, || {
            nestweaver_daemon::lifecycle::read_effective_config_binding_for_verified_pid(
                &instance_id,
                health.pid,
            )
        })?;
        Ok(PreparedRestart {
            config,
            daemon_pid: health.pid,
            pidfile,
            owns_pidfile_lock: false,
        })
    })();

    restart_prepare_result(db_path, prepared)
}

fn restart_prepare_result<T>(db_path: &Path, prepared: Result<T>) -> Result<T> {
    prepared.with_context(|| {
        format!(
            "refusing automatic daemon restart because its live configuration and ownership could not be verified. \
             The daemon has not been shut down. Re-run this command with --config <path>, or restart it manually \
             with `nestweaver daemon --db {} restart --config <path>`",
            db_path.display()
        )
    })
}

/// Transaction gate shared by production and ordering tests. Neither callback
/// runs when PREPARE fails, and COMMIT cannot run unless Shutdown was accepted.
async fn run_prepared_restart<T, Shutdown, ShutdownFuture, Commit, CommitFuture>(
    prepared: Result<PreparedRestart>,
    shutdown: Shutdown,
    commit: Commit,
) -> Result<T>
where
    Shutdown: FnOnce() -> ShutdownFuture,
    ShutdownFuture: std::future::Future<Output = Result<bool>>,
    Commit: FnOnce(PreparedRestart) -> CommitFuture,
    CommitFuture: std::future::Future<Output = Result<T>>,
{
    let prepared = prepared?;
    anyhow::ensure!(
        shutdown().await?,
        "daemon rejected shutdown; refusing to spawn a replacement"
    );
    commit(prepared).await
}

fn verify_replacement_evidence(
    expected_instance_id: &str,
    health: &nestweaver_proto::HealthCheckResponse,
    expected_config: &RestartConfig,
    binding: nestweaver_daemon::lifecycle::EffectiveConfigBinding,
) -> Result<()> {
    anyhow::ensure!(
        health.version == env!("CARGO_PKG_VERSION"),
        "replacement daemon version {} does not match client version {}",
        health.version,
        env!("CARGO_PKG_VERSION")
    );
    anyhow::ensure!(
        health.instance_id == expected_instance_id,
        "replacement daemon instance {} does not match expected instance {expected_instance_id}",
        health.instance_id
    );
    anyhow::ensure!(
        health.pid != 0,
        "replacement daemon HealthCheck returned PID 0"
    );
    anyhow::ensure!(
        binding.pid == health.pid,
        "replacement effective-config binding PID {} does not match HealthCheck PID {}",
        binding.pid,
        health.pid
    );
    let actual_config = select_restart_config(None, || Ok(binding))?;
    anyhow::ensure!(
        &actual_config == expected_config,
        "replacement daemon effective config {actual_config:?} does not match captured restart config {expected_config:?}"
    );
    Ok(())
}

/// Connect to a replacement and require current-version runtime identity,
/// pidfile ownership, and exact effective-config agreement before accepting it.
pub async fn connect_verified_replacement(
    sock_path: &PathBuf,
    db_path: &Path,
    expected_config: &RestartConfig,
) -> Result<DaemonClient> {
    let mut client = DaemonClient::connect_to_socket(sock_path).await?;
    let health = client.health_check().await?;
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    // Hold the exact verified pidfile inode open until the sidecar and all
    // post-spawn evidence have been checked.
    let _pidfile = open_verified_live_pidfile(&instance_id, health.pid)?;
    let binding = nestweaver_daemon::lifecycle::read_effective_config_binding_for_verified_pid(
        &instance_id,
        health.pid,
    )?;
    verify_replacement_evidence(&instance_id, &health, expected_config, binding).with_context(
        || {
            format!(
                "replacement daemon at {} failed identity/config verification; stop it manually before retrying",
                sock_path.display()
            )
        },
    )?;
    Ok(client)
}

impl DaemonClient {
    /// Connect to the daemon for the given database, auto-starting if needed.
    ///
    /// After connecting, performs a version check. If the running daemon's
    /// version doesn't match this binary's version, it asks the daemon to
    /// gracefully drain active writes and shut down, then restarts.
    pub async fn connect(db_path: &Path, config_path: Option<&Path>) -> Result<Self> {
        let sock_path = autostart::ensure_daemon_async(db_path, config_path).await?;
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

            // Capture the ORIGINAL daemon's trustworthy plan before waiting
            // for the transaction lock. A concurrent/manual winner must later
            // prove continuity with this plan; its own provenance can never be
            // treated as the expectation.
            let original_prepared = prepare_restart(db_path, &resp, config_path)?;
            let expected_config = original_prepared.config.clone();

            // PREPARE -> COMMIT. Retain the instance spawn lock through
            // replacement verification. If another client completed the
            // upgrade while we waited, verify it against the original plan.
            let spawn_lock = autostart::SpawnLock::acquire_async(db_path).await?;
            client = Self::connect_to_socket(&sock_path)
                .await
                .context("reconnect to daemon after acquiring restart spawn lock")?;
            let locked_health = client.health_check().await.context(
                "refusing automatic daemon restart: could not revalidate the live daemon after acquiring the spawn lock; no shutdown was requested",
            )?;
            if locked_health.version == our_version {
                let verified = connect_verified_replacement(
                    &sock_path,
                    db_path,
                    &expected_config,
                )
                .await
                .context(
                    "current-version daemon that won the restart race did not preserve the original effective config",
                )?;
                info!("another client completed and verified the daemon upgrade");
                return Ok(verified);
            }
            let prepared = prepare_restart(db_path, &locked_health, config_path).and_then(|value| {
                anyhow::ensure!(
                    value.config == expected_config,
                    "daemon effective config changed while waiting for the restart transaction lock; refusing shutdown"
                );
                Ok(value)
            });
            drop(original_prepared);
            client = run_prepared_restart(
                prepared,
                || async {
                    let shutdown = client
                        .inner
                        .shutdown(nestweaver_proto::ShutdownRequest {})
                        .await
                        .context("daemon shutdown request failed; refusing to spawn a replacement")?
                        .into_inner();
                    Ok(shutdown.ok)
                },
                |mut prepared| async move {
                    // Wait on the already-open inode whose lock and PID were
                    // verified during PREPARE. Never reread the path or signal
                    // a successor.
                    prepared.wait_for_owner_release().await?;

                    // The old owner is gone and we hold its inode lock. A
                    // stale socket cannot belong to a successor because the
                    // spawn lock has covered the whole transaction.
                    let stale_socket = nestweaver_daemon::lifecycle::socket_path(
                        &nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path),
                    );
                    let _ = fs::remove_file(stale_socket);
                    prepared.release_pidfile_lock();

                    // Re-start from the captured owned decision; the sidecar
                    // is never read again after the old daemon dies.
                    let restart_config = prepared.config.clone();
                    let (sock_path, spawn_lock) = autostart::ensure_daemon_with_spawn_lock_async(
                        db_path,
                        restart_config.as_path(),
                        spawn_lock,
                    )
                    .await?;
                    let verified =
                        connect_verified_replacement(&sock_path, db_path, &restart_config).await;
                    drop(spawn_lock);
                    verified
                },
            )
            .await?;

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

    /// Returns a mutable reference to the underlying gRPC client.
    pub fn inner_mut(&mut self) -> &mut NestWeaverDaemonClient<Channel> {
        &mut self.inner
    }

    /// Health-check the daemon, returning its version, instance, uptime —
    /// and its own OS PID. Bounded so an unresponsive daemon can't
    /// hang the caller.
    pub async fn health_check(&mut self) -> Result<nestweaver_proto::HealthCheckResponse> {
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.inner
                .health_check(nestweaver_proto::HealthCheckRequest {}),
        )
        .await
        .context("health check timed out — daemon connected but unresponsive")?
        .context("health check failed")?;
        Ok(resp.into_inner())
    }

    /// Return graph and structured embedding readiness for the running daemon.
    pub async fn brain_status(&mut self) -> Result<nestweaver_proto::BrainStatusResponse> {
        let resp = self
            .inner
            .brain_status(nestweaver_proto::BrainStatusRequest {})
            .await
            .context("brain_status RPC failed")?;
        Ok(resp.into_inner())
    }

    /// The running daemon's own PID, as reported over its socket.
    ///
    /// Cross-check this against a pidfile PID before signaling that PID: a
    /// foreign PID written into the pidfile of a LIVE daemon fails this
    /// check (the socket still reports the real daemon's PID), while a plain
    /// flock check would have trusted the file.
    pub async fn socket_reported_pid(&mut self) -> Result<u32> {
        Ok(self.health_check().await?.pid)
    }

    /// Poll until the daemon for `db_path` answers a health check or
    /// `timeout` elapses. Does NOT auto-start anything — use after
    /// triggering a start (launchd kickstart / fork) to detect an
    /// eventually-healthy daemon whose boot outlives a short fixed wait.
    pub async fn wait_healthy(
        db_path: &Path,
        timeout: std::time::Duration,
    ) -> Result<nestweaver_proto::HealthCheckResponse> {
        let start = std::time::Instant::now();
        let mut delay = std::time::Duration::from_millis(100);
        let max_delay = std::time::Duration::from_secs(1);
        loop {
            // health_check has its own 10s timeout, and the deadline below is
            // only checked BETWEEN attempts — cap each attempt at the
            // remaining budget so the loop never overshoots `timeout` by up
            // to 10s.
            let remaining = timeout.saturating_sub(start.elapsed());
            match Self::connect_existing(db_path).await {
                Ok(mut client) => {
                    match tokio::time::timeout(remaining, client.health_check()).await {
                        Ok(Ok(resp)) => return Ok(resp),
                        Ok(Err(e)) => {
                            tracing::debug!("wait_healthy: health check not yet passing: {e:#}")
                        }
                        Err(_) => {
                            tracing::debug!(
                                "wait_healthy: health check exceeded the remaining budget"
                            )
                        }
                    }
                }
                Err(e) => tracing::debug!("wait_healthy: daemon not yet connectable: {e:#}"),
            }
            if start.elapsed() >= timeout {
                anyhow::bail!(
                    "daemon for {} did not become healthy within {:.1}s",
                    db_path.display(),
                    timeout.as_secs_f64()
                );
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(max_delay);
        }
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

    /// Ask the daemon to stop serving the web UI (releases the listen port).
    pub async fn stop_ui(&mut self) -> Result<nestweaver_proto::StopUiResponse> {
        let resp = self
            .inner
            .stop_ui(nestweaver_proto::StopUiRequest {})
            .await
            .context("stop_ui RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Tell the daemon to start a code watcher for a repository.
    pub async fn watch_code(
        &mut self,
        repo_path: &str,
        instance_id: &str,
    ) -> Result<nestweaver_proto::WatchCodeResponse> {
        self.watch_code_with_force(repo_path, instance_id, false)
            .await
    }

    /// Like [`DaemonClient::watch_code`], with explicit control over
    /// replacing an already-running watcher.
    ///
    /// A `watch` CLI that is kill -9'd leaves its daemon-side watcher
    /// running, and every new watch session then fails with "already
    /// running". `force: true` stops the orphaned incumbent and adopts the
    /// new session instead.
    pub async fn watch_code_with_force(
        &mut self,
        repo_path: &str,
        instance_id: &str,
        force: bool,
    ) -> Result<nestweaver_proto::WatchCodeResponse> {
        let resp = self
            .inner
            .watch_code(nestweaver_proto::WatchCodeRequest {
                repo_path: repo_path.to_string(),
                instance_id: instance_id.to_string(),
                force,
            })
            .await
            .context("watch_code RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Ask the daemon to stop its active file watcher (if any).
    pub async fn stop_watch(&mut self) -> Result<nestweaver_proto::StopWatchResponse> {
        let resp = self
            .inner
            .stop_watch(nestweaver_proto::StopWatchRequest {})
            .await
            .context("stop_watch RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Return the authoritative embedding eligibility snapshot for a scope.
    pub async fn plan_embed(
        &mut self,
        scope: &str,
        force: bool,
    ) -> Result<nestweaver_proto::EmbedResponse> {
        let resp = self
            .inner
            .plan_embed(nestweaver_proto::EmbedRequest {
                scope: scope.to_string(),
                force,
                batch_size: 0,
            })
            .await
            .context("plan_embed RPC failed")?;
        Ok(resp.into_inner())
    }

    /// Run bulk embedding on the daemon using its configured embedding backend.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(
            &path,
            format!(
                r#"
instance_id = "restart-test"
repos = []

[snapshot_storage]
backend = "local"
path = "{}"

[workspace]
backend = "local"
path = "{}"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"
"#,
                dir.path().join("snapshots").display(),
                dir.path().join("workspace").display()
            ),
        )
        .unwrap();
        path
    }

    fn binding(
        pid: u32,
        effective_config: nestweaver_daemon::lifecycle::EffectiveConfigBindingSource,
    ) -> nestweaver_daemon::lifecycle::EffectiveConfigBinding {
        nestweaver_daemon::lifecycle::EffectiveConfigBinding::new(pid, effective_config)
    }

    #[test]
    fn explicit_config_precedes_even_absent_or_corrupt_live_binding() {
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(&dir, "explicit.toml");
        let mut binding_reads = 0;
        let selected = select_restart_config(Some(&config), || {
            binding_reads += 1;
            unreachable!("an explicit config must bypass the live binding")
        })
        .unwrap();

        assert_eq!(binding_reads, 0);
        assert_eq!(
            selected,
            RestartConfig::Configured(fs::canonicalize(config).unwrap())
        );
    }

    #[test]
    fn explicit_config_must_be_parse_valid_before_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("invalid.toml");
        fs::write(&config, "not = [valid").unwrap();
        let error = select_restart_config(Some(&config), || unreachable!()).unwrap_err();
        assert!(
            format!("{error:#}").contains("invalid --config"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn configured_and_compiled_default_bindings_become_owned_restart_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(&dir, "bound.toml");
        let canonical = fs::canonicalize(&config).unwrap();
        let configured = select_restart_config(None, || {
            Ok(binding(
                7,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                    path: canonical.to_str().unwrap().to_string(),
                },
            ))
        })
        .unwrap();
        let defaults = select_restart_config(None, || {
            Ok(binding(
                7,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
            ))
        })
        .unwrap();

        assert_eq!(configured.as_path(), Some(canonical.as_path()));
        assert_eq!(defaults.as_path(), None);
    }

    #[test]
    fn captured_restart_config_does_not_reread_binding_after_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let config = valid_config(&dir, "captured.toml");
        let canonical = fs::canonicalize(config).unwrap();
        let sidecar_marker = dir.path().join("effective-config.json");
        fs::write(&sidecar_marker, "present").unwrap();
        let selected = select_restart_config(None, || {
            assert!(sidecar_marker.exists());
            Ok(binding(
                9,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                    path: canonical.to_str().unwrap().to_string(),
                },
            ))
        })
        .unwrap();

        fs::remove_file(sidecar_marker).unwrap();
        assert_eq!(selected.as_path(), Some(canonical.as_path()));
    }

    #[test]
    fn every_untrusted_binding_error_refuses_with_manual_restart_remedy() {
        use nestweaver_daemon::lifecycle::EffectiveConfigBindingError as E;
        let path = PathBuf::from("/tmp/effective-config.json");
        let errors = vec![
            E::Absent { path: path.clone() },
            E::Read {
                path: path.clone(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
            },
            E::Corrupt {
                path: path.clone(),
                source:
                    serde_json::from_str::<nestweaver_daemon::lifecycle::EffectiveConfigBinding>(
                        "{",
                    )
                    .unwrap_err(),
            },
            E::Unsafe {
                path: path.clone(),
                reason: "unsafe owner".to_string(),
            },
            E::TooLarge {
                path: path.clone(),
                size: 65_537,
                max: 65_536,
            },
            E::UnsupportedVersion {
                path: path.clone(),
                found: 99,
                supported: 1,
            },
            E::PidMismatch {
                path: path.clone(),
                expected: 8,
                found: 7,
            },
        ];

        for binding_error in errors {
            let prepare = select_restart_config(None, || Err(binding_error));
            let mut shutdown_calls = 0;
            let refused = restart_prepare_result(Path::new("/tmp/brain.lbug"), prepare)
                .map(|_| shutdown_calls += 1)
                .unwrap_err();
            let message = format!("{refused:#}");
            assert_eq!(shutdown_calls, 0, "PREPARE failure must precede shutdown");
            assert!(
                message.contains("daemon has not been shut down"),
                "{message}"
            );
            assert!(message.contains("--config <path>"), "{message}");
        }
    }

    #[test]
    fn unknown_binding_provenance_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let error = select_restart_config(None, || {
            Ok(binding(
                3,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                    path: String::new(),
                },
            ))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("empty config path"));

        let missing = dir.path().join("old-daemon-had-no-sidecar");
        let old_daemon = restart_prepare_result::<RestartConfig>(
            Path::new("/tmp/brain.lbug"),
            Err(
                nestweaver_daemon::lifecycle::EffectiveConfigBindingError::Absent { path: missing }
                    .into(),
            ),
        )
        .unwrap_err();
        assert!(format!("{old_daemon:#}").contains("restart it manually"));
    }

    #[test]
    fn prepare_requires_held_same_inode_flock_and_matching_health_pid() {
        use std::io::{Seek, Write};

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("daemon.pid");
        let mut owner = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&pidfile)
            .unwrap();
        write!(owner, "41").unwrap();

        let free = open_verified_live_pidfile_at(&pidfile, 41).unwrap_err();
        assert!(format!("{free:#}").contains("lock is not held"));

        assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX) }, 0);
        open_verified_live_pidfile_at(&pidfile, 41)
            .expect("held same-inode flock plus matching PID is trusted");

        // Rewriting the path contents cannot redirect COMMIT to a foreign PID:
        // PREPARE reads the same locked inode it just checked.
        owner.set_len(0).unwrap();
        owner.seek(std::io::SeekFrom::Start(0)).unwrap();
        write!(owner, "42").unwrap();
        let mismatch = open_verified_live_pidfile_at(&pidfile, 41).unwrap_err();
        assert!(
            format!("{mismatch:#}").contains("does not match HealthCheck PID 41"),
            "unexpected error: {mismatch:#}"
        );
        unsafe {
            libc::flock(owner.as_raw_fd(), libc::LOCK_UN);
        }
    }

    #[test]
    fn prepare_rejects_zero_pid_and_wrong_instance_before_sidecar_read() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("daemon.pid");
        fs::write(&pidfile, "1").unwrap();
        let zero = open_verified_live_pidfile_at(&pidfile, 0).unwrap_err();
        assert!(format!("{zero:#}").contains("PID 0"));

        let db = dir.path().join("brain.lbug");
        let health = nestweaver_proto::HealthCheckResponse {
            instance_id: "wrong-instance".to_string(),
            pid: 1,
            ..Default::default()
        };
        let wrong = prepare_restart(&db, &health, None).unwrap_err();
        assert!(format!("{wrong:#}").contains("does not match requested instance"));
    }

    #[tokio::test]
    async fn transaction_gate_never_shuts_down_on_prepare_error_or_spawns_on_shutdown_failure() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shutdowns = Arc::new(AtomicUsize::new(0));
        let spawns = Arc::new(AtomicUsize::new(0));
        let shutdown_counter = Arc::clone(&shutdowns);
        let spawn_counter = Arc::clone(&spawns);
        let prepare_error = run_prepared_restart::<(), _, _, _, _>(
            Err(anyhow::anyhow!("prepare failed")),
            move || async move {
                shutdown_counter.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
            move |_| async move {
                spawn_counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{prepare_error:#}").contains("prepare failed"));
        assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
        assert_eq!(spawns.load(Ordering::SeqCst), 0);

        let file = tempfile::tempfile().unwrap();
        let prepared = PreparedRestart {
            config: RestartConfig::CompiledDefaults,
            daemon_pid: std::process::id(),
            pidfile: file,
            owns_pidfile_lock: false,
        };
        let spawns = Arc::new(AtomicUsize::new(0));
        let spawn_counter = Arc::clone(&spawns);
        let shutdown_error = run_prepared_restart::<(), _, _, _, _>(
            Ok(prepared),
            || async { Ok(false) },
            move |_| async move {
                spawn_counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{shutdown_error:#}").contains("refusing to spawn"));
        assert_eq!(spawns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn owner_release_timeout_never_signals_or_spawns() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("daemon.pid");
        let owner = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&pidfile)
            .unwrap();
        assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX) }, 0);
        let observed = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pidfile)
            .unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let prepared = PreparedRestart {
            config: RestartConfig::CompiledDefaults,
            daemon_pid: child.id(),
            pidfile: observed,
            owns_pidfile_lock: false,
        };
        let spawns = Arc::new(AtomicUsize::new(0));
        let spawn_counter = Arc::clone(&spawns);
        let error = run_prepared_restart::<(), _, _, _, _>(
            Ok(prepared),
            || async { Ok(true) },
            move |mut prepared| async move {
                prepared
                    .wait_for_owner_release_for(std::time::Duration::from_millis(40))
                    .await?;
                spawn_counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("refusing to signal"));
        assert_eq!(spawns.load(Ordering::SeqCst), 0);
        assert!(
            child.try_wait().unwrap().is_none(),
            "timeout must not signal the uncertain captured numeric PID"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        unsafe {
            libc::flock(owner.as_raw_fd(), libc::LOCK_UN);
        }
    }

    #[test]
    fn replacement_evidence_accepts_configured_and_compiled_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = fs::canonicalize(valid_config(&dir, "replacement.toml")).unwrap();
        let configured = RestartConfig::Configured(config.clone());
        let health = nestweaver_proto::HealthCheckResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            instance_id: "expected".to_string(),
            pid: 71,
            ..Default::default()
        };
        verify_replacement_evidence(
            "expected",
            &health,
            &configured,
            binding(
                71,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                    path: config.to_str().unwrap().to_string(),
                },
            ),
        )
        .unwrap();
        verify_replacement_evidence(
            "expected",
            &health,
            &RestartConfig::CompiledDefaults,
            binding(
                71,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
            ),
        )
        .unwrap();
    }

    #[test]
    fn replacement_evidence_rejects_config_version_instance_and_pid_mismatches() {
        let valid_health = nestweaver_proto::HealthCheckResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            instance_id: "expected".to_string(),
            pid: 81,
            ..Default::default()
        };
        let actual_defaults = || {
            binding(
                81,
                nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
            )
        };

        let config_mismatch = verify_replacement_evidence(
            "expected",
            &valid_health,
            &RestartConfig::Configured(PathBuf::from("/expected/config.toml")),
            actual_defaults(),
        )
        .unwrap_err();
        assert!(format!("{config_mismatch:#}").contains("does not match captured"));

        let wrong_version = nestweaver_proto::HealthCheckResponse {
            version: "0.0.0-old".to_string(),
            ..valid_health.clone()
        };
        assert!(
            format!(
                "{:#}",
                verify_replacement_evidence(
                    "expected",
                    &wrong_version,
                    &RestartConfig::CompiledDefaults,
                    actual_defaults(),
                )
                .unwrap_err()
            )
            .contains("does not match client version")
        );

        let wrong_instance = nestweaver_proto::HealthCheckResponse {
            instance_id: "unexpected".to_string(),
            ..valid_health.clone()
        };
        assert!(
            format!(
                "{:#}",
                verify_replacement_evidence(
                    "expected",
                    &wrong_instance,
                    &RestartConfig::CompiledDefaults,
                    actual_defaults(),
                )
                .unwrap_err()
            )
            .contains("does not match expected instance")
        );

        let wrong_pid = binding(
            82,
            nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
        );
        assert!(
            format!(
                "{:#}",
                verify_replacement_evidence(
                    "expected",
                    &valid_health,
                    &RestartConfig::CompiledDefaults,
                    wrong_pid,
                )
                .unwrap_err()
            )
            .contains("does not match HealthCheck PID")
        );
    }

    /// `wait_healthy` must keep polling until the timeout and then
    /// report not-healthy (rather than declaring failure after a short fixed
    /// wait while the daemon is still booting, e.g. under launchd).
    #[tokio::test]
    async fn wait_healthy_times_out_when_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("no-such-daemon.lbug");
        let start = std::time::Instant::now();
        let err = DaemonClient::wait_healthy(&db_path, std::time::Duration::from_millis(400))
            .await
            .expect_err("no daemon running — must time out");
        assert!(
            err.to_string().contains("did not become healthy"),
            "unexpected error: {err:#}"
        );
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(400),
            "must poll for the full timeout, not fail on the first attempt"
        );
    }
}
